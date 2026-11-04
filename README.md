# ScapegoatBot 0.2

一个 QQ Agent：代替主人、像真人一样回复消息。守护进程通过 NapCat（OneBot 11）收发
QQ 消息，用可配置的 LLM 生成回复，并把所有状态集中在本机 `~/.sbot`。

> 0.2 是完全重写的版本，旧的 0.1 代码没有迁移；重构量化见
> [docs/development/refactor-report.md](docs/development/refactor-report.md)。
> 使用说明见 [docs/guide/](docs/guide/README.md)。

## 能力

- **拟人回复**：静态提示词 + owner context + `/USER.md` + 记忆索引 + 风格画像 +
  技能索引；运行时上下文每回合单独注入。
- **工具**：11 个文件工具（Read / Write / Edit / LS / Grep / Glob / RM / MV / CP /
  Mkdir / Stat）、`API`（QQ 动作）、`Memory`、`Skill`、`Download`、`WebSearch` /
  `WebFetch`、`Monitor`、`Agent`（子代理）、`MCP`（可选）。
- **子代理**：`general` / `explore` / `plan` / `docs` 内置类型 + `/agents/*.md`
  自定义定义；独立上下文、工具集强制为父代理子集、支持嵌套与 monitor 绑定。
- **动态视图**：`/messages`、`/inbox`、`/sessions`、`/friends`、`/groups`、
  `/config`、`/system` 由运行时生成，不落盘，支持 `filter` / `limit` / `offset`。
- **安全闸**：输入/输出两层规则 + 分类器（Jev 或主模型）；`shadow` 只记录，
  `enforce` 命中即永久 `hold`，直到用真实上下文复核通过。
- **动作门禁 `[gate]`**：每次 `napcat.*` 调用都过速率、重复/跨群/@all 规则，
  可选 LLM 审查；`approve` 模式下变更动作需主人在 CLI 或私聊 nonce 确认。
- **owner intent**：跨 chat 发送、加好友、改资料等扩张动作必须由 runtime 证据支持；
  同 chat 回复不需要。
- **周期安全检查**：每 `check_every_tokens` 用独立模型检查注入/社工与是否已照做；
  `high + complied` 暂停外发，`critical` 暂停新回合。
- **事件驱动**：默认所有 QQ 消息只进 `/inbox`，不逐条唤醒；内置可删除的
  `wake-on-mention`（非主人 @）与 `wake-on-owner`（主人消息）预设，
  其余消息由 agent 主动读取。
- **存储加密**：XChaCha20-Poly1305，密钥在 `~/.sbot/key`；SQLite 索引明文，
  内容加密；blob 按 1 MiB 分块。
- **守护进程**：`sbot start` 前台运行；其余命令通过 Windows 命名管道 / Unix UDS 控制。
- **备份与诊断**：`sbot backup` 在线一致性快照；`sbot doctor` 只读对账。

## 快速开始

前置：Rust 1.85+（edition 2024）、NapCatQQ（OneBot 11 WebSocket）、一个兼容的 LLM API。

```bash
cargo build --release
./target/release/sbot start
```

首次启动会创建 `~/.sbot/sbot.toml`、`~/.sbot/key`、`~/.sbot/data/data.db`，并写入
VFS 种子（`/AGENTS.md`、`/MEMORY.md`、`/USER.md`、`/memory/*`、`/agents`、
`/skills`、`/trash`、`/tmp`）。NapCat 或模型不可用时守护进程照常启动，只是不能收发。

用 `SBOT_HOME` 指定其他根目录（测试与多实例隔离）：

```bash
SBOT_HOME=/tmp/sbot ./target/release/sbot start
```

完整安装步骤见 [docs/guide/install.md](docs/guide/install.md)。

### 最小配置

`~/.sbot/sbot.toml` 是严格 schema（`version = 1`，未知字段直接拒绝）：

```toml
version = 1

[agent]
model = "anthropic/claude-opus-5-5"
available_models = ["anthropic/claude-opus-5-5"]

[napcat]
url = "ws://127.0.0.1:3001"

[filter]
# 留空表示接收全部；规则语义是“拒收”，不是静默。
```

所有可用键、默认值与 `models.json` / `auth.json` 格式见
[docs/guide/configuration.md](docs/guide/configuration.md)，可运行样例是仓库根目录的
[config.example.toml](config.example.toml)（由 `tests/config_example.rs` 校验）。

### 凭据

所有凭据只放在 `~/.sbot/auth.json`，**明文优先**；也可以写 `"$ENV_NAME"` 做可选插值
（只有值以 `$` 开头才展开）。`sbot.toml` 中没有任何凭据，`auth.json` 不挂载 VFS。

```json
{
  "providers": {
    "anthropic": { "api_key": "sk-..." }
  },
  "napcat": { "access_token": "" },
  "security": { "guard": { "api_key": "sk-..." } }
}
```

缺少引用到的 provider key 时启动会告警，对应模型不可用。

### 模型清单

`~/.sbot/models.json` 定义 provider、协议（`anthropic` / `openai` / `chat`）、地址与
模型参数；模型引用为 `provider/id`。文件不存在时使用内置清单
（`assets/models.default.json`）；创建后与内置清单合并。

## 命令

