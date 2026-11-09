# 配置

三份文件，职责分开：

| 文件 | 内容 | 是否含凭据 |
|------|------|-----------|
| `~/.sbot/sbot.toml` | 行为配置，严格 schema | 否 |
| `~/.sbot/models.json` | provider、协议、模型参数 | 否 |
| `~/.sbot/auth.json` | 所有 API key / token | 是（本机明文） |

三份文件都支持 `~/.sbot` 逻辑根；用 `SBOT_HOME` 改变根目录时，路径跟着变。

## 加载与覆盖

1. 内置默认值；
2. `sbot.toml`（未知字段直接报错，`version` 必须为 `1`）；
3. 环境变量 `SBOT__SECTION__KEY`，例如：

```bash
SBOT__AGENT__MODEL=anthropic/claude-haiku-5-5
SBOT__NAPCAT__URL=ws://127.0.0.1:3002
```

`models_file` / `auth_file` 可以在 `sbot.toml` 里改成绝对路径或相对配置文件目录的路径。

## sbot.toml

完整可运行样例见仓库根目录的 `config.example.toml`。下面按节说明常用键与默认值。

### `[agent]`

| 键 | 默认 | 说明 |
|----|------|------|
| `name` | `ScapegoatBot` | 显示名 |
| `mode` | `stand_in` | `stand_in`（用主人账号）或 `companion`（独立账号） |
| `owner_qq` | 0 | companion 必填；stand_in 留 0 表示用登录账号 |
| `owner_name` | 空 | 称呼主人的名字 |
| `model` | `anthropic/claude-opus-5-5` | 主模型，`provider/id` |
| `available_models` | 见默认 | 白名单；不在其中的模型不可用 |
| `fallback_models` | `[]` | 主模型失败后的回退链 |
| `thinking_level` | `high` | 与模型声明的 `thinking_levels` 校验 |
| `compaction_model` | 空 | 摘要模型；空 = 主模型 |
| `max_output_tokens` | 8192 | 单次回复上限 |
| `max_steps` | 64 | 单回合工具循环上限 |
| `master` | `self` | 报告与审批通知的目标：`self`（登录账号）或 QQ 号 |
| `receive_scopes` | `["*"]` | 允许读的 chat 列表 |
| `send_scopes` | `["*"]` | 允许发的 chat 列表；空 = 禁止一切发送 |
| `retry_attempts` / `retry_base_ms` / `retry_max_ms` | 3 / 400 / 30000 | provider 重试次数、基础间隔、上限 |
| `retry_mode` | `stepped` | `stepped` 阶梯退避（base、2×base…封顶 max）；`fixed` 固定 base |

`mode` 启动时固定，修改需重启。

#### 主人的消息

主人只通过 QQ 与这个无人值守的 agent 交互，但主人的消息没有特权通道：

- 本进程发出的消息才是 agent 自己的话（`self`）。每次发送都在发送账本里登记，NapCat 的
  `message_sent` 回声据此认领（`src/qq/ledger.rs`）；对不上时宁可当成 agent 的话。
- 其余消息——包括 stand_in 下主人在手机上用**同一个账号**手打的——都和别人的消息一样
  进 inbox，发送者记为本账号的 QQ（stand_in 下就是 `owner_qq`）。这需要 NapCat 打开
  `reportSelfMessage`。
- 是否因此醒来由 monitor 决定。默认装有 `wake-on-owner`，agent 可以修改或删除它；
  不想看主人的消息也是 agent 自己的选择。
