# ScapegoatBot 用户指南

ScapegoatBot 是一个跑在本机的 QQ Agent：它连接 NapCat（OneBot 11），用你配置的模型，
代替你（或作为独立账号）在群里和私聊里像真人一样回复。

这份指南面向**使用者**，按“装好 → 配好 → 用起来 → 出问题怎么查”组织。
想改代码或了解设计，看 [开发文档](../development/README.md) 与
[重构设计](../../.agents/plan/2026-10-28-rebuild-design.md)。

## 目录

| 文档 | 内容 |
|------|------|
| [install.md](install.md) | 安装 Rust、构建、NapCat 连接、首次启动 |
| [configuration.md](configuration.md) | `sbot.toml` / `models.json` / `auth.json` 全量配置 |
| [usage.md](usage.md) | CLI、日常使用、热切换、备份与诊断 |
| [persona-memory.md](persona-memory.md) | 人设、`/AGENTS.md`、`/USER.md`、记忆与技能 |
| [tools-and-vfs.md](tools-and-vfs.md) | 虚拟文件系统、动态视图、19 个内置工具 |
| [security.md](security.md) | 输入/输出闸、hold、恢复、主人保护、威胁边界 |
| [troubleshooting.md](troubleshooting.md) | 常见故障与排查命令 |
| [limits.md](limits.md) | 当前版本明确不支持的项 |

## 五分钟路径

```bash
# 1. 构建
cargo build --release

# 2. 首次启动（会在 ~/.sbot 生成默认配置并等待 NapCat）
./target/release/sbot start

# 3. 另开一个终端：填入模型 key
$EDITOR ~/.sbot/auth.json

# 4. 查看状态
./target/release/sbot status
```

最小可用的 `~/.sbot/sbot.toml`：

```toml
version = 1
[agent]
model = "anthropic/claude-opus-5-5"
available_models = ["anthropic/claude-opus-5-5"]

[napcat]
url = "ws://127.0.0.1:3001"
```

`~/.sbot/auth.json`：

```json
{
  "providers": { "anthropic": { "api_key": "sk-ant-..." } }
}
```

`models.json` 不是必须的：内置清单（`assets/models.default.json`）会自动加载；
需要新增 provider/模型时再创建 `~/.sbot/models.json`，它会与内置清单合并。

## 三个核心概念

- **回合（turn）**：一条或多条 QQ 消息触发的一次模型对话。同一 chat 的连发会在
  `[queue].debounce_ms`（默认 3 秒）内合并。
- **VFS**：agent 看到的文件系统。`/AGENTS.md`、`/USER.md`、`/MEMORY.md` 是文件，
  `/messages`、`/inbox`、`/sessions`、`/config`、`/system` 是运行时生成的动态视图。
- **hold**：安全闸判定危险后进入的停机状态。此时所有 QQ 动作被拒绝，直到 agent
  用当前上下文重新过闸（`agent.resume` 或 `sbot security resume`）。

## 一个可靠的部署顺序

1. NapCat 先跑通，OneBot 11 WebSocket 可连接。
2. 用 `sbot start` 启动守护进程，确认 `sbot status` 显示模型可用。
3. 先在私聊或小号群里试，观察 `/inbox` 与日志。
4. 配好 `[filter]` 与 `send_scopes`，再接到真实群。
5. 定期 `sbot backup --output <目录>`；备份包含主密钥，按机密文件保管。
