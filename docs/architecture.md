# 架构

## 目标

Tomato Router 是运行在 Windows 本机的单进程 HTTP 网关。它接受 Codex 的 Responses API 请求，按配置的严格优先级选择上游 provider，并在不破坏流式语义的前提下完成故障转移。

设计约束：

- 对外协议固定为 Responses API。
- 上游可以是原生 Responses，也可以是 Chat Completions。
- 路由是严格单调的：只允许从高优先级向低优先级走。
- 网关无状态：不存储 response，不依赖 `previous_response_id`。
- 一旦向客户端提交 SSE 事件，就不再尝试其他 provider。

## 组件

```text
Codex (Windows/WSL)
        |
        | POST /v1/responses
        v
  Axum HTTP server
        |
        | auth + body limit + JSON parse
        v
  gateway::execute
        |
        | ModelRouter::chain_for(model)
        v
  [provider A, provider B, ...]   strict priority
        |
        +--> provider.begin_attempt()       breaker check
        +--> provider.acquire()             semaphore + queue
        +--> upstream::responses::start()   native passthrough
        |    or upstream::chat::start()     Chat adapter
        |
        v
  first SSE event peek
        |
        +-- failure before commit --> next provider
        |
        +-- success --> stream to Codex
```

| 模块 | 职责 |
| --- | --- |
| `config` | TOML 解析、默认值、环境变量展开、启动期校验 |
| `provider` | HTTP client、鉴权、并发信号量、等待队列、熔断器、状态快照 |
| `router` | 模型到 provider 链的解析、模型列表 |
| `gateway` | 严格优先级循环、错误分类、提交点控制 |
| `server` | Axum 路由、本地鉴权、请求体限制、健康/状态接口 |
| `protocol::sse` | 增量 SSE 解析，支持跨 chunk、CRLF、注释和多行 data |
| `protocol::chat` | Responses 请求 -> Chat 请求、历史消息、工具和工具调用翻译 |
| `upstream::responses` | 原生 Responses 透传、首事件探测、JSON/SSE 适配 |
| `upstream::chat` | Chat 上游调用、首事件探测、Chat SSE -> Responses SSE 翻译 |

## 请求生命周期

### 1. 接入

`POST /v1/responses` 的请求体先经过：

1. `DefaultBodyLimit`（`server.max_body_bytes`）。
2. 本地鉴权中间件（`server.require_auth`）。
3. JSON 解析，读取 `model` 和 `stream`。

`previous_response_id` 在入口直接拒绝。原因：网关不存储 response，且严格优先级意味着下一次请求可能落到另一个 provider；带状态的 response id 无法安全重放。

### 2. 链解析

`ModelRouter::chain_for`：

1. 精确匹配 `[[routes]] model`。
2. 匹配 `[[routes]] model = "*"`。
3. 都没有时，返回所有 provider 的声明顺序。

返回的 `Vec<Arc<Provider>>` 就是本次请求的唯一候选顺序。

### 3. 单 provider 尝试

```text
begin_attempt()
  |
  +-- breaker open --> unavailable, next provider
  |
acquire()
  |
  +-- try_acquire_owned() ok --> permit
  +-- max_queue == 0 --------> queue_full, next provider
  +-- waiting >= max_queue --> queue_full, next provider
  +-- wait > queue_timeout --> queue_timeout, next provider
  |
upstream start()
  |
  +-- non-commit failure --> record_failure, next provider
  |
  +-- success --> ProviderLease moves into response stream
```

`ProviderLease` 持有 `OwnedSemaphorePermit`，因此并发槽位会一直占用到整个 SSE 流结束，而不是只占用到收到响应头。

### 4. 提交点

原生 Responses 和 Chat 适配器都采用“响应头 + 第一个 SSE 事件”双阶段探测：

- 非 2xx：读取错误体，分类，记录失败，降级。
- 2xx 且第一个事件是 `error` / `response.failed`：记录失败，降级。
- 2xx 且第一个事件正常：创建会话，开始向客户端输出。
- 第一个事件前超时/断开：记录失败，降级。

这意味着客户端不会看到“第一个 provider 的 `response.created`，然后突然切换到第二个 provider”这种损坏的流。