| 命令 | 作用 |
|------|------|
| `sbot start` | 前台运行守护进程 |
| `sbot stop` / `restart` | 停止 / 重启 |
| `sbot status` | 运行状态、模型、hold、用量 |
| `sbot ai start\|stop\|restart` | 只控制 LLM 调用；消息接收与存储不中断 |
| `sbot chat "<text>" [--scope ...]` | 从控制通道给 agent 一句话 |
| `sbot pause` / `resume` | 写 / 删除 `ESTOP`，暂停新回合 |
| `sbot security status\|resume` | 查看 hold 与最近安全事件 / 重新过闸 |
| `sbot gate pending\|approve\|reject` | `approve` 模式的待确认动作与裁决 |
| `sbot model set <provider/id>` / `list` | 热切换主模型（下一回合生效） |
| `sbot effort set <level>` / `list` | 热切换 effort |
| `sbot backup [--output DIR]` | SQLite 快照 + 文件归档；**包含主密钥** |
| `sbot doctor` | 只读诊断，不修复数据 |

`--config <PATH>` 可指定另一份 `sbot.toml`；`backup` / `doctor` / `start` 直接读本地
文件，不需要守护进程在跑。

## 安全模型

- **两层内容闸**：输入与输出各过一遍；规则层先行，Jev 或主模型分类器补充。
  `enforce` 命中后写 `SECURITY_HOLD`，所有 `napcat.*` 与 Download/Web/MCP 被拒绝，
  只有 `agent.*` 自管理可用。默认 `shadow`（只记录）；启用 `enforce` 前请配好分类器
  或显式 `backend = "off"`。
- **动作门禁**：`trusted` / `auto` / `approve`；`auto` 拦截速率超限、同 chat 重复、
  跨群同文与无 owner intent 的 @全体；`approve` 通过 `sbot gate` 一次性批准。
- **恢复**：`agent.resume` / `sbot security resume` 用真实被判内容重新过闸，通过才解除；
  没有超时与自动恢复。
- **主人保护**：针对 `owner_qq` / `master_qq` 的踢人、禁言等 destructive action
  无条件拒绝（目标 QQ 接受数字或字符串）。
- **refusal 不重试**：provider 返回 `stop_reason = refusal` 时进入 hold，不换模型、不重发。
- **加密边界**：`~/.sbot/key` 与数据一起备份；仅拿到 `data.db` / `messages/` /
  `sessions/` 无法解密。
- **威胁边界**：不防同机管理员、不防进程内存读取。

细节与未实现项见 [docs/guide/security.md](docs/guide/security.md) 与
[docs/guide/limits.md](docs/guide/limits.md)。

## 隐私与已知限制

- `[session].scope` 默认 `shared`：整个 QQ 号一份 transcript，跨聊天上下文会互相
  可见。需要强隔离时切换到 `per_chat`：每个 chat 一条独立 transcript 链，压缩、轮转、
  保留、崩溃恢复均按 chat 执行。
- **后台复盘**：空闲时从 transcript 提取记忆候选（只追加、来源固定 `self`）并更新
  `/STYLE.md`；技能 curator 定期把长期未用技能归档到 `/skills/.archive/`，不删除。
- 子代理支持嵌套 spawn（受 `[swarm].max_depth` 限制）；`Agent spawn` 的 `monitor`
  参数可把事件绑定到该子代理实例。
- `/config` 写入支持白名单键（`[filter]`、`[agent].available_models` /
  `thinking_level`、`[compaction]`、`[skills]`、`[queue]`、`[style]`），其余键只读；
  写入会校验并原子落盘，但只有 `[filter]` 与 `[queue]` 立即热更新
  （`max_concurrent` 的信号量上限需重启）。
- 尚未提供旧 0.1 数据迁移；0.2 从空存储开始。
- 已知边界（审批通道、启发式规则、WebFetch 后端抓取、群级动作保护等）见
  [docs/guide/limits.md](docs/guide/limits.md)。

## 测试

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

CI（GitHub Actions）运行同样的三项检查。

真实 API 测试默认忽略，需要显式环境变量：

```bash
SBOT_LIVE_BASE_URL=... SBOT_LIVE_API_KEY=... SBOT_LIVE_MODEL=... \
  cargo test --test live_providers -- --ignored
```

`SBOT_LIVE_JEV_BASE_URL` / `SBOT_LIVE_JEV_API_KEY` 用于真实 Jev 分类器测试；
`tests/live_napcat.rs` 需要真实 NapCat。

## 目录

```text
src/
  main.rs          薄入口
  app.rs           守护进程装配、IPC、QQ 管线、工具装配
  agent/           循环、消息/事件、回合队列、runner
  providers/       Anthropic / Chat / Responses、SSE、fallback
  security/        内容闸、Jev/llm 分类器、周期检查、动作门禁、hold、审计
  qq/              事件归一化、格式、过滤、重连
  session/         分段、turn buffer、压缩
  vfs/             路径、SQLite 后端、配额、动态视图契约
  tools/           Tool trait、actions 注册表、文件/API/记忆/技能/下载/Web/Monitor
  tools/views/     动态视图：messages/inbox/sessions/contacts/config/system
  storage/         迁移、加密、消息归档
  monitor/         事件源、cron、投递
  ipc/             命名管道 / UDS
  cli/             命令、备份、诊断
  config/          根结构 + sections/ 分节 schema
docs/
  guide/           用户指南
  development/     开发文档（架构入口、实机测试、提示词维护）
```

## License

MIT，见 [LICENSE](LICENSE)。
