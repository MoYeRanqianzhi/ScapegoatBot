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
- `high` 且 `complied = true`：hold（关闭 QQ 出口）；
- `critical`：hold，并在下一回合开始前丢弃当前上下文（换到新的 transcript 分段，旧分段留档）。
  以前这里会“暂停所有新回合”，只有宿主机上的控制通道能解开；agent 无人值守，那等于永久停机，
  0.2.0-alpha.37 起改为现在的做法。

检查超时/失败按放行处理；连续失败计数只告警。

## hold

命中后：

- 写 `~/.sbot/SECURITY_HOLD`（重启仍然有效；只记原因，不记被判危险的原文）；
- 所有 `napcat.*` 动作返回 `Disabled during hold`，包括拉黑、禁言、踢人；
- 其余工具（文件、记忆、`WebSearch`、`WebFetch`、`MCP`、`Download`）不受影响；
- 回合照常运行：`agent.*` 与 `api.*` 仍可用，agent 可以看状态、隔离可疑内容、自查。

hold **没有超时**，但**由 agent 自己解除**：这个 agent 无人值守，不会有人来替它解开。

### 恢复

agent 调用 `API` 的 `agent.resume`（宿主机上的 `sbot security resume` 走同一条路径）：

1. 取**当前已提交上下文**的尾部，用同样的规则层/分类器复核；
2. 干净就删除 `SECURITY_HOLD`，否则继续 hold，并提示 agent 先隔离；
3. 输出闸命中时，危险输出从未进入上下文，通常可以直接恢复；输入闸命中时，可疑内容
   已在上下文里，agent 要先用 `agent.quarantine`（传入原文）把它换成占位符，
   或用 `agent.clear_context` 重新开始，再调用 `agent.resume`。

`agent.quarantine` 把原文另存到 `/quarantine/`，只替换内存中的上下文；磁盘上的 transcript
分段保留原样作为审计记录；重启恢复上下文时按 `/quarantine/` 里的文件再替换一次，隔离不会因重启失效。

不复核当初被判危险的那段原文：它永远是危险的，拿它复核等于 hold 永远解不开。

`provider stop_reason = refusal` 会直接 `force_hold`，不重试、不换模型。

## owner intent 与发送

owner intent 由 runtime 核实，不接受模型自报或聊天里的“主人同意了”：

