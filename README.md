# ScapegoatBot 0.2

一个 QQ Agent：代替主人、像真人一样回复消息。守护进程通过 NapCat（OneBot 11）收发 QQ 消息，用可配置的 LLM 生成回复，并把所有状态集中在本机 `~/.sbot`。

> 0.2 是完全重写的版本。旧的 0.1 代码没有迁移，重构量化见
> [docs/development/refactor-report.md](docs/development/refactor-report.md)。

## 能力

- **拟人回复**：静态提示词 + owner context + `/USER.md` + 记忆索引 + 风格画像 + 技能索引；运行时上下文每回合单独注入。
- **工具**：11 个文件工具（读/写/编辑/删除/移动/复制/建目录/信息/Glob/Grep）、`API`（NapCat action）、`Memory`、`Skill`、`Download`、`WebSearch` / `WebFetch`、`Monitor`、`Agent`（子代理）、`MCP`（可选）。
- **子代理**：`general` / `explore` / `plan` / `docs` 四种内置类型 + `/agents/*.md` 自定义定义；独立上下文、工具集强制为父代理子集、并发上限。当前为单层，不支持嵌套 spawn 与 monitor 绑定。
- **MCP**：可选、默认关闭；stdio JSON-RPC，自动发现 `tools/list`，通过单个 `MCP` 工具调用。
- **动态视图**：`/messages`、`/inbox`、`/sessions`、`/friends`、`/groups`、`/config`、`/system` 由运行时生成，不落盘。
- **安全闸**：规则层（凭据形状、提示注入）+ Jev 分类器；命中即进入永久 `hold`，直到 agent 用当前上下文重新过闸。
- **准入过滤**：`[filter]` 是拒收门。被拒消息不写 JSONL、不进 inbox、不进 transcript。
- **存储加密**：XChaCha20-Poly1305，密钥在 `~/.sbot/key`；SQLite 索引明文，内容加密；blob 按 1 MiB 分块。
- **守护进程**：`sbot start` 前台运行；其余命令通过 Windows 命名管道 / Unix UDS 控制。
- **备份与诊断**：`sbot backup` 在线一致性快照；`sbot doctor` 只读对账。

## 快速开始

### 前置

