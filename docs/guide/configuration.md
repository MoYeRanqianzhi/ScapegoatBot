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
| `compaction_model` | 空 | 摘要模型；空 = 主模型（当前版本未使用） |
| `max_output_tokens` | 8192 | 单次回复上限 |
| `max_steps` | 48 | 单回合工具循环上限 |
| `master` | `self` | owner 通知目标：`self` 或 QQ 号 |
| `receive_scopes` | `["*"]` | 允许读的 chat 列表 |
| `send_scopes` | `["*"]` | 允许发的 chat 列表；空 = 禁止一切发送 |
| `retry_attempts` / `retry_base_ms` / `retry_max_ms` | 3 / 400 / 8000 | provider 重试次数、基础间隔、上限 |
| `retry_mode` | `stepped` | `stepped` 阶梯退避（base、2×base…封顶 max）；`fixed` 固定 base |

`mode` 启动时固定，修改需重启。

### `[napcat]`

| 键 | 默认 | 说明 |
|----|------|------|
| `url` | `ws://127.0.0.1:3001` | OneBot 11 WebSocket 地址 |

access token 在 `auth.json`，不在这里。

### `[filter]`：准入门

语义是**拒收**：命中的消息不写 JSONL、不进 inbox、不进 transcript，monitor 也看不到。

| 键 | 默认 | 说明 |
|----|------|------|
| `ignore_groups` | `[]` | 群号列表；这些群的消息直接丢弃 |
| `ignore_users` | `[]` | QQ 号列表 |
| `ignore_patterns` | `[]` | 正则；命中即拒收 |
| `require_mention` | false | 群里只有 @ 自己或回复自己时才接收 |

### `[session]`

| 键 | 默认 | 说明 |
|----|------|------|
| `scope` | `shared` | 全部 chat 一条 transcript；`per_chat` 每个 chat 独立 |

`shared` 让 A 群与 B 私聊的上下文互相可见，是替身场景的默认选择；需要强隔离时用
`per_chat`（文件名带 `group-<id>-` / `private-<id>-` 前缀）。

### `[queue]`

| 键 | 默认 | 说明 |
|----|------|------|
| `mode` | `steer` | 回合中到达的消息：`steer` 立即注入 / `followup` 下一回合 / `collect` 合并 |
| `max_concurrent` | 4 | 全局并发回合数；修改需重启 |
| `cap_messages` | 200 | 每 chat 待处理消息上限 |
| `cap_batches` | 20 | 每 chat 排队批次数上限 |
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
| `max_session_bytes` | 2 GiB | transcript 配额 |
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

### `[memory]` / `[skills]` / `[style]`

| 节 | 关键键 | 说明 |
|----|--------|------|
| `[memory]` | `enabled`、`review = off\|idle\|every_turn`、`idle_minutes`、`review_interval_minutes`、字符预算 | 记忆索引与后台复盘 |
| `[skills]` | `enabled`、`curator`、`dirs` | 技能加载与归档 |
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

`source` 四选一：`qq` / `timer` / `stream` / `internal`。`timer` 支持 `every`、`at`、
`cron`（5 字段）。运行时也能通过 `Monitor` 工具增删，但重启即消失；配置文件里的才是常驻。

`deliver.cc = true` 会在投递给子代理时把同一内容抄送主代理。绑定目标不存在时，
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
  - 每日 token/USD 在回合开始时检查，超限跳过新回合（消息仍然入库）；
  - `turn_tokens` 与 `round_trip_wall_secs` 是硬停：循环到上限就以 error 结束；
  - 每 chat 每小时只统计成功发送/已开始的回合；
  - `guard_usd_per_day` 用估算成本，耗尽后分类器降级为规则层。
- `[gate]`：每次 `napcat.*` 调用都过门禁。
  - `trusted`：直接放行，只统计速率。
  - `auto`（默认）：每分钟调用上限 `max_per_minute`；发送规则拦截 30 秒内同 chat 重复、
    2 分钟内同内容发到 3 个以上 chat、10 分钟内跨 chat 的 5-gram 重叠、跨 chat 转发媒体、
    无 owner intent 的 @全体；
    `llm_review = true` 时对变更类动作追加一次 LLM 审查，失败按拒绝。
  - `approve`：变更类动作返回 `approval_required` 与请求 id，并私聊 master 一个
    6 位 nonce；主人回复 nonce 或用 `sbot gate approve <id>` 批准，120 秒内重试一次
    即可，60 秒未确认自动失效。队列持久化到数据库（只存参数哈希）。
- `[hooks.context]`：`name`、`path`、`kind`、`when`、`inject`、`max_bytes`；
  自定义注入到 system prompt 的上下文文件。
- `[hooks.events]`：所有事件都会触发 `log` / `block` / `inject_context` / `notify`；
  `rewrite` 只在 `BeforeSend` 生效，`scope` 支持 `model`（默认）与 `all`。
  `inject_context` 的落点：SessionStart → system，MessageReceived → 回合内提示，
  Pre/PostCompact → 摘要输入/新分段；`notify` 只支持 `owner`/`master`。

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
- `capabilities` 决定是否启用服务端工具/图片输入。
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
