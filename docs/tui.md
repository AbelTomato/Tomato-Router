# TUI 控制台

TUI 和网关运行在同一个进程里。退出 TUI（`q` 或 Ctrl+C）会先停止网关，不会留下后台服务；需要常驻服务时使用 headless 启动方式。TUI 要求交互式终端，建议使用 Windows Terminal，窗口至少 60x18。

## 启动

```powershell
.\target\release\tomato-router.exe --config config\tomato.toml tui
```

打开后立即启动网关：

```powershell
.\target\release\tomato-router.exe --config config\tomato.toml tui --autostart
```

## 界面与按键

界面包含 Dashboard、Providers、Routes、Settings、Logs、Help 六页，按 `1`-`6` 或 `Tab` 切换。

| 按键 | 作用 |
| --- | --- |
| `s` / `x` / `r` | 启动 / 停止 / 重启网关 |
| `F5` | 必要时先保存，再启动或重启 |
| `w` | 保存配置 |
| `v` | 查看校验结果、错误和警告 |
| `R` | 从磁盘重新加载配置 |
| `a` / `Enter` / `d` | 新增 / 编辑 / 删除当前页的 provider 或 route |
| `Ctrl+Up` / `Ctrl+Down` | 调整 provider 声明顺序（即默认优先级） |
| `q` / `Ctrl+C` | 退出；网关运行时会先确认并停止 |
| `?` | 打开帮助 |

## 保存和启动语义

- 保存会重写完整 TOML，并在同目录生成 `config/tomato.toml.bak-tomato-<时间戳>` 备份。**注释和未知字段不会保留**，需要保留注释或编辑 `headers`、`query`、`compat`、`retry_after_wait_ms` 等高级字段时，请直接编辑配置文件。
- 启动和重启总是读取磁盘上的已保存配置；存在未保存修改时会先提示保存。
- 停止时最多等待 5 秒让活跃请求结束，超时后中断剩余请求。
- API key 在非编辑状态下会掩码显示，但内联 `api_key` 仍以明文写入配置；备份文件也可能包含密钥，注意保护或删除。
- 环境变量在网关启动时解析；修改环境变量后需要退出并重新启动 TUI，仅在 TUI 内重启网关不会重新读取父进程环境。

日志页只保留进程内的环形缓冲，不会写入日志文件；需要持久日志时使用 headless 模式并重定向输出。

进程模型、终端生命周期和日志实现见 [architecture.md](architecture.md#tui-控制台)。
