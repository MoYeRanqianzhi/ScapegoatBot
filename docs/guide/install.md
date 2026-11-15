# 安装与首次启动

## 前置

| 依赖 | 要求 | 说明 |
|------|------|------|
| Rust | 1.89+（edition 2024） | 只在从源码构建时需要：用 `rustup` 安装 stable 即可；最低版本见 `Cargo.toml` 的 `rust-version` |
| NapCatQQ | 支持 OneBot 11 的版本 | 提供 WebSocket 接口 |
| LLM API | Anthropic Messages / OpenAI Chat Completions / OpenAI Responses 兼容 | 见 `models.json` |

不需要 Node、Python 或数据库服务：状态全部在 `~/.sbot`，SQLite 由程序内置。

## 下载预编译版本

[Releases](https://github.com/MoYeRanqianzhi/ScapegoatBot/releases) 里每个版本有 20 个文件，名字是
`sbot-<版本>-<平台>.tar.gz`（Windows 是 `.zip`），解开后是一个同名目录，里面是 `sbot` 本体、
LICENSE 与 README。

| 系统 | 平台 | 说明 |
|------|------|------|
| Linux，glibc ≥ 2.28（Debian 10+、Ubuntu 20.04+、RHEL / Rocky / Alma 8+、Amazon Linux 2023、Fedora、Arch、openSUSE 等） | `x86_64-unknown-linux-gnu`、`aarch64-unknown-linux-gnu` | 首选 |
| 任何 Linux（包括 Alpine、glibc 更旧的系统） | `x86_64-unknown-linux-musl`、`aarch64-unknown-linux-musl` | 全静态，不依赖任何系统库 |
| Windows 10 / 11 | `x86_64-pc-windows-msvc`、`aarch64-pc-windows-msvc` | 不需要安装 VC++ 运行库 |
| macOS 11+ | `aarch64-apple-darwin`（Apple 芯片）、`x86_64-apple-darwin`（Intel） | |

x86_64 的每种平台按 CPU 等级各出一份，等级写在架构后面（`x86_64_v3-unknown-linux-gnu`）；等级
越高，编译器能用的指令越新、越快：

- 不带后缀（v1）：任何 x86-64 CPU；
- `_v2`：2009 年以后的 CPU（SSE4.2、POPCNT）；
- `_v3`：Intel Haswell（2013）及以后、AMD Zen 及以后（AVX2、BMI2、FMA）；
- `_v4`：支持 AVX-512 的 CPU。

不确定就先下不带后缀的版本，运行 `sbot --version`：`cpu:` 一行是这台机器能用的最高等级，再换成
对应的文件。选高了，程序一启动就以“非法指令”（Illegal instruction）退出，不会有别的影响。

校验下载的文件（第二条核对它确实由本仓库的 release workflow 从 tag 对应的提交构建）：

```bash
sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify sbot-<版本>-<平台>.tar.gz --repo MoYeRanqianzhi/ScapegoatBot
```

## 从源码构建

```bash
git clone https://github.com/MoYeRanqianzhi/ScapegoatBot
cd ScapegoatBot
cargo build --release
```

产物是 `target/release/sbot`。下面用 `sbot` 代指它（预编译版本就是解出来的那个文件）。

## NapCat

1. 按 NapCat 文档登录 QQ。
2. 在 NapCat 的 OneBot 11 网络配置里添加一个 **WebSocket 服务器**（正向 WS，
   配置项 `network.websocketServers`）。sbot 只作为客户端去连接它，NapCat 的
   “WebSocket 客户端”（反向 WS）连不上 sbot。示例：
   - 地址：`ws://127.0.0.1:3001`（填到 `sbot.toml` 的 `[napcat].url`）；
   - `messagePostFormat` 保持默认的 `array`；
   - 打开 `reportSelfMessage`：stand_in 下主人在手机上手打的消息只能这样被 agent 看到；
     agent 自己发的消息会被发送账本认出来，不会被当成主人的话；
   - 若设置了 `token`，填到 `~/.sbot/auth.json` 的 `napcat.access_token`。
3. 确认 NapCat 与 QQ 都处于登录状态。

sbot 不修改 NapCat 配置。

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

NapCat 没连上（或 QQ 还没登录）时守护进程会一直等：控制通道可用，`sbot status` 的
`agent.state` 是 `waiting_for_napcat`，日志每次重试写一条 `waiting for NapCat`；连上之后 agent
才启动。模型没配好时进程在但不回复（`agent.state` 是 `no_provider`）。

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
