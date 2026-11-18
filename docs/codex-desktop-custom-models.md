# Codex 桌面端显示自定义模型

Codex CLI 的 `--profile` 配置不会自动进入桌面端模型选择器。桌面端选择器来自 app-server 的 `model/list`，只读取启动时加载的模型目录；要让 Claude 等非内置模型出现在桌面端，需要配置 `model_catalog_json`。

Codex 的基础接入见 [codex-setup.md](codex-setup.md)。

## 1. 确认桌面端使用的配置目录

Windows 桌面即使启用了 `runCodexInWindowsSubsystemForLinux = true`，也可能仍把 `CODEX_HOME` 指向 Windows 用户目录：

```powershell
wsl.exe -d Ubuntu -- bash -lc 'pid=$(pgrep -f "codex.*app-server" | head -n1); tr "\0" "\n" < "/proc/$pid/environ" | grep -E "^(HOME|CODEX_HOME)="'
```

如果输出 `CODEX_HOME=/mnt/c/Users/<user>/.codex`，则应修改 Windows 的 `C:\Users\<user>\.codex\config.toml`，而不是 WSL 内的 `~/.codex/config.toml`。

## 2. 合并模型目录

`model_catalog_json` 会**替换**内置目录，不会自动追加。因此需要先按 `slug` 合并内置目录与自定义模型，生成一份完整目录。

用桌面实际使用的 Codex 二进制导出基础目录。进程命令行中的第一个参数就是该二进制路径：

```powershell
wsl.exe -d Ubuntu -- bash -lc 'ps -eo args | grep "[c]odex.*app-server" | head -n1'
wsl.exe -d Ubuntu -- bash -lc '<codex-path> debug models --bundled'
```

将自定义模型条目按 `slug` 合并进基础目录，保存为例如 `models-desktop-merged.json`。待显示的模型应满足 `visibility = "list"` 和 `supported_in_api = true`，并用实际 Codex 版本验证整份 JSON 可以解析。PowerShell 7 示例会把同名模型替换为自定义条目，其余基础模型保留：

```powershell
$baseJson = wsl.exe -d Ubuntu -- bash -lc '<codex-path> debug models --bundled'
$base = ($baseJson -join "`n" | ConvertFrom-Json).models
$custom = (Get-Content .\claude-models.json -Raw | ConvertFrom-Json).models
$bySlug = [ordered]@{}
foreach ($model in $base) { $bySlug[$model.slug] = $model }
foreach ($model in $custom) { $bySlug[$model.slug] = $model }
@{ models = @($bySlug.Values) } | ConvertTo-Json -Depth 100 |
  Set-Content .\models-desktop-merged.json -Encoding utf8NoBOM
```

## 3. 引用合并后的目录

在桌面实际读取的配置中引用合并后的目录。app-server 在 WSL 内运行，路径必须使用 WSL 形式：

```toml
model = "gpt-6-luna"
model_catalog_json = "/mnt/c/Users/<user>/.codex/models-desktop-merged.json"
```

不要写成 `C:\Users\...`：WSL 进程会把 Windows 路径当作不存在的文件，桌面端可能报 `No such file or directory (os error 2)`。默认 `model` 可以继续保留 GPT，启动后再从选择器切换到 Claude。

## 4. 验证并重启

```powershell
wsl.exe -d Ubuntu -- bash -lc 'CODEX_HOME=/mnt/c/Users/<user>/.codex <codex-path> debug models'
```

输出中应包含目标模型。`model_catalog_json` 只在启动时加载，修改后必须完全退出并重新打开 Codex 桌面端，仅关闭窗口或重载界面不会生效。

## 相关文档

- [codex-setup.md](codex-setup.md)：Codex 基础配置、自动同步和 WSL 网络
- [configuration.md](configuration.md)：provider 配置和 Messages/Claude 参数