- Rust 1.99+（edition 2024）
- [NapCatQQ](https://github.com/NapNeko/NapCatQQ)，提供 OneBot 11 WebSocket
- 一个 Anthropic Messages / OpenAI Chat Completions / OpenAI Responses 兼容的 LLM API

### 构建与运行

```bash
cargo build --release
./target/release/sbot start
```

首次启动会创建 `~/.sbot/sbot.toml`、`~/.sbot/key`、`~/.sbot/data/data.db`，并写入 VFS 种子（`/AGENTS.md`、`/MEMORY.md`、`/USER.md`、`/memory/*`、`/agents`、`/trash`、`/tmp`）。
NapCat 未连接时守护进程照常启动，只是 QQ 工具会失败。

用 `SBOT_HOME` 指定其他根目录（测试与多实例隔离）：

```bash
SBOT_HOME=/tmp/sbot ./target/release/sbot start
```

### 凭据

所有凭据只放在 `~/.sbot/auth.json`，**明文优先**，也可以写 `"$ENV_NAME"` 做可选插值。`sbot.toml` 中没有任何凭据，`auth.json` 不挂载 VFS。

```json
{
  "providers": {
    "anthropic": { "api_key": "sk-..." },
    "deepseek": { "api_key": "sk-..." }
  },
  "napcat": { "access_token": "" },
  "security": { "guard": { "api_key": "sk-..." } }
}
```

缺少引用到的 provider key 时启动会告警，模型不可用。

### 配置

`sbot.toml` 是严格 schema：`version = 1`，未知字段直接拒绝。最小可用配置：

```toml
version = 1

[agent]
model = "anthropic/claude-opus-5-5"
available_models = ["anthropic/claude-opus-5-5", "anthropic/claude-haiku-5-5"]

[napcat]
url = "ws://127.0.0.1:3001"
account_qq = 10000

[filter]
# 留空表示接收全部；规则语义是“拒收”，不是静默。
```

环境变量可覆盖任意键：`SBOT__AGENT__MODEL=anthropic/claude-haiku-5-5`。

## 命令

| 命令 | 作用 |
|------|------|
| `sbot start` | 前台运行守护进程 |
| `sbot stop` / `restart` | 停止 / 重启 |
| `sbot status` | 运行状态、模型、hold、用量 |
| `sbot ai start\|stop\|restart` | 只控制 LLM 调用；消息接收与存储不中断 |
| `sbot chat "<text>" [--scope ...]` | 从控制通道给 agent 一句话 |
| `sbot pause` / `resume` | 写 / 删除 `ESTOP`，暂停新回合 |
| `sbot security status\|resume` | 查看 hold 与最近安全事件 / 请求重新过闸 |
| `sbot model set <provider/id>` / `list` | 热切换主模型（下一回合生效） |
| `sbot effort set <level>` / `list` | 热切换 effort |
| `sbot backup [--output DIR]` | SQLite 快照 + 文件归档；**包含主密钥** |
| `sbot doctor` | 只读诊断，不修复数据 |

## 安全模型

- **两步闸**：输入与输出各过一遍；规则层先行，Jev 分类器补充。命中后写 `SECURITY_HOLD`，所有 `napcat.*` 动作被拒绝，只有 `agent.*` 自管理可用。
- **主人保护**：针对 `owner_qq` / `master_qq` 的封禁、踢出、禁言、删好友无条件拒绝。
- **refusal 不重试**：provider 返回 `stop_reason = refusal` 时进入 hold，不换模型、不重发。
- **加密边界**：`~/.sbot/key` 与数据一起备份；仅拿到 `data.db` / `blobs/` / `sessions/` 无法解密。
- **威胁边界**：不防同机管理员、不防进程内存读取。

## 隐私与已知限制

- `[session].scope` 默认 `shared`：整个 QQ 号一份 transcript，跨聊天上下文会互相可见。需要强隔离时切换到 `per_chat`：每个 chat 一条独立 transcript 链（文件名带 `group-<id>-` / `private-<id>-` 前缀），压缩、轮转、保留、崩溃恢复均按 chat 执行。
- **后台复盘**：空闲时从 transcript 提取记忆候选（只追加、来源固定 `self`）并更新 `/STYLE.md`；技能 curator 定期把长期未用技能归档到 `/skills/.archive/`，不删除。
- 子代理支持嵌套 spawn（受 `[swarm].max_depth` 限制），`Agent message` 续聊复用原定义的提示与工具集；`Agent spawn` 的 `monitor` 参数可把事件绑定到该子代理实例，`deliver.to = <spawn_id>` / `agent:<name>` 也支持。
- `/config` 写入已支持白名单键（`[filter]`、`[agent].available_models` / `thinking_level`、`[compaction]`、`[skills]`、`[queue]`、`[style]`），其余键只读；写入会校验并原子落盘，但 `[filter]` 与 `[queue]` 写入立即热更新（`max_concurrent` 的信号量上限需重启）。
- 尚未提供旧 0.1 数据迁移；0.2 从空存储开始。
- `tools::web` 的搜索/抓取依赖用户配置的端点，不内置官方地址。

## 测试

```bash
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

真实 API 测试默认忽略，需要显式环境变量：

```bash
SBOT_LIVE_BASE_URL=... SBOT_LIVE_API_KEY=... SBOT_LIVE_MODEL=... \
  cargo test --test live_providers -- --ignored
```

`SBOT_LIVE_JEV_BASE_URL` / `SBOT_LIVE_JEV_API_KEY` 用于真实 Jev 分类器测试。

## 目录

```
src/
  main.rs        薄入口
  app.rs         守护进程装配、IPC、QQ 管线
  agent/         循环、消息/事件、回合队列、runner
  providers/     Anthropic / Chat / Responses、SSE、fallback
  security/      规则层、Jev、hold、审计
  qq/            事件归一化、格式、过滤、重连
  session/       分段、turn buffer、压缩
  vfs/           路径、SQLite 后端、配额、动态视图契约
  tools/         Tool trait、文件/API/记忆/技能/下载/Web/Monitor/视图
  storage/       迁移、加密、消息归档
  monitor/       事件源、cron、投递
  ipc/           命名管道 / UDS
  cli/           命令、备份、诊断
```

开发文档见 [docs/development/README.md](docs/development/README.md)。

## License

MIT，见 [LICENSE](LICENSE)。