- 触发批次里的非 `self` 消息全部来自 `owner_qq` 时，回合带 owner intent；混进别人的消息、
  或回合中途有别人的消息经 steering 进入，就没有（见 [安全模型](security.md#owner-intent-与发送)）。
  控制通道（`sbot chat`）恒带。

### `[napcat]`

| 键 | 默认 | 说明 |
|----|------|------|
| `url` | `ws://127.0.0.1:3001` | OneBot 11 WebSocket 地址 |

access token 在 `auth.json`，不在这里。

### `[filter]`：准入门

语义是**拒收**：命中的消息不写 JSONL、不进 inbox、不进 transcript，monitor 也看不到。
`ignore_groups` / `ignore_users` 同样作用于通知与请求（被忽略的群里的进群、撤回、戳一戳，
被忽略的人发来的好友/加群请求），它们也不进 `/system/events`；正则与 `require_mention`
只对有正文的消息生效。
主人的消息没有豁免：过滤器是 agent 划定自己活动范围的工具，把主人也排除在外同样是它的选择。
主人始终可以用控制通道（`sbot chat`），gate 确认码的私聊回复在过滤之前处理。

| 键 | 默认 | 说明 |
|----|------|------|
| `ignore_groups` | `[]` | 群号列表；这些群的消息直接丢弃 |
| `ignore_users` | `[]` | QQ 号列表 |
| `ignore_patterns` | `[]` | 正则；命中即拒收 |
| `require_mention` | false | 群里只有 @ 自己或回复自己时才接收 |

### `[queue]`（普通消息只进 inbox；唤醒预设走队列）

| 键 | 默认 | 说明 |
|----|------|------|
| `mode` | `steer` | 回合中到达的消息：`steer` 注入当前回合 / `followup` 回合结束后逐条各开一个回合 / `collect` 回合结束后再等 `debounce_ms`，合并成一个回合 |
| `max_concurrent` | 4 | 队列同时派发的 handler 数；shared transcript 下主回合仍由 turn_lock 串行 |
| `cap_messages` | 200 | 每 chat 待处理消息上限 |
| `drop` | `summarize` | 溢出策略：`summarize` / `old` / `new` |
| `debounce_ms` | 3000 | 连发合并窗口 |

`/config/queue` 写入会立即热更新（`max_concurrent` 除外）。

### `[storage]`

| 键 | 默认 | 说明 |
|----|------|------|
| `path` | `./data`（相对配置文件目录） | 数据目录 |
| `encryption` | `xchacha20poly1305` | 内容加密；`none` 仅调试 |
| `max_vfs_bytes` | 10 GiB | VFS 活跃配额；0 = 不限 |
| `max_message_bytes` | 10 GiB | 消息归档配额 |
| `max_session_bytes` | 2 GiB | transcript 配额；启动时与每次轮转后清理最旧的已关闭分段；0 = 不限 |
| `max_blob_bytes` | 512 MiB | 单文件二进制上限 |
| `message_retention_days` | 0 | 按天数清理旧消息；0 = 只按配额 |
| `session_retention_days` | 0 | transcript 同理 |

切换 `encryption` 不会让旧文件失效：每个 JSONL 文件头记录了自己的算法。

### `[tools]`

- `download`：`allowed_domains` / `blocked_domains` / `max_bytes`
- `web_search` / `web_fetch`：`backend = "auto" | "server" | "tool"`、
  `base_url`、`count` / `max_content_tokens`；`web_fetch.local = true` 时直接抓取
  （逐跳校验重定向 + HTML 转文本），不需要后端与 key
- `mcp`：见下

Web 搜索/抓取不内置官方地址：要么由 provider 声明服务端工具（Anthropic/Responses），
要么配置你自己的后端（OpenAI Chat 只能用后者）。

### `[mcp]`

```toml
[mcp]
enabled = true

[[mcp.servers]]
name = "echo"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-everything"]
env = { FOO = "bar" }
enabled = true
```

MCP 默认关闭；服务器启动失败只告警，不影响主进程。stdio 单行与单次结果都有大小上限。
token 放在 `env` 里：agent 读到的 `/config/mcp` 与 `/config.toml` 把 `env` 的值显示成
`<redacted>`（键名保留），整文件写回时原样带回的占位符会恢复成原值。不要把 token 写进
`args`，那里不遮蔽。

### `[memory]` / `[skills]` / `[style]`

| 节 | 关键键 | 说明 |
|----|--------|------|
| `[memory]` | `enabled`、`review = off\|idle\|every_turn`、`idle_minutes`、`review_interval_minutes`、`review_model`（空 = 主模型）、字符预算 | 记忆索引与后台复盘（`topics` 已在 0.2.0-alpha.42 移除：从未生效） |
| `[skills]` | `curator`、`curator_interval_hours`、`curator_min_new_skills` | 技能归档（技能本身在 VFS 的 `/skills`；`shared_dir` 已在 0.2.0-alpha.40 移除） |
| `[style]` | `enabled`、`profile_path`、`max_chars` | 语气画像 |

### `[swarm]`

| 键 | 默认 | 说明 |
|----|------|------|
| `enabled` | true | 子代理总开关 |
| `max_depth` | 8 | 嵌套深度 |
| `max_concurrency` | 16 | 并发子代理 |
| `agents_dir` | `~/.sbot/agents` | 自定义定义目录 |
| `default_max_steps` | 32 | 子代理步数 |

### `[[monitor]]`

```toml
[[monitor]]
name = "at-me"
enabled = true
source = { qq = true }
when = "event.type == 'message' and event.mentions_self"
deliver = { to = "main", as = "event", batch_window_ms = 200 }
lifecycle = { idle_only = false, max_events = 1000 }
```

`source` 四选一：`qq` / `timer` / `stream` / `internal`。`timer` 三选一，都按本地时间：

- `every`：固定周期，至少 `60s`；以上次触发的计划时刻为锚点，重启与抖动都不会让周期漂移。
- `at = "HH:MM"`：**每天**这个时刻（一次性的提醒用 `lifecycle.one_shot = true`）。
- `cron`：标准 5 字段（分 时 日 月 周，`0`/`7` 都是周日）。日与周两个字段都写了具体值时，
  任一满足即触发；任一以 `*` 开头时只看另一个。永远不会匹配的表达式（如 `0 0 30 2 *`）
  加载时报错。
- 夏令时跳过的时刻顺延到之后第一个有效分钟；回拨造成的重复时刻只触发较早的一次。
- `jitter_secs`（可选）：每次触发随机推迟 `0..=n` 秒，让多个同刻的定时器错开；不设或 `0`
  准点。最多 `3600`，`every` 时必须短于周期；推迟期间到点的下一次触发与这次合并。
  例：`source = { timer = { every = "10m", jitter_secs = 60 } }`。

`internal = { kind = "security" | "budget" | "agent" | "storage" }` 订阅运行时状态变化，
`event.type` 是 `<kind>:<事件>`：

| kind | `event.type` | 何时 |
|------|--------------|------|
| `security` | `security:hold` / `security:released` | 进入 / 解除 hold（状态变化时各一次） |
| `budget` | `budget:exhausted` | 每日 token 或金额预算当天第一次耗尽 |
| `agent` | `agent:done` / `agent:failed` | 子代理一次运行结束 |
| `storage` | `storage:quota` / `storage:disk` | VFS 用量达到配额 90%、磁盘剩余低于写入下限（每 5 分钟检查，越线时一次） |

例：`source = { internal = { kind = "security" } }`，`when = "event.type == 'security:hold'"`。

`stream = { url = "…", kind = "websocket" | "sse" }` 订阅外部事件流（CI、Webhook 中转等），
**只能写在配置文件里**，`Monitor` 工具拒绝创建。`websocket` 要 `ws://` / `wss://`，每个文本
帧是一个事件；`sse` 要 `http://` / `https://`，每个 SSE 事件是一个事件（带 `event:` 名时
文本前缀 `[名字]`）。事件的 `event.type` 是 `stream`，`event.text` 是消息原文（超过 4000
字符截断）。断线后 5 秒起指数退避重连（最长 5 分钟）；断开与恢复各发一条系统通知。
外部内容和 QQ 消息一样经过输入闸。

运行时也能通过 `Monitor` 工具增删，但重启即消失；配置文件里的才是常驻。

`when` 是用 `and` 连接的谓词：`event.type == '…'`、`event.chat == '…'`、
`event.sender == '…'` / `!=`、`event.text contains '…'`、`event.mentions_self`，
任一项前加 `not` 取反。每一项在加载（或 `Monitor add`）时都会校验，写错直接报错。

`deliver.as` 决定投递形式：

- `event`（默认）：`[monitor 名字 | source | chat]` 信封加事件文本，跑一个 monitor 回合；
- `prompt`：投递 `deliver.prompt` 模板（`{{event.text}}`、`{{event.chat}}`、`{{count}}`）；
- `chat`：只用于 `qq` 源且 `to = "main"`，把原始消息交给会话队列，按普通会话回合处理
  （`[queue]` 的去抖与 `mode` 生效，`batch_window_ms` 不生效）。

`source = { qq = false }` 会报错：关掉一个 monitor 用 `enabled = false`。

默认所有 QQ 消息只写入 `/inbox` / transcript，**不逐条唤醒 agent**。内置两个可删除的
唤醒预设，都是 `deliver.as = "chat"`：

| 名称 | 条件 | 行为 |
|------|------|------|
| `wake-on-mention` | 非主人的 `@` 消息 | 交给会话队列，跑一个普通会话回合 |
| `wake-on-owner` | `owner_qq` 的消息（含 stand_in 下主人手打的，见上文“主人的消息”） | 同上 |

拍一拍、撤回等 QQ notice，以及好友/群请求、带 `group_id`/`user_id` 的未知事件，
都会以 `[poke]` / `[recall]` / `[request]` / `[unknown]` 伪消息写入 inbox；
正文带一行摘要和压缩原始 JSON（截断到 1200 字符），原始字段同时保留在消息的 `raw`。
它们不会自动唤醒 agent，除非另建 monitor 订阅对应事件。

用 `Monitor` 工具的 `remove` 删除预设后，会在数据库记录一条禁用标记；重启不会自动恢复。
需要恢复时用 `Monitor add` 同名重新添加即可（标记随之清除，下次启动预设回来）；其他消息
仍留在 `/inbox`，可主动读取。是否记住删除只看来源（预设），与名字无关。

`Monitor add` 的参数与 `[[monitor]]` 同一套 schema 与校验（未知字段报错），区别只有：
默认 `idle_only = true`、`max_events = 50`、`expires = "2h"`；不接受 `stream` 源、
`qq.backlog` 与 `lifecycle.catch_up`；重启即消失。

`deliver.to` 可以是 `main`、子代理实例 id 或 `agent:<名字>`。
`deliver.cc = true` 会在投递给子代理时把同一内容抄送主代理。目标不存在时，
`on_missing` 决定 `drop`（丢弃并告警）、`queue`（最多等 60 秒）或 `spawn`
（新建 general 子代理处理）。

`lifecycle.catch_up` 控制重启后如何处理错过的定时触发：`coalesce`（默认）补投一次并
说明错过了多少次，`skip` 直接放弃。上次触发时间写在 `meta` 表，运行时创建的 monitor
不参与补投。

### `[security.guard]`

| 键 | 默认 | 说明 |
|----|------|------|
| `enabled` | true | 总开关 |
| `mode` | `shadow` | `shadow` 只记录；`enforce` 命中即 hold |
| `backend` | `jev` | `jev` / `llm` / `off`。`llm` 用主 provider 判定，无需 Jev key |
| `base_url` | 空 | Jev 端点；**jev + enforce 下必须配置**，否则每个回合都会被 hold |
| `threshold` | 0.9 | 分类器危险概率阈值 |
| `timeout_ms` | 800 | 单次判定超时 |

key 放 `auth.json`：

```json
{ "security": { "guard": { "api_key": "sk-..." } } }
```

只想跑规则层时显式写 `backend = "off"`，不要在 `enforce` 下留空。
`backend = "llm"` 复用主 provider：`model` 留空则用主模型，判定提示见
`src/security/gate.rs` 的 `LLM_JUDGE_SYSTEM`。

### `[security]` 周期检查

| 键 | 默认 | 说明 |
|----|------|------|
| `check_every_tokens` | 262144 | 每新增这么多 token，在回合边界跑一次独立检查；0 = 关闭 |
| `check_on_compaction` | true | 压缩前也检查一次 |
| `check_overlap_tokens` | 4096 | 检查窗口向前重叠的近似 token |
| `review_model` | 空 | 检查用模型；空 = 主模型 |
| `check_timeout_secs` | 60 | 检查超时时间；超时放行、只告警 |

检查结果写 `security_events`，不写 transcript。`high + complied` 暂停外发（hold），
`critical` 暂停新回合；`medium` 只记录与告警。

### `[compaction]`

| 键 | 默认 | 说明 |
|----|------|------|
| `turn_context_budget` | 120000 | 单回合工作集上限，超过先压缩 |
| `reserve_tokens` | 16384 | 模型窗口里预留给输出/系统提示的 token |
| `keep_recent_tokens` | 20000 | 压缩后保留的最近 token |
| `max_failures` | 3 | 连续自动压缩失败后停止尝试 |
| `memory_flush` | true | 压缩前静默提取 durable facts |
| `post_compact_recall` | true | 压缩后把 `/MEMORY.md` 索引写进新分段 |
| `prune` | true | 裁剪较早的工具结果 |
| `prune_soft_limit` | 4000 | 单条旧工具结果保留的字符数 |
| `prune_keep_turns` | 3 | 最近多少个回合不裁剪 |

自动压缩阈值 = `min(turn_context_budget, context_window - reserve_tokens - max_output_tokens)`，
且不低于 `keep_recent_tokens`。摘要默认用主模型；`[agent].compaction_model` 可指定独立模型。

### `[budget]` / `[gate]` / `[hooks]`

- `[budget]`：`daily_tokens`、`daily_usd`、`turn_tokens`、`chat_sends_per_hour`、
  `chat_turns_per_hour`、`round_trip_wall_secs`、`guard_usd_per_day`。
  - 每日 token/USD 在回合开始时检查（在自动压缩之前），超限跳过新回合（消息仍然入库）；
  - 每日用量逐请求记账：主回合、回退、子代理、压缩摘要、记忆 flush、周期安全审查、
    LLM 门禁的每次模型请求，都按**发出请求的那个模型**的价格计入，缓存读写 token 也算；
  - `turn_tokens` 计的是本回合**新增**的 token：所有请求的输出之和，加上最后一次请求的
    完整提示比第一次多出的部分（每次请求重发的历史不重复计算）；
  - `turn_tokens` 与 `round_trip_wall_secs` 是硬停：循环到上限就以 error 结束；
  - 每 chat 每小时只统计成功发送/已开始的回合；
  - `guard_usd_per_day` 用估算成本，耗尽后分类器降级为规则层。
- `[gate]`：每次 `napcat.*` 调用都过门禁。
  - `trusted`：直接放行，不做发送规则检查，也不受 `max_per_minute` 限制（`[budget]`
    的每 chat 每小时发送上限仍然生效）。
  - `auto`（默认）：每分钟调用上限 `max_per_minute`；发送规则拦截 30 秒内同 chat 重复、
    2 分钟内同内容发到 3 个以上 chat、10 分钟内跨 chat 的 5-gram 重叠、跨 chat 转发媒体、
    无 owner intent 的 @全体；
    `llm_review = true` 时对变更类动作追加一次 LLM 审查，失败按拒绝。
  - `approve`：变更类动作返回 `approval_required` 与请求 id，并私聊 `master_qq` 一条
    通知；`master_qq` 是另一个账号时通知带 6 位确认码、回复即批准，否则只能用
    `sbot gate approve <id>`。批准后 120 秒内用相同参数重试一次即可，60 秒未确认
    自动失效。队列持久化到数据库（只存参数哈希）。
- 上下文注入不在 `[hooks]` 里配置（0.2.0-alpha.37 移除了 `[[hooks.context]]`，旧配置会
  启动失败并提示迁移）：`/AGENTS.md`、`/USER.md`、`/MEMORY.md`、风格档案与技能索引存在即注入；
  其余文件由 agent 自己列在 VFS 的 `/hooks/context.toml`（见 [人设、记忆与技能](persona-memory.md)）。
- `[[hooks.events]]`：声明式规则，不执行脚本。事件与动作的组合只能是下表里的，写错了
  启动报错：

  | 事件 | 触发点 | 可用动作 | `inject_context` 落点 |
  |------|--------|----------|------------------------|
  | `session_start` | 启动、压缩后重建 system | `log`、`inject_context` | system 块 |
  | `session_end` | 进程退出、清空上下文 | `log`、`notify` | — |
  | `pre_compact` | 压缩前 | `log`、`inject_context` | 摘要的输入 |
  | `post_compact` | 压缩后 | `log`、`inject_context` | 新分段的开头 |
  | `message_received` | 消息进入回合前 | `log`、`block`、`inject_context` | 本回合的即时提示（不进 transcript） |
  | `before_send` | `API` 发布动作前 | `log`、`block`、`rewrite` | — |
  | `stop` | 回合结束 | `log` | — |
  | `config_change` | `/config` 写入后 | `log` | — |

  - `match = { group_id, user_id, message_type = "group" | "private" }` 精确匹配；设了
    任一项的 hook 只在带 chat 的事件（`message_received`、`before_send`）上命中。
  - `block` 需要 `reason`；`message_received` 上的 `block` 让这条消息不进入上下文。
  - `rewrite` 用 `pattern`（正则）/ `replacement` 改写消息文本段与群公告正文；上传文件
    没有正文，只受 `block` / `log` 影响。
  - `inject_context` 需要 `text`，或 VFS 路径 `path`（读取时最多 8000 字符，读不到跳过并
    记日志）。
  - `notify` 默认只记日志；`notify_channel = "owner"`（或 `"master"`）才私聊 `master_qq`，
    需要 NapCat 在线，这条通知不经过 `before_send`。
  - `scope` 与 `match.tool_name` 已在 0.2.0-alpha.42 移除（从未改变过行为），旧配置启动
    时提示删除。

## models.json

```json
{
  "providers": {
    "anthropic": {
      "api": "anthropic",
      "base_url": "https://api.anthropic.com",
      "models": [
        {
          "id": "claude-opus-5-5",
          "reasoning": true,
          "thinking_levels": ["low", "medium", "high"],
          "context_window": 200000,
          "max_tokens": 8192,
          "capabilities": ["tools", "vision", "server_tools.web_search"],
          "cost": { "input": 4.0, "output": 20.0 }
        }
      ]
    },
    "local": {
      "api": "chat",
      "base_url": "http://127.0.0.1:32100",
      "models": [{ "id": "deepseek-flash", "context_window": 128000 }]
    }
  }
}
```

- `api` 三选一：`anthropic`、`openai`（Responses）、`chat`（Chat Completions）。
- 模型引用是 `provider/id`；`id` 同时作为发给 API 的 model 字段。
- `capabilities` 决定是否启用服务端工具；入站 QQ 图片按 `input`/`vision` 门控，`Read`/MCP 工具图片仍直接附加。
- `cost` 是每百万 token 的美元价：`input`、`output`，以及可选的 `cache_read`（命中缓存的输入）、
  `cache_write`（写入缓存的输入）。后两项没写时按 `input` 计——预算宁可多算。
- `sbot model set` 热切换后，新模型的 `context_window`、图片能力与价格立即生效。
- 提示缓存：Anthropic 协议在静态 system 前缀与（agent 循环的）对话历史末尾各打一个缓存断点，
  工具循环的每一步都命中上一步写下的前缀；摘要、审查这类一次性请求不打历史断点。
  OpenAI 兼容接口由服务端自动缓存。
- `base_url` 留空时由各 provider 用自己的默认地址（Anthropic 官方；OpenAI 兼容
  端点通常必须显式配置）。
- `models.json` 不存在时只使用内置清单（`assets/models.default.json`）；存在时按
  provider 合并。

## auth.json

```json
{
  "providers": {
    "anthropic": { "api_key": "sk-ant-..." },
    "local": { "api_key": "sk-local" }
  },
  "napcat": { "access_token": "" },
  "security": { "guard": { "api_key": "sk-jev-..." } },
  "tools": {
    "web_search": { "api_key": "..." },
    "web_fetch": { "api_key": "..." }
  }
}
```

- 明文优先；也可以写 `"$ENV_NAME"`，只有值以 `$` 开头时才展开环境变量。
- `auth.json` 不挂载 VFS，模型看不到。
- 缺 key 时启动告警，对应模型不可用。
- `sbot doctor` 会检查引用到的 provider 是否有 key。
