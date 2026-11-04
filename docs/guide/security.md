# 安全模型

ScapegoatBot 的安全目标是：**模型可以自由行动，但不能悄悄把敏感内容送出去，
也不能在被判定危险后继续对外操作**。它不是操作系统级沙箱，也不防同机管理员。

## 三层检查

| 层 | 时机 | 检查对象 | 失败语义 |
|----|------|----------|----------|
| 输入闸 | 每次模型请求前 | 新进入上下文的文本 | 不可判定即不安全 |
| 输出闸 | 模型产出后、副作用前 | 文本、思考、工具参数 | 不可判定即不安全 |
| 动作门禁 `[gate]` | 每次 `napcat.*` 调用前 | action + 参数 + owner 证据 | 审查失败即拒绝 |

前两层是“内容危不危险”，`[gate]` 是“这次调用该不该发生”。

### 规则层 + 分类器

1. **规则层**（确定性正则）：凭据形态（`sk-…`、`api_key=…`）、提示注入话术、
   索要系统提示。规则命中**不受概率阈值影响**。
2. **分类器**：Jev（默认）或 `llm`（复用主 provider）。返回危险概率，达到
   `threshold`（默认 0.9）即危险。超时、HTTP 失败、响应不可解析都按
   “无法判定即不安全”处理。

`mode = "shadow"` 只记录、不拦截；`mode = "enforce"` 命中即写
`~/.sbot/SECURITY_HOLD` 并进入 hold。

### 默认配置是什么

默认 `mode = "shadow"`、`backend = "jev"`、`base_url` 为空：

- 开箱只跑规则层，且只记录、不拦截；
- 想真正拦截，必须配 `mode = "enforce"`，并三选一：
  - 配好 Jev：`base_url` + `auth.json` 的 `security.guard.api_key`；
  - `backend = "llm"`：复用主 provider（不需要 Jev key）；
  - 只要规则层：显式 `backend = "off"`。

**enforce 下没有可用分类器时，闸会 hold 每一个新回合**（fail-closed）。
启动与 `sbot doctor` 都会明确提示。

### Jev 配置

```toml
[security.guard]
enabled = true
mode = "enforce"
backend = "jev"
base_url = "https://your-jev-endpoint"
threshold = 0.9
timeout_ms = 800
```

```json
{ "security": { "guard": { "api_key": "sk-..." } } }
```

请求发往 `{base_url}/v1/systemone`，响应里的 `answers.<name>.noul` 作为概率。

### llm 分类器

```toml
[security.guard]
backend = "llm"
model = ""        # 空 = 主模型
timeout_ms = 800
```

用主 provider 发一个短 JSON 判定请求；适合不想额外托管 Jev 的部署。代价是每次
输入/输出判定都消耗一次主模型的 token。`[budget].guard_usd_per_day` 耗尽后分类器
降级为规则层（规则仍然生效），并在日志里告警。

## 周期安全检查 `[security]`

输入/输出闸只看单次内容；周期检查在回合边界用**独立模型**找“是否有人尝试注入/
社工，以及 agent 是否真的照做了”。

| 键 | 默认 | 说明 |
|----|------|------|
| `check_every_tokens` | 262144 | 自上次检查新增这么多 token 后触发；0 = 关闭 |
| `check_on_compaction` | true | 压缩前也检查一次 |
| `check_overlap_tokens` | 4096 | 检查窗口向前重叠的近似 token |
| `review_model` | 空 | 检查模型；空 = 主模型 |
| `check_timeout_secs` | 60 | 超时放行（只告警，不暂停） |

结果只写 `security_events`，不写主 transcript。动作：

- `low`：记录；
- `medium`：记录 + 告警；
- `high` 且 `complied = true`：暂停外发（hold）；
- `critical`：暂停新回合（`agent_paused`），重启后仍从 hold 文件恢复。

检查超时/失败按放行处理；连续失败计数只告警。

## hold

命中后：

- 写 `~/.sbot/SECURITY_HOLD`（重启仍然有效）；
- 所有 `napcat.*` 动作返回 `Disabled during hold`，包括拉黑、禁言、踢人；
- `Download`、`WebSearch`、`WebFetch`、`MCP` 同样拒绝；
- `agent.*` 与 `api.*` 只读查询仍可用，agent 可以看状态、写记忆、自查；
- `critical` 暂停的新回合在 `agent_paused` 置位后跳过；控制通道仍可 `chat`。

hold **没有超时、没有自动恢复**。

### 恢复

```bash
sbot security resume
```

或 agent 自己调用 `API` 的 `agent.resume`。恢复流程：

1. 取最近一次被判危险的原始内容（内存里，不写磁盘）+ 当前历史尾部 + hold 原因；
2. 用同样的规则层/分类器复核；
3. 通过才删除 `SECURITY_HOLD` 并清掉 `agent_paused`，否则继续 hold。

`provider stop_reason = refusal` 会直接 `force_hold`，不重试、不换模型。

