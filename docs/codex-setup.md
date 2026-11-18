# Codex 接入

## 基础配置

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

## 自动同步 Codex 配置

网关配置完成后，可以让 Tomato Router 自动更新 Codex 的用户级 `config.toml`。命令只维护 `model`、`model_provider` 以及目标 provider 下的 `name`、`base_url`、`env_key` 和 `wire_api`，会保留 Codex 的其他设置、注释和已有 provider 字段；不会把真实 API key 写入文件或输出到日志。

```powershell
.\target\release\tomato-router.exe --config config\tomato.toml codex sync
```

默认目标是 Windows 的 `%USERPROFILE%\.codex\config.toml`，Linux/WSL 中则使用 `$CODEX_HOME/config.toml`，没有 `CODEX_HOME` 时使用 `~/.codex/config.toml`。可以显式指定目标：

```powershell
# Windows 原生 Codex
.\target\release\tomato-router.exe --config config\tomato.toml codex sync `
  --codex-config "$env:USERPROFILE\.codex\config.toml" `
  --model gpt-5.5

# 从 Windows 访问 WSL 中的 Codex 配置
.\target\release\tomato-router.exe --config config\tomato.toml codex sync `
  --codex-config "\\wsl.localhost\Ubuntu\home\user\.codex\config.toml"
```

执行前可用 `--dry-run` 预览目标和将写入的字段。已有文件在修改前会生成同目录的 `config.toml.bak-tomato-<时间戳>` 备份，并使用同目录临时文件替换；重复执行不会重复添加字段。修改后重启 Codex；如果使用 Codex daemon，也需要重启 daemon 使其重新读取配置。

`--codex-config` 优先级高于 `--codex-home`，两者都未提供时才读取 `CODEX_HOME`。Windows 上同步 WSL 配置时建议传入 `\\wsl.localhost\<发行版>\home\<用户>\.codex\config.toml` 这类 UNC 路径；也可以直接在 WSL 中执行命令。

## WSL 访问 Windows 上的网关

有两种方式，任选一种。

### 方式 A：WSL 镜像网络（推荐）

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

### 方式 B：NAT 网络

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

## 桌面端自定义模型

Codex CLI 的 `--profile` 配置不会自动进入桌面端模型选择器。让 Claude 等非内置模型显示在桌面端，需要配置 `model_catalog_json`，完整流程见 [codex-desktop-custom-models.md](codex-desktop-custom-models.md)。

## 相关文档

- [README.md](../README.md)：构建、启动和最小配置
- [configuration.md](configuration.md)：provider、鉴权和上游兼容项
- [architecture.md](architecture.md#codex-配置同步)：配置同步的实现细节
