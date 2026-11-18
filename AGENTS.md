# AGENTS.md

本文件定义 Tomato Router 的文档规范与开发日志规范，作为长期维护时的协作约定。所有参与开发的人和 AI Agent 都应遵循。

## 1. 文档规范

### 1.1 语言与风格

- 项目文档默认使用中文；代码、命令、API 字段、错误码和配置键保留英文原文。
- 使用 GitHub Flavored Markdown。
- 先写结论，再写细节；可执行命令放在代码块中。
- 文件路径使用仓库相对路径，例如 `src/gateway.rs`、`docs/architecture.md`。
- 文档描述必须与当前代码一致。行为变化时，同步更新相关文档，不能只改代码。
- 不记录任何真实 API key、token、密码或完整鉴权请求头。示例统一使用 `sk-...`、`${ENV_NAME}` 等占位符。

### 1.2 文档目录

| 路径 | 用途 |
| --- | --- |
| `README.md` | 用户向文档：快速开始、配置说明、Codex/WSL 接入、已知限制 |
| `docs/architecture.md` | 内部架构：请求生命周期、严格优先级、失败转移、协议翻译 |
| `docs/configuration.md` | 完整配置参考：provider、鉴权、上游兼容项 |
| `docs/codex-setup.md` | Codex 接入、自动同步和 WSL 网络访问 |
| `docs/codex-desktop-custom-models.md` | Codex 桌面端自定义模型目录 |
| `docs/tui.md` | TUI 按键、保存语义和日志行为 |
| `docs/devlog/YYYY-MM-DD.md` | 按日期归档的开发日志 |
| `AGENTS.md` | 本规范与长期维护约定 |

### 1.3 更新时机

- 新增或修改配置项：更新 `README.md` 的配置参考和 `config/tomato.example.toml`。
- 修改请求流程、失败转移、熔断、协议转换：更新 `docs/architecture.md`。
- 修复重要 bug、完成关键验证、发现风险：写入当天开发日志。
- 纯内部重构且无行为变化：至少写入开发日志，说明原因和影响范围。
- 提交前检查代码、测试、文档、开发日志是否一致。

## 2. 开发日志规范

### 2.1 目录与命名

- 目录：`docs/devlog/`
- 文件名：`YYYY-MM-DD.md`，使用本地日期（Asia/Shanghai，UTC+8）。
- 每天一个文件；跨天继续工作则在新日期文件中继续记录。

### 2.2 时间与顺序

- 每条记录必须以 `HH:MM` 开头，使用 24 小时制本地时间。
- 记录按时间从上到下排列，最早的在最上面。
- 新事件追加到文件末尾。
- 如果补记更早的事件，插入到正确的时间位置，保持整体时间顺序。

### 2.3 内容要求

- 简要：一条记录通常 1-3 句话，能独立阅读。
- 保留关键信息：做了什么、为什么做、验证结果、影响范围、后续动作。
- 涉及代码、提交或文件时写明具体标识，例如 `5483443`、`src/gateway.rs`。
- 记录故障时包含：现象、关键错误码或报错、初步判断、是否已修复。
- 不记录：密钥、完整 prompt、大段原始日志、无结论的流水账。
- 需要展开的长内容放到 `docs/` 下的专题文档，开发日志只保留摘要和链接。

### 2.4 文件模板

```markdown
# YYYY-MM-DD

- HH:MM 事件摘要。关键决策或验证结果。相关文件/提交：`...`。
- HH:MM 事件摘要。
```

### 2.5 示例

```markdown
# 2026-11-13

- 16:35 完成 mock Chat 上游的非 lite P0，Codex 成功执行 `exec_command`。
- 16:52 `cargo fmt`、`cargo clippy`、19 个测试通过；创建初始提交 `5483443`。
- 17:03 真实中转站 Chat 模式返回 `RELAY_OK`，WSL Codex P0 通过。
```

## 3. 维护约定

- 开始一天的工作前，先阅读最近一篇开发日志，了解当前状态和未完成事项。
- 结束当天工作前，补全当天日志，尤其是未完成事项和风险。
- 重要决策不能只留在开发日志中；应同步写入 `README.md` 或 `docs/architecture.md`。
- `config/tomato.toml` 可能包含真实密钥，已被 `.gitignore` 忽略；不得把其中的密钥复制到文档、日志或提交中。
- 提交前执行：

```powershell
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```
