# Tomato Router

面向 Codex 的本地 OpenAI Responses API 网关。用 Rust 编写，运行在 Windows 上，把 Windows 原生和 WSL 里的 Codex 流量统一收敛到一个入口，并按**严格优先级**在多个 OpenAI 兼容中转站之间故障转移。

## 特性

- 对外只暴露 Responses API（`POST /v1/responses`），满足 Codex 0.167+ 的 `wire_api = "responses"` 要求。
- 上游支持两种模式：
  - `wire_api = "responses"`：原生 Responses 透传，保留 reasoning、原生工具和完整流式事件。
  - `wire_api = "chat"`：把 Responses 请求翻译成 Chat Completions，再把 Chat SSE 翻译回 Responses SSE。
- 支持 Codex lite 模式的 `additional_tools`：命名空间工具会展开为 Chat 函数（如 `functions__exec`），返回时恢复为带 `namespace` 的 `custom_tool_call`。
- 严格优先级：每个新请求都从最高优先级开始；某个 provider 失败、队列满或熔断时才向下走，绝不反向或加权路由。
- 每 provider 独立并发上限、等待队列、队列超时和熔断器。
- `max_queue = 0` 表示不等待：并发槽位满时立即降级到下一个 provider。
- 在向客户端提交第一个事件之前，所有失败都可自动降级；一旦开始输出，失败会以 `response.failed` / `error` 事件结束。
- 本地 Bearer token 鉴权、请求体大小限制、健康检查和运行状态接口。

## 快速开始

### 1. 构建

```powershell
cargo build --release
cargo test
```

### 2. 配置

```powershell
Copy-Item config\tomato.example.toml config\tomato.toml
```

编辑 `config\tomato.toml`，至少修改：

- 每个 provider 的 `base_url`
- API key 环境变量名
- `[providers.models]` 中的模型映射
- `[[routes]]` 中的优先级链

然后设置密钥并校验配置：

```powershell
$env:PRIMARY_API_KEY = "sk-primary-..."
$env:BACKUP_API_KEY  = "sk-backup-..."

.\target\release\tomato-router.exe --config config\tomato.toml --check
```

### 3. 启动

```powershell
.\target\release\tomato-router.exe --config config\tomato.toml
```

默认监听 `127.0.0.1:8787`。启动日志会打印实际监听地址和 provider 数量。

```powershell
curl.exe http://127.0.0.1:8787/healthz
```

如果启用了本地鉴权：

```powershell
$env:TOMATO_ROUTER_KEY = "local-random-secret"
curl.exe -H "Authorization: Bearer $env:TOMATO_ROUTER_KEY" http://127.0.0.1:8787/admin/status
```

## 配置 Codex

Codex 的 provider 配置必须使用 `wire_api = "responses"`。以下内容可以放在 Windows 的 `%USERPROFILE%\.codex\config.toml`，也可以放在 WSL 的 `~/.codex/config.toml`。

```toml
model = "gpt-5.5"
model_provider = "tomato"

[model_providers.tomato]
name = "Tomato Router"
base_url = "http://127.0.0.1:8787/v1"
env_key = "TOMATO_ROUTER_KEY"
wire_api = "responses"
```

设置环境变量后再启动 Codex：

```powershell
$env:TOMATO_ROUTER_KEY = "local-random-secret"
codex
```

如果网关关闭了本地鉴权，Codex 仍然可以配置一个占位 `env_key`，网关不会校验它。

### WSL 访问 Windows 上的网关

有两种方式，任选一种。

**方式 A：WSL 镜像网络（推荐）**

在 Windows 用户目录创建或修改 `%USERPROFILE%\.wslconfig`：

```ini
[wsl2]
networkingMode=mirrored
```

然后重启 WSL：

```powershell
wsl --shutdown
```

重启后 WSL 内的 `127.0.0.1` 会映射到 Windows 本机，Codex 直接使用：

```toml
base_url = "http://127.0.0.1:8787/v1"
```

**方式 B：NAT 网络**

1. 把网关改成监听所有网卡：

   ```toml
   [server]
   listen = "0.0.0.0:8787"
   require_auth = true
   api_keys = ["${TOMATO_ROUTER_KEY}"]
   ```

2. 放行 Windows 防火墙的 TCP 8787 入站。
3. 在 WSL 中取 Windows 主机 IP：

   ```bash
   ip route show | awk '/default/ {print $3}'
   ```

4. Codex 配置使用 `http://<Windows-IP>:8787/v1`，并设置 `TOMATO_ROUTER_KEY`。

> 绑定 `0.0.0.0` 时务必启用 `require_auth`。不要把无鉴权的网关暴露到局域网或公网。

## API

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `POST` | `/v1/responses` | Responses API，支持流式和非流式（非流式需要原生 Responses 上游） |
| `GET` | `/v1/models` | 已配置的模型列表 |
| `GET` | `/v1/models/{model}` | 单个模型 |
| `GET` | `/v1/responses/{id}` | 固定返回 404：网关无状态，不存储 response |
| `GET` | `/healthz` | 存活检查 |
| `GET` | `/readyz` | 至少一个 provider 的熔断器可用时返回 200 |
| `GET` | `/admin/status` | provider 队列、并发、熔断器和鉴权状态 |
| `GET` | `/` | 服务信息 |

所有 `/v1/*` 和 `/admin/*` 接口在 `require_auth = true` 时都需要 `Authorization: Bearer <key>`，也接受 `x-api-key: <key>`。

## 优先级与故障转移语义

### 路由链

请求模型按以下顺序解析 provider 链：

1. 精确匹配的 `[[routes]] model`
2. `[[routes]] model = "*"`
3. 没有匹配路由时，按 provider 在配置文件中的声明顺序

