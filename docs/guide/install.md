# 安装与首次启动

## 前置

| 依赖 | 要求 | 说明 |
|------|------|------|
| Rust | 1.85+（edition 2024） | 用 `rustup` 安装 stable 即可 |
| NapCatQQ | 支持 OneBot 11 的版本 | 提供 WebSocket 接口 |
| LLM API | Anthropic Messages / OpenAI Chat Completions / OpenAI Responses 兼容 | 见 `models.json` |

不需要 Node、Python 或数据库服务：状态全部在 `~/.sbot`，SQLite 由程序内置。

## 构建

```bash
git clone https://github.com/MoYeRanqianzhi/ScapegoatBot
cd ScapegoatBot
cargo build --release
```

产物是 `target/release/sbot`。下面用 `sbot` 代指它。

## NapCat

1. 按 NapCat 文档登录 QQ。
2. 打开 OneBot 11 的 WebSocket 服务（反向或正向均可）。正向示例：
   - 地址：`ws://127.0.0.1:3001`
   - 若设置了 access token，填到 `~/.sbot/auth.json` 的 `napcat.access_token`。
3. 确认 NapCat 与 QQ 都处于登录状态。

ScapegoatBot 只作为 WebSocket 客户端连接，不修改 NapCat 配置。

## 首次启动

```bash
./target/release/sbot start
```

首次启动会：

- 创建 `~/.sbot/`（`SBOT_HOME` 可改位置）；
- 写入默认 `sbot.toml`（已存在则不动）；
- 生成 `~/.sbot/key`（32 字节主密钥，文件权限 0600）；
- 创建 `~/.sbot/data/data.db` 并跑迁移；
- 写入 VFS 种子：`/AGENTS.md`、`/MEMORY.md`、`/USER.md`、`/memory/*`、
  `/agents`、`/skills`、`/trash`、`/tmp`。

NapCat 没连上时守护进程照常启动，只是 QQ 动作会失败；日志会写
`NapCat is not connected`。模型没配好同理：进程在，但不回复。

`Ctrl-C` 停止；也可以 `sbot stop`。

## 目录布局

```text
~/.sbot/
  sbot.toml           主配置（零凭据）
  models.json         模型清单
  auth.json           凭据（只在本机，不挂载 VFS）
  key                 主密钥（0600）
  ESTOP               存在则暂停新回合
  SECURITY_HOLD       存在则处于 hold
  data/
    data.db           SQLite 索引（明文）
    messages/         加密的消息 JSONL
    sessions/         加密的 transcript 分段
    blobs/            加密的二进制分块
  logs/
  run/                单实例锁与 PID
```

**迁移**：停掉进程，复制整个 `~/.sbot` 到新机器即可。只拿到 `data.db` 或
`messages/` 而没有 `key` 文件无法解密。

## 多实例

```bash
SBOT_HOME=/srv/sbot-b ./target/release/sbot start
SBOT_HOME=/srv/sbot-b ./target/release/sbot status
```

每个实例有独立的目录、IPC 端点与单实例锁。同一 `SBOT_HOME` 不能同时跑两个进程。

## 第一次验证

```bash
# 1. 连接与模型
sbot status

# 2. 让 agent 回一句（走控制通道，不需要 QQ 消息）
sbot chat "Reply with exactly: live-ok"

# 3. 看是否落盘
ls ~/.sbot/data/sessions/
```

在 QQ 里给账号发一条私聊，若 `sbot status` 的 hold 为 false、日志出现
`turn completed`，说明链路通了。

## 升级

```bash
git pull
cargo build --release
sbot stop
sbot start          # 启动时自动跑新的数据库迁移
```

数据库迁移只追加、不修改已发布条目；跨版本降级不保证兼容。升级前建议
`sbot backup --output <目录>`。