### 5. 流式输出

原生 Responses：

- 用 `SseParser` 重新解析上游字节。
- 把 `event` / `data` 原样转发。
- 遇到 `response.completed` / `response.incomplete` 记为成功。
- 遇到 `response.failed` / `error` 记为失败。
- 流在终态事件前结束，补发一个 `error` 事件。

Chat 适配器：

- 读取 Chat SSE chunk。
- 把 `delta.content` 转成 `response.output_text.delta`。
- 把 `delta.tool_calls` 转成 `response.function_call_arguments.*` 或自定义工具输入事件。
- 在 `finish_reason` / `[DONE]` / 流结束时生成 `response.completed`。
- 组装完整的 `response.output`，供 `response.completed` 使用。

### 6. 非流式

原生 Responses 上游：

- 请求体 `stream` 按客户端要求设置。
- 2xx JSON 直接返回。
- 如果上游返回 SSE，网关收集到终态事件后返回最终 response JSON。

Chat 上游：

- 当前直接跳过并尝试下一个 provider；如果没有原生 Responses provider，返回 400。
- 这是 MVP 限制，Codex 本身始终使用流式请求。

## 队列与并发

每个 provider 有独立的 `tokio::sync::Semaphore`：

- `max_concurrency`：信号量许可数，等于同时进行中的上游流上限。
- `max_queue`：最多允许多少个请求等待许可。
- `queue_timeout_ms`：等待许可的最长时间。
- `max_queue = 0`：`try_acquire` 失败立即返回 `queue_full`，不进入等待。

等待计数使用 `AtomicUsize`，在超时、成功、取消时都会减一。`/admin/status` 中的 `queue_depth` 是当前等待数，`inflight = max_concurrency - available_permits`。

队列满和队列超时都是**本地准入控制**，不会触发 provider 熔断器，也不会被算作上游故障。它们只会让本次请求降级到下一个 provider。

## 熔断器

每个 provider 独立维护：

```text
Closed --(N 次连续可重试失败)--> Open(until)
Open --(now >= until)--> HalfOpen
HalfOpen --(success)--> Closed
HalfOpen --(failure)--> Open(until)
```

规则：

- `failure_threshold` 是连续失败阈值。
- 成功会清零连续失败。
- `Auth`（401/403）立即打开熔断器，因为 provider 级 key 错误不会靠重试恢复。
- 冷却时间 = `cooldown_ms * multiplier`，最低 1 秒。
  - Auth: 10x
  - NotFound: 5x
  - RateLimit: 2x
  - 其他: 1x
- HalfOpen 允许多个探测请求通过；任一成功关闭熔断器，任一失败重新打开。
- 本地队列错误和客户端 400 不会触发熔断。

## 错误分类

| 类别 | 典型来源 | 是否降级 | 是否熔断 |
| --- | --- | --- | --- |
| `Auth` | 401 / 403 | 是 | 是（立即） |
| `NotFound` | 404 | 是 | 是 |
| `RateLimit` | 429 | 是 | 是 |
| `Server` | 5xx | 是 | 是 |
| `Network` | 连接/读取失败 | 是 | 是 |
| `Timeout` | 连接/首字节/空闲超时 | 是 | 是 |
| `Protocol` | 非法 SSE、提前 EOF | 是 | 是 |
| `Client` | 400 / 422 普通错误 | 否 | 否 |
| `ContextLength` | 上下文超限 | 否 | 否 |
| `ContentPolicy` | 内容策略拒绝 | 否 | 否 |
| `QueueFull` | 本地队列满 | 是（下一个 provider） | 否 |
| `QueueTimeout` | 本地排队超时 | 是（下一个 provider） | 否 |

全部 provider 都失败时，`GatewayError::all_failed` 根据尝试记录选择 HTTP 状态：

- 有 429 / 队列满 / 队列超时：429 `all_providers_busy`
- 有上游超时：504 `all_providers_timeout`
- 全部是熔断不可用：503 `all_providers_unavailable`
- 其他：502 `all_providers_failed`

错误响应保持 OpenAI 风格：

