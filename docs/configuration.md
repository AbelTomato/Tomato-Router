# 配置参考

本文列出 `[[providers]]` 的完整字段、鉴权方式和上游协议兼容项。最小可用配置和启动步骤见 [README](../README.md)。

## Provider

```toml
[[providers]]
id = "primary"                 # 必填，唯一
name = "Primary relay"         # 可选，展示名
base_url = "https://relay/v1"  # 必填，API 根路径（通常以 /v1 结尾）
wire_api = "responses"         # "responses"、"chat" 或 "messages"（默认 "chat"）
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

### 鉴权方式

| 配置 | 行为 |
| --- | --- |
| `api_key_env = "NAME"` | 从环境变量读取，默认 `Authorization: Bearer <key>` |
| `api_key = "sk-..."` 或 `api_key = "${NAME}"` | 直接配置或展开环境变量 |
| `auth = { type = "none" }` | 不发送鉴权 |
| `auth = { type = "header", header = "x-api-key", prefix = "" }` | 自定义 header |
| `auth = { type = "query", query_param = "api_key" }` | 查询参数 |

`base_url` 应指向 API 根路径，例如 `https://relay.example.com/v1`。网关会自动拼接 `responses`、`chat/completions` 或 `messages`；Messages 模式也兼容不带 `/v1` 的根路径。

## Chat 上游兼容选项

```toml
[providers.compat]
max_tokens_field = "max_tokens"          # 或 "max_completion_tokens"
supports_stream_options = true
supports_reasoning_effort = false
supports_parallel_tool_calls = true
# reasoning_field = "reasoning_content"
# extra_body = { top_k = 20 }
```

## Messages 上游（Anthropic Claude）

```toml
[[providers]]
id = "claude"
name = "Claude relay"
base_url = "https://relay.example.com/v1"   # 也可只写到域名根路径
wire_api = "messages"
api_key_env = "CLAUDE_API_KEY"

max_concurrency = 2
max_queue = 0
queue_timeout_ms = 800

[providers.auth]
type = "header"
header = "x-api-key"
prefix = ""

[providers.models]
"claude-opus-5-5" = "claude-opus-5-5"

[providers.compat]
thinking = "auto"              # "auto"、"adaptive" 或 "off"
default_max_tokens = 16384     # 请求未带 max_output_tokens 时使用
```

Messages 适配器的行为：

- **鉴权**：Anthropic Messages 使用 `x-api-key`，不是 `Authorization: Bearer`。必须显式配置 `[providers.auth]`，否则网关会按默认 Bearer 方式发送。
- **版本头**：自动补 `anthropic-version: 2023-06-01`；如果在 `[providers.headers]` 中自行设置了该头，则以配置为准。
- **路径**：`base_url` 可以带 `/v1`，也可以只写到域名根路径；适配器会拼出正确的 `/v1/messages`，不会产生 `/v1/v1/messages`。
- **thinking**：默认 `auto`。`reasoning.effort = "none"` 时关闭；其他情况发送 `thinking: { type: "adaptive" }`，并按 `minimal/low → low`、`medium → medium`、`high → high`、`xhigh/max/ultra → max` 映射 `output_config.effort`。`claude-opus-5-5` 不支持 `enabled + budget_tokens`，必须使用 adaptive。
- **reasoning 往返**：上游返回的 thinking 文本和 signature 会被编码进 Responses `reasoning` item 的 `encrypted_content`（base64url）。下一轮请求携带该 item 时，网关解码并还原为原生 `thinking` 块；`redacted_thinking` 也会保留。
- **工具**：函数工具和自定义工具都会转成 Messages `tools`；自定义工具仍使用单 `input` 字符串包装，返回时还原为 `custom_tool_call`。`web_search`、`tool_search` 等原生 Responses 工具会被丢弃。
- **消息重组**：developer/system 消息合并进顶层 `system`；assistant 文本和 `tool_use` 合并到同一条 assistant 消息；`tool_result` 合并到 user 消息。
- **max_tokens**：Anthropic 要求该字段。请求带 `max_output_tokens` 时使用请求值，否则使用 `compat.default_max_tokens`（默认 16384）。

## 编辑方式

TUI 可以编辑 provider 的基础字段、鉴权方式、并发/队列/熔断参数和模型映射。`headers`、`query`、`compat`、`retry_after_wait_ms` 等高级字段需要直接编辑配置文件；TUI 保存会重写完整 TOML，不保留注释和未知字段。详见 [tui.md](tui.md#保存和启动语义)。

## 相关文档

- [architecture.md](architecture.md)：请求生命周期、故障转移和协议翻译
- [codex-setup.md](codex-setup.md)：Codex 接入和自动同步