链内顺序就是严格优先级。每个新请求都从链首开始，不会因为上一个请求用了低优先级 provider 就继续用它。

### 单次尝试

对链中的每个 provider：

1. 熔断器打开且未到冷却时间：跳过，记录 `provider_unavailable`。
2. 尝试获取并发槽位：
   - 有空闲槽位：立即获得。
   - 没有空闲槽位且 `max_queue = 0`：记录 `queue_full`，立即尝试下一个 provider。
   - 没有空闲槽位且 `max_queue > 0`：最多等待 `queue_timeout_ms`；超时记录 `queue_timeout`，尝试下一个。
3. 发出上游请求，等待响应头和第一个 SSE 事件。
4. 成功：把请求提交给该 provider，后续事件直接流给客户端。
5. 失败：如果是可重试错误，记录并尝试下一个 provider。

### 哪些错误会降级

会降级：

- 401 / 403（上游 key 无效）
- 404（模型不存在）
- 429
- 5xx
- 网络错误、连接超时、首字节超时
- 上游协议错误（例如流在第一个事件前结束）

不会降级，直接返回客户端：

- 400 普通请求错误
- 上下文长度超限
- 内容策略拒绝

这样做是为了避免把同一个明显无效的请求重复发送给所有 provider，掩盖真正的问题。

### 提交点

网关会先读取上游的响应头和**第一个 SSE 事件**，确认不是错误事件后才向客户端发送 `response.created`。因此以下情况仍可安全降级：

- 连接失败
- 上游返回非 2xx
- 上游返回 200 但第一个事件是 `error` / `response.failed`
- 上游在第一个事件前断开

一旦第一个正常事件已经发给客户端，协议上就没有安全的降级点了。后续错误会变成客户端可见的 `response.failed` 或 `error` 事件。

## Provider 配置参考

```toml
[[providers]]
id = "primary"                 # 必填，唯一
name = "Primary relay"         # 可选，展示名
base_url = "https://relay/v1"  # 必填，API 根路径（通常以 /v1 结尾）
wire_api = "responses"         # "responses"（默认 "chat"）
api_key_env = "PRIMARY_KEY"    # 推荐
# api_key = "${PRIMARY_KEY}"   # 也支持环境变量展开
# auth = { type = "none" }

max_concurrency = 4            # 同时进行中的流数量
max_queue = 8                  # 等待队列长度；0 = 不等待
queue_timeout_ms = 800         # 排队最长时间
cooldown_ms = 30000            # 熔断基础冷却时间
failure_threshold = 3          # 连续失败多少次后打开熔断器
retry_after_wait_ms = 1000     # 预留：上游 Retry-After 的处理策略

[providers.models]
"client-model" = "upstream-model"
```

鉴权方式：

| 配置 | 行为 |
| --- | --- |
| `api_key_env = "NAME"` | 从环境变量读取，默认 `Authorization: Bearer <key>` |
| `api_key = "sk-..."` 或 `api_key = "${NAME}"` | 直接配置或展开环境变量 |
| `auth = { type = "none" }` | 不发送鉴权 |
| `auth = { type = "header", header = "x-api-key", prefix = "" }` | 自定义 header |
| `auth = { type = "query", query_param = "api_key" }` | 查询参数 |

`base_url` 应指向 API 根路径，例如 `https://relay.example.com/v1`。网关会自动拼接 `responses` 或 `chat/completions`。

### Chat 上游兼容选项

```toml
[providers.compat]
max_tokens_field = "max_tokens"          # 或 "max_completion_tokens"
supports_stream_options = true
supports_reasoning_effort = false
supports_parallel_tool_calls = true
# reasoning_field = "reasoning_content"
# extra_body = { top_k = 20 }
```

## 已知限制

- **Chat 适配器只支持流式请求。** Codex 始终发送 `stream: true`，所以正常使用不受影响；`stream: false` 需要原生 Responses 上游。
- **Chat 适配器会丢弃原生工具。** `web_search`、`tool_search` 等 Responses 原生工具没有 Chat Completions 等价物，会被跳过。函数工具和自定义工具会被翻译。
- **Chat 适配器会丢弃 reasoning item。** Chat Completions 没有可移植的加密 reasoning 表示；可见消息和工具调用历史仍完整保留。
- **`apply_patch` 是尽力而为的转换。** 网关把自定义工具包装成只有一个 `input` 字符串的函数，再把 `input` 还原成 `custom_tool_call`。模型如果不遵守格式，可能生成无效 patch。
- **网关完全无状态。** `previous_response_id` 会被拒绝；Codex 使用 `store: false` 并每次发送完整历史，因此可以安全地跨 provider 重放。
- **不保证 prompt cache 命中。** 原生 Responses 模式下会转发 `session-id`、`thread-id`、`x-codex-turn-metadata` 等白名单 header，但不同 provider 的缓存策略不同。
- **Chat 模式的 reasoning 内容目前不会转成 reasoning 事件。** 只输出可见文本和工具调用。

## 开发

```powershell
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

项目结构：

```text
src/
  config.rs          TOML 配置、校验、环境变量展开
  provider.rs        并发槽位、等待队列、熔断器、鉴权、HTTP client
  router.rs          严格优先级链解析和模型列表
  gateway.rs         失败转移主循环
  server.rs          Axum 路由、本地鉴权、状态接口
  protocol/
    chat.rs          Responses -> Chat 请求/历史/工具翻译
    sse.rs           SSE 解析器
  upstream/
    chat.rs          Chat 上游调用和 Chat -> Responses SSE 翻译
    responses.rs     Responses 原生透传和 JSON/SSE 适配
docs/
  architecture.md    内部流程和状态机
```

更多实现细节见 [docs/architecture.md](docs/architecture.md)。