```json
{
  "error": {
    "message": "...",
    "type": "gateway_error",
    "param": null,
    "code": "all_providers_busy"
  }
}
```

## Responses -> Chat 翻译

### 输入历史

Responses 的 `input` 是 item 数组，Chat 的 `messages` 是角色消息数组：

| Responses item | Chat message |
| --- | --- |
| `message` + `role=developer/system` | `system` |
| `message` + `role=user` | `user` |
| `message` + `role=assistant` | `assistant` |
| `function_call` | 追加到上一条 assistant 的 `tool_calls` |
| `function_call_output` | `tool` + `tool_call_id` |
| `custom_tool_call` | assistant `tool_calls`，arguments = `{"input": "..."}` |
| `custom_tool_call_output` | `tool` + `tool_call_id` |
| `reasoning` | 丢弃 |
| `additional_tools` | 合并到顶层 tools，不在历史中输出 |

连续的系统消息会合并成一条，避免部分 Chat 上游拒绝多条 system message。

### 工具

- 顶层 `tools` 和 `input` 中的 `additional_tools` 都会收集。
- `namespace` 会递归展开，工具名变成 `namespace__name`。
- 工具名会清洗为 `[A-Za-z0-9_-]`，超过 64 字符时截断并附加 FNV-1a 哈希。
- `custom` 工具变成只有一个 `input: string` 参数的函数，description 中附带格式约束（最多 2,000 字符）。
- Responses 原生工具（`web_search`、`tool_search` 等）没有 Chat 等价物，直接跳过。

返回时通过 `ToolRoutes` 反查原始名称、namespace 和工具类型：

- `function` -> `function_call`
- `custom` -> `custom_tool_call`，并从 `{"input": "..."}` 中取出原始输入
- 如果原工具在命名空间中，`custom_tool_call` / `function_call` 会带上 `namespace` 字段（Codex lite 模式的 `functions__exec` 会还原为 `namespace="functions"`、`name="exec"`）

### Chat SSE -> Responses SSE

```text
role/delta.content
  -> response.output_item.added (message)
  -> response.content_part.added
  -> response.output_text.delta
  -> response.output_text.done
  -> response.content_part.done
  -> response.output_item.done

delta.tool_calls[].function.arguments
  -> response.output_item.added (function_call)
  -> response.function_call_arguments.delta
  -> response.function_call_arguments.done
  -> response.output_item.done

custom tool arguments
  -> response.output_item.added (custom_tool_call)
  -> response.custom_tool_call_input.delta
  -> response.custom_tool_call_input.done
  -> response.output_item.done
```

最后生成 `response.completed`（或 `response.incomplete`），并附带完整 `output` 和 `usage`。

## 原生 Responses 透传

- 复制客户端 JSON，只改 `model` 和 `stream`。
- 转发白名单 header：`openai-beta`、`session-id`、`thread-id`、`originator`、`x-codex-turn-metadata`。
- 不转发 `authorization`、`host`、`content-length` 等逐跳或网关自有 header。
- 上游 2xx JSON 且客户端要求流式时，合成：
  - `response.created`（`status=in_progress`、空 output）
  - `response.completed` / `response.incomplete` / `response.failed`
- 上游 SSE 且客户端要求非流式时，收集到终态事件并返回 response JSON。

## 安全

- 默认监听 `127.0.0.1`。
- `require_auth = true` 时，所有受保护接口校验 Bearer token 或 `x-api-key`；比较使用常量时间实现。
- API key 只从配置或环境变量读取，不写入日志。
- `log_request_bodies` 默认关闭；开启后请求体（可能包含源码和 prompt）会进入日志。
- `/healthz` 和 `/readyz` 不需要鉴权，便于 Windows 服务/探针使用；它们不返回 provider key。

## 后续可扩展点

- Chat 适配器的非流式聚合。
- Chat reasoning 字段（如 `reasoning_content`）到 Responses reasoning 事件的映射。
- 更精细的 `Retry-After` 处理（当前读取但未用于等待）。
- 原生 Responses 的 `response.failed` 后错误体保留和更细的错误码映射。
- 可选的 Windows Service 包装（当前是控制台进程）。
- 指标导出（Prometheus）和请求级 tracing id。
