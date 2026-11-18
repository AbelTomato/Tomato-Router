# Tomato Router

面向 Codex 的本地 OpenAI Responses API 网关。用 Rust 编写，运行在 Windows 上，把 Windows 原生和 WSL 里的 Codex 流量统一收敛到一个入口，并按**严格优先级**在多个 OpenAI 兼容中转站之间故障转移。

## 特性

- 对外只暴露 Responses API（`POST /v1/responses`），满足 Codex 0.167+ 的 `wire_api = "responses"` 要求。
- 上游支持三种模式：
  - `wire_api = "responses"`：原生 Responses 透传，保留 reasoning、原生工具和完整流式事件。
  - `wire_api = "chat"`：把 Responses 请求翻译成 Chat Completions，再把 Chat SSE 翻译回 Responses SSE。
  - `wire_api = "messages"`：把 Responses 请求翻译成 Anthropic Messages，支持 Claude thinking/signature 往返，再把 Messages SSE 翻译回 Responses SSE。
- 支持 Codex lite 模式的 `additional_tools`：命名空间工具会展开为上游工具名（如 `functions__exec`），返回时恢复为带 `namespace` 的调用。
- 严格优先级：每个新请求都从最高优先级开始；某个 provider 失败、队列满或熔断时才向下走，绝不反向或加权路由。
- 每 provider 独立并发上限、等待队列、队列超时和熔断器。
- `max_queue = 0` 表示不等待：并发槽位满时立即降级到下一个 provider。
- 在向客户端提交第一个事件之前，所有失败都可自动降级；一旦开始输出，失败会以 `response.failed` / `error` 事件结束。
- 本地 Bearer token 鉴权、请求体大小限制、健康检查和运行状态接口。
- 内置 TUI 控制台，可在同一进程内编辑配置、查看 provider 状态和日志，并启动/停止/重启网关。

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

### 4. TUI 控制台（可选）

不想开两个终端时，可以用内置 TUI 同时编辑配置和控制网关：

```powershell
.\target\release\tomato-router.exe --config config\tomato.toml tui
```

打开后立即启动网关：

```powershell
.\target\release\tomato-router.exe --config config\tomato.toml tui --autostart
```

TUI 要求交互式终端，建议使用 Windows Terminal，窗口至少 60x18。按键、保存语义和日志行为见 [docs/tui.md](docs/tui.md)。

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

自动更新 Codex 配置：

```powershell
.\target\release\tomato-router.exe --config config\tomato.toml codex sync
```

命令支持 `--dry-run`、显式 `--codex-config` / `--codex-home` 和 WSL UNC 路径，细节见 [docs/codex-setup.md](docs/codex-setup.md)。

WSL 访问 Windows 上的网关有镜像网络和 NAT 两种方式，见 [docs/codex-setup.md#wsl-访问-windows-上的网关](docs/codex-setup.md#wsl-访问-windows-上的网关)。

要让 Claude 等非内置模型出现在 Codex 桌面端选择器，需要配置 `model_catalog_json`；完整步骤见 [docs/codex-desktop-custom-models.md](docs/codex-desktop-custom-models.md)。

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

## 路由与故障转移

请求模型按以下顺序解析 provider 链：

1. 精确匹配的 `[[routes]] model`
2. `[[routes]] model = "*"`
3. 没有匹配路由时，按 provider 在配置文件中的声明顺序

链内顺序就是严格优先级。每个新请求都从链首开始，不会因为上一个请求用了低优先级 provider 就继续用它。

网关会先读取上游的响应头和**第一个 SSE 事件**，确认不是错误事件后才向客户端发送 `response.created`。因此连接失败、上游非 2xx、错误首事件和首事件前断开都可以安全降级；一旦第一个正常事件已经发给客户端，后续错误会变成客户端可见的 `response.failed` 或 `error` 事件。

401/403、404、429、5xx、网络错误、连接超时、首字节超时和上游协议错误会触发降级；400 普通请求错误、上下文长度超限和内容策略拒绝会直接返回客户端。

完整状态机、队列、熔断器和错误分类见 [docs/architecture.md](docs/architecture.md)。

## 配置参考

最小 provider 示例：

```toml
[[providers]]
id = "primary"
name = "Primary relay"
base_url = "https://relay.example.com/v1"
wire_api = "responses"         # "responses"、"chat" 或 "messages"
api_key_env = "PRIMARY_KEY"

max_concurrency = 4
max_queue = 8
queue_timeout_ms = 800
cooldown_ms = 30000
failure_threshold = 3

[providers.models]
"client-model" = "upstream-model"
```

鉴权方式、Chat 兼容选项、Messages/Claude 参数和高级字段见 [docs/configuration.md](docs/configuration.md)。

## 已知限制

- **Chat 和 Messages 适配器只支持流式请求。** 非流式请求需要原生 Responses 上游；Codex 始终发送 `stream: true`，正常使用不受影响。
- **Chat 和 Messages 适配器会丢弃原生工具。** `web_search`、`tool_search` 等没有等价物；函数工具和自定义工具会翻译。
- **Messages 的 thinking 通过 reasoning summary 暴露。** thinking 文本和 signature 保存在 `encrypted_content` 中，隐藏推理不保证完整可见；MVP 不实现 `cache_control`。
- **网关完全无状态。** `previous_response_id` 会被拒绝；Codex 使用 `store: false` 并每次发送完整历史。
- **不保证 prompt cache 命中。** 不同 provider 的缓存策略不同。
- **`apply_patch` 是尽力而为的转换。** 模型不遵守格式时可能生成无效 patch。

更多协议限制和后续扩展点见 [docs/architecture.md](docs/architecture.md)。

## 开发

```powershell
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

代码结构、组件职责和请求流程见 [docs/architecture.md](docs/architecture.md)。

## 文档

- [docs/architecture.md](docs/architecture.md)：请求生命周期、严格优先级、失败转移、协议翻译
- [docs/configuration.md](docs/configuration.md)：完整 provider、鉴权和上游兼容项
- [docs/codex-setup.md](docs/codex-setup.md)：Codex 接入、自动同步和 WSL 网络
- [docs/codex-desktop-custom-models.md](docs/codex-desktop-custom-models.md)：桌面端自定义模型目录
- [docs/tui.md](docs/tui.md)：TUI 按键和保存语义
- [docs/devlog/](docs/devlog/)：开发日志
