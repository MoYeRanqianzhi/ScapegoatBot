# ScapegoatBot 0.2

一个 QQ Agent：代替主人、像真人一样回复消息。守护进程通过 NapCat（OneBot 11）收发
QQ 消息，用可配置的 LLM 生成回复，并把所有状态集中在本机 `~/.sbot`。

> 0.2 是完全重写的版本，旧的 0.1 代码与数据都没有迁移。
> 说明书在二进制里，与它同一个版本：`sbot docs` 列出全部，`sbot docs sbot` 从头讲起。同一份内容在
> 仓库的 [skills/](skills/) 下，也可以用 `sbot docs --export <目录>` 导出，直接装成 agent skill。

## 能力

- **拟人回复**：静态提示词 + owner context + `/USER.md` + 记忆索引 + 风格画像 +
  技能索引；运行时上下文每回合单独注入。
- **工具**：11 个文件工具（Read / Write / Edit / LS / Grep / Glob / RM / MV / CP /
  Mkdir / Stat）、`API`（QQ 动作，以及下载、图片处理等本地动作）、`Skill`、
  `WebSearch` / `WebFetch`、`Monitor`、`Agent`（子代理）、`MCP`（可选）。记忆是 VFS 里的
  普通文件，agent 用文件工具读写。
- **子代理**：`general` / `explore` / `plan` / `docs` 内置类型 + `/agents/*.md`
  自定义定义；独立上下文、工具集强制为父代理子集、支持嵌套与 monitor 绑定。
- **动态视图**：`/messages`、`/inbox`、`/sessions`、`/friends`、`/groups`、
  `/config`、`/system` 由运行时生成，不落盘，支持 `filter` / `limit` / `offset`。
- **安全闸**：输入/输出两层规则 + 分类器（Jev 或主模型）；`shadow` 只记录，
  `enforce` 命中即永久 `hold`，直到用真实上下文复核通过。
- **动作门禁 `[gate]`**：每次 `napcat.*` 调用都过速率、重复/跨群/@all 规则，
  可选 LLM 审查；`approve` 模式下变更动作需主人在 CLI 确认（`master_qq` 是另一个账号时
  也可私聊回复确认码）。
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

前置：NapCatQQ（OneBot 11 WebSocket）、一个兼容的 LLM API。