## owner intent 与发送

owner intent 由 runtime 核实，不接受模型自报或聊天里的“主人同意了”：

- **control**：`sbot chat` 通过本地 IPC 触发，恒为 true；
- **owner_message**：触发批次里有 `owner_qq` 的非 self 消息；
- 子代理继承 spawn 时的证据。

`API` 对扩张动作（发消息、加好友、处理请求、改资料、上传文件、改群名片/群名/
授予权限/发公告）要求证据。**同一 chat 的回复有明确理由**，不需要额外证据；
跨 chat 发送、私聊陌生人等必须有 owner intent，否则返回 `owner_intent_required`。
收缩动作（踢人、禁言、退群、解散、撤回、删除群文件）不需要证据。

## 动作门禁 `[gate]`

| 模式 | 行为 |
|------|------|
| `trusted` | 直接放行，只统计速率 |
| `auto`（默认） | 速率限制 + 发送规则 + 可选 LLM 审查 |
| `approve` | 变更类动作需要主人通过 IPC 确认 |

`auto` 的规则：

- 每分钟 `max_per_minute` 次 `napcat.*`，超出拒绝；
- 30 秒内同 chat 重复同一内容 → 拒绝；
- 2 分钟内同一内容发往 3 个以上不同 chat → 拒绝（除非有 owner intent）；
- 无 owner intent 的 @全体 → 拒绝；
- `llm_review = true`：对变更类动作追加一次审查，审查失败/超时按拒绝处理。

`approve`：变更类动作返回 `approval_required` 和请求 id（60 秒内有效）。机器人同时
用 NapCat 私聊 master 发送 6 位 nonce；主人在该私聊里直接回复 nonce 即批准，也可用
控制通道：

```bash
sbot gate pending
sbot gate approve <id>   # 批准后 120 秒内重试一次
sbot gate reject <id>
```

批准是一次性的；普通群聊/他人回复不构成确认。队列持久化到 `gate_approvals`（只存
参数哈希），重启后未过期的批准仍可消费一次。

## 主人保护

目标为 `owner_qq` / `master_qq` 的 destructive action 无条件拒绝：

- 踢人、禁言、改名片、给/撤管理；
- 目标 QQ 可以是数字或数字字符串；解析失败按拒绝处理，不会跳过保护；
- 全员禁言、退群、解散群没有用户目标，做不了“目标是主人”的判断，但会改变主人所在群
  的可达性，因此要求 owner intent；群公告同样需要。

## 发送范围

`[agent].send_scopes` 控制能发到哪些 chat：

- 空列表 = 禁止一切发送（fail-closed）；
- `["*"]` = 不限制；
- 否则只允许列出的 `group:<gid>` / `private:<qq>`。

`[agent].receive_scopes` 控制能读哪些 chat：消息视图、收件箱、联系人视图与带
`group_id` / `user_id` 的 API 查询都会过滤；空列表 = 拒绝一切读取。`get_msg` 只有
`message_id`，无法判定 chat，仍可读取（见 limits.md）。

## 网络

- `Download` / `WebFetch` 在请求前校验协议、域名与解析后的公网地址；
- `Download` 的 DNS 解析在连接层过滤非公网地址（防 rebinding），重定向逐跳校验；
- `WebFetch` 由你配置的后端执行抓取；返回前复核 `final_url`，不在允许范围内的结果
  不会交给模型（但内容已经被后端取过，敏感目标不要交给不可信后端）。

## 数据与密钥

- 消息 JSONL、transcript、blob 内容用 XChaCha20-Poly1305；不同用途的密钥由
  HKDF 从主密钥派生。
- 文件头记录算法；改 `[storage].encryption` 不会让旧历史失效。
- `~/.sbot/key` 是唯一秘密，权限 0600；`sbot backup` **包含它**。
- 索引（SQLite）与路径是明文元数据：能看出“谁在什么时候聊过”，看不到内容。
- 发送账本 `outbound_intents` 在启动时把久悬的 `pending` 标为 `unknown`；发送超时
  也不会被当成“肯定没发出去”，避免重复发送。
- 威胁边界：不防同机管理员、不防进程内存读取、不防拿到 key 的人。

## 已知边界

| 项 | 现状 |
|----|------|
| `approve` 的确认通道 | 只有本地 IPC；没有向 master QQ 发 nonce 的流程；队列在内存里，重启丢失 |
| `auto` 的发送规则 | 基于频率/重复/跨群同文/@all 的启发式，没有跨 chat n-gram 内容比对 |
| 周期检查的缓存复用 | 检查请求是独立窗口，不与主请求逐字节共享前缀 |
| 群级破坏动作 | 没有用户目标，靠 owner intent 而不是硬保护 |
| WebFetch | 后端已经抓取后才复核 `final_url` |

这些项在 [limits.md](limits.md) 也列出；启用前请不要把它们当成已有的防护。