- **control**：`sbot chat` 通过本地 IPC 触发，恒为 true；
- **owner_message**：触发批次里的非 `self` 消息**全部**来自 `owner_qq`（stand_in 下包括主人在本账号上手打、经发送账本确认不是本进程发出的消息，见 [配置说明](configuration.md#主人的消息)）；别人的消息同批进入时，模型照着谁的话行动无法区分，主人的授权不能被借用；文件与记忆里的文字永远不算；
- 回合进行中经 steering 进入的消息只要有一条不是主人的，owner intent 立即撤销，本回合内不再恢复；
- 子代理继承 spawn 时的证据（同一份状态，父回合撤销时子代理也随之失去）。

`API` 对扩张动作（发消息、加好友、处理请求、改资料、上传文件、改群名片/群名/
授予权限/发公告）要求证据。**同一 chat 的回复有明确理由**，不需要额外证据；
跨 chat 发送、私聊陌生人等必须有 owner intent，否则返回 `owner_intent_required`。
收缩动作（踢人、禁言、撤回、删除群文件）在触发本回合的 chat 内不需要证据；
作用于别的群需要 owner intent。全员禁言、退群、解散群恒需 owner intent（见下方“主人保护”）。

## 动作门禁 `[gate]`

| 模式 | 行为 |
|------|------|
| `trusted` | 直接放行，只统计速率 |
| `auto`（默认） | 速率限制 + 发送规则 + 可选 LLM 审查 |
| `approve` | 变更类动作需要主人通过 IPC 确认 |

`auto` 的规则（发送类规则作用于所有公开内容的动作：消息、群公告、上传文件；公告正文
按一段文本、上传的文件按一个文件段计算）：

- 每分钟 `max_per_minute` 次 `napcat.*`，超出拒绝；
- 30 秒内同 chat 重复同一内容 → 拒绝；
- 2 分钟内同一内容发往 3 个以上不同 chat → 拒绝（除非有 owner intent）；
- 10 分钟内与发往其他 chat 的长文本（≥24 字）共享 5 个以上 5-gram → 拒绝；
- 本回合上下文里来自其他 chat 的片段（消息、pending history、Read/API 查询）被复述 →
  拒绝；
- 图片/文件/语音/视频第一次出现在别的 chat，转发到新 chat → 拒绝；
- 无 owner intent 的 @全体 → 拒绝；
- `llm_review = true`：对变更类动作追加一次审查，审查失败/超时按拒绝处理。

`approve`：变更类动作返回 `approval_required` 和请求 id（60 秒内有效）。机器人同时
私聊 `master_qq` 一条通知。只有 `master_qq` 与机器人登录账号不同时，通知才带 6 位
确认码、主人在那个私聊里回复确认码即批准；`master_qq` 就是本账号时（stand_in 默认），
本账号发出的消息与 agent 自己发的无法区分，私聊回复一律不算，只能用控制通道：

```bash
sbot gate pending
sbot gate approve <id>   # 批准后 120 秒内用相同参数重试一次
sbot gate reject <id>
```

批准是一次性的，并绑定 action 与参数哈希；普通群聊/他人回复不构成确认。发送范围检查
在门禁之前，越界的请求不会生成待确认项。队列持久化到 `gate_approvals`（只存
参数哈希），重启后未过期的批准仍可消费一次。

## 主人保护

目标为 `owner_qq` / `master_qq` 的 destructive action 无条件拒绝：

- 踢人、禁言、改名片、给/撤管理；
- 目标 QQ 可以是数字或数字字符串；解析失败按拒绝处理，不会跳过保护；
- 全员禁言、退群、解散群没有用户目标，做不了“目标是主人”的判断，但会改变主人所在群
  的可达性，因此要求 owner intent；群公告同样需要。
- 踢人、禁言、改名片这类群管理本身不要求 owner intent，但只能作用于触发本回合的那个群；
  在 A 群的回合里管理 B 群需要 owner intent。

## 发送范围

`[agent].send_scopes` 控制能对哪些 chat 产生影响——发消息、戳一戳、群管理、
上传文件、改群名片/群名/公告都算：

- 空列表 = 禁止一切对外动作（fail-closed）；
- `["*"]` = 不限制；
- 否则只允许列出的 `group:<gid>` / `private:<qq>`。

召回、表情回应这类只带 `message_id` 的动作，按消息归档反查它属于哪个 chat，再按上面的
范围判断；归档里查不到的 id 一律拒绝，不能拿任意 id 去召回别处的消息。

`napcat.send_msg` 必须恰好给出一个目标（`group_id` 或 `user_id`）。NapCat 在私聊对象
查不到时会改发到同时给出的 `group_id`，两者同时出现时无法确定真实目标，直接拒绝。

## 发出的文件与媒体

消息里的 image/record/video/file 段，以及 `upload_*_file`、`set_qq_avatar` 的 `file`，
只接受三种写法：VFS 绝对路径（如 `/tmp/a.png`）、公网 `http(s)://` URL、`base64://`。
NapCat 收到的永远是 `base64://`：VFS 路径由 sbot 自己读出，URL 由 sbot 按 Download
的网络策略下载（只连公网、逐跳检查重定向）。`file://`、宿主机路径、`data:` 一律拒绝——
NapCat 会读取宿主机上存在的任意路径，原样转交就等于让一次提示注入把
`~/.sbot/auth.json` 发进群里。单次调用内联的内容合计不超过 20 MiB。

action 只接受注册表声明过的参数（`api.help` 可查），未声明的字段直接报错，
不会被转交给 NapCat。

`[agent].receive_scopes` 控制能读哪些 chat：消息视图、收件箱、联系人视图与带
`group_id` / `user_id` 的 API 查询都会过滤；空列表 = 拒绝一切读取。`get_msg` 只有
`message_id`，无法判定 chat，仍可读取（见 limits.md）。

## 网络

- `Download` / `WebFetch` 在请求前校验协议、域名与解析后的公网地址；
- `Download` 的 DNS 解析在连接层过滤非公网地址（防 rebinding），重定向逐跳校验；
- `WebFetch` 默认由你配置的后端执行抓取，返回前复核 `final_url`；设置
  `[tools.web_fetch].local = true` 后改为本地逐跳校验并直接抓取（HTML 转文本、
  2 MiB/字符上限）。敏感目标优先用 local。

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
| `approve` 的确认通道 | 本地 IPC，或向 master QQ 发 nonce；待审批队列持久化在 `gate_approvals`，重启不丢 |
| `auto` 的发送规则 | 频率/重复/跨群同文/@all/5-gram/context_sources/媒体来源是启发式；Grep 不按来源拆分 |
| 周期检查的缓存复用 | 检查请求是独立窗口，不与主请求逐字节共享前缀 |
| 群级破坏动作 | 没有用户目标，靠 owner intent 而不是硬保护 |
| owner intent 的归属 | 只看本回合进入的消息；更早的上下文（pending history、之前的回合）里别人的话不会撤销 intent |
| WebFetch | 后端已经抓取后才复核 `final_url` |

这些项在 [limits.md](limits.md) 也列出；启用前请不要把它们当成已有的防护。