预编译版本在 [Releases](https://github.com/MoYeRanqianzhi/ScapegoatBot/releases)：Linux（glibc 与
全静态的 musl）、Windows、macOS，x86_64 与 aarch64 都有，解开就能用；该下哪个文件见
[skills/sbot-deploy/SKILL.md](skills/sbot-deploy/SKILL.md)。从源码构建需要 Rust 1.89+（edition 2024；
`Cargo.toml` 的 `rust-version`）：

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

完整的部署步骤（NapCat、凭据、服务管理器、升级与换机器）：`sbot docs sbot-deploy`，即
[skills/sbot-deploy/SKILL.md](skills/sbot-deploy/SKILL.md)。

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

所有可用键、默认值与 `models.json` / `auth.json` 格式：`sbot docs sbot-deploy configuration`
（[configuration.md](skills/sbot-deploy/references/configuration.md)）。写全了每个默认值的样例：
`sbot docs sbot-deploy sbot.example.toml`
（[sbot.example.toml](skills/sbot-deploy/references/sbot.example.toml)，由
`tests/config_example.rs` 校验）。

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
| `sbot ai start\|stop` | 只控制 LLM 调用；消息接收与存储不中断 |
| `sbot chat "<text>" [--scope group:<gid>\|private:<qq>]` | 从控制通道给 agent 一句话；`--scope` 指明针对的会话 |
| `sbot pause` / `resume` | 写 / 删除 `ESTOP`，暂停新回合 |
| `sbot security status\|resume` | 查看 hold 与最近安全事件 / 重新过闸 |
| `sbot gate pending\|approve\|reject` | `approve` 模式的待确认动作与裁决 |
| `sbot model set <provider/id>` / `list` | 热切换主模型（下一回合生效）/ 列出白名单模型与是否已定义 |
| `sbot effort set <level>` / `list` | 热切换 effort / 列出当前模型支持的级别 |
| `sbot backup [--output DIR]` | SQLite 快照 + 文件归档；**包含主密钥与 `auth.json`（明文 API key）** |
| `sbot doctor` | 只读诊断，不修复数据 |
| `sbot docs [<skill> [<reference>]]` / `--export <DIR>` | 列出、打印或导出内嵌的说明书 |

`--config <PATH>` 可指定另一份 `sbot.toml`；`backup` / `doctor` / `start` 直接读本地
文件，不需要守护进程在跑；`docs` 连 `~/.sbot` 都不需要。

## 安全模型

- **两层内容闸**：输入与输出各过一遍；规则层先行，Jev 或主模型分类器补充。
  `enforce` 命中后写 `SECURITY_HOLD`，仅 `napcat.*` 被拒绝；Web/MCP、文件以及
  `local.*`/`agent.*`/`api.*` 仍可用。默认 `shadow`（只记录）；启用 `enforce` 前请配好分类器
  或显式 `backend = "off"`。
- **动作门禁**：`trusted` / `auto` / `approve`；`auto` 拦截速率超限、同 chat 重复、
  跨群同文与无 owner intent 的 @全体；`approve` 通过 `sbot gate` 一次性批准。
- **恢复**：`agent.resume` / `sbot security resume` 复核当前上下文，干净才解除；被判的
  原文可以先 `agent.quarantine` 隔离。没有超时与自动恢复。
- **主人保护**：针对 `owner_qq` / `master_qq` 的踢人、禁言等 destructive action
  无条件拒绝（目标 QQ 接受数字或字符串）。
- **refusal 不重试**：provider 返回 `stop_reason = refusal` 时进入 hold，不换模型、不重发。
- **加密边界**：`~/.sbot/key` 与数据一起备份；仅拿到 `data.db` / `messages/` /
  `sessions/` 无法解密。
- **威胁边界**：不防同机管理员、不防进程内存读取。

细节与未实现项：`sbot docs sbot-operate security`
（[security.md](skills/sbot-operate/references/security.md)）与 `sbot docs sbot-operate limits`
（[limits.md](skills/sbot-operate/references/limits.md)）。

## 隐私与已知限制

- 只有一条 transcript：整个 QQ 号一份，跨聊天上下文会互相可见。
- **后台复盘**：空闲时从 transcript 提取记忆候选（只追加、来源固定 `self`）并更新
  `/STYLE.md`；技能 curator 定期把长期未用技能归档到 `/skills/.archive/`，不删除。
- 子代理支持嵌套 spawn（受 `[swarm].max_depth` 限制）；`Monitor add` 的
  `deliver.to = "<子代理 id>"` 可把事件投给该子代理实例。
- `/config` 写入支持白名单键（`[filter]`、`[agent].available_models` /
  `thinking_level`、`[compaction]`、`[skills]`、`[queue]`、`[style]`），其余键只读；
  写入会校验并原子落盘，但只有 `[filter]` 与 `[queue]` 立即热更新
  （`max_concurrent` 的信号量上限需重启）。
- 尚未提供旧 0.1 数据迁移；0.2 从空存储开始。
- 已知边界（审批通道、启发式规则、WebFetch 后端抓取、群级动作保护等）见
  [limits.md](skills/sbot-operate/references/limits.md)。

## 测试

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

CI（GitHub Actions，`.github/workflows/ci.yml`）在 Linux / Windows / macOS 的 x86_64 与 aarch64 上
跑这三项，另外还有 musl 上的测试、最低 Rust 版本、rustdoc、依赖审计（`deny.toml`）、未使用的
依赖与 workflow 自身的检查。

真实 API 测试默认忽略，需要显式环境变量：

```bash
SBOT_LIVE_BASE_URL=... SBOT_LIVE_API_KEY=... SBOT_LIVE_MODEL=... \
  cargo test --test live_providers -- --ignored
```

`SBOT_LIVE_JEV_BASE_URL` / `SBOT_LIVE_JEV_API_KEY` 用于真实 Jev 分类器测试。
`tests/onebot_mock_client.rs` 与 `tests/napcat_api.rs` 对本地模拟的 OneBot 服务端运行，
不需要 NapCat，随 `cargo test` 一起跑；真实 NapCat 的验证步骤见
[.agents/memory/live-testing.md](.agents/memory/live-testing.md)。

## 目录

```text
src/
  main.rs          薄入口
  app/             守护进程装配（assembly）、IPC 处理、QQ 管线（events）、后台维护
  agent/           循环、消息/事件、回合队列、runner
  providers/       Anthropic / Chat / Responses、SSE、fallback、计量
  llm/             与协议无关的用量与停止原因
  security/        内容闸、Jev/llm 分类器、周期检查、动作门禁、hold、审计
  qq/              事件归一化、格式、过滤、发送账本、重连
  session/         分段、turn buffer、压缩
  vfs/             路径、查询串与 filter、SQLite 后端、配额
  tools/           Tool trait、文件/API/记忆/技能/下载/Web/Monitor/Agent
  tools/views/     动态视图：messages/inbox/sessions/contacts/config/system
  actions.rs       API action 注册表
  storage/         迁移、加密、消息归档
  monitor/         when 谓词、配置 → 规格、投递、timer/stream/internal 源
  swarm/           子代理（内置类型与 /agents 自定义定义）
  prompt/          系统提示与上下文注入
  memory/          记忆复盘
  hooks.rs         [[hooks.events]] 求值
  budget.rs        每日 / 每回合预算
  mcp/             MCP stdio 客户端
  media.rs         图片解码上限与缩图
  ipc/             命名管道 / UDS
  cli/             命令、备份、诊断
  config/          根结构 + sections/ 分节 schema
skills/            说明书：sbot、sbot-deploy、sbot-use、sbot-operate；编译时内嵌进二进制（`sbot docs`）
.agents/           给开发者（人与 agent）的记忆、计划与 TODO；本项目没有另外的开发文档
```

## License

MIT，见 [LICENSE](LICENSE)。
