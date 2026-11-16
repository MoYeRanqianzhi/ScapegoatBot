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
2. **分类器**：Jev（默认）或 `llm`（复用主 provider）。Jev 返回危险概率，达到
   `threshold`（默认 0.9）即危险；`llm` 后端由模型直接给出“危险 / 不危险”的结论。超时、
   HTTP 失败、响应不可解析都按“无法判定即不安全”处理——超时由 `timeout_ms` 决定，默认
   800 毫秒，对 `llm` 后端通常不够，要调大（上限 5000）。

`mode = "shadow"` 时输入 / 输出闸只记录、不拦截；`mode = "enforce"` 命中即写
`~/.sbot/SECURITY_HOLD` 并进入 hold。

**`shadow` 不等于永远不会 hold。** 有两条路径不看 `mode`：provider 明确拒绝回答
（`stop_reason = refusal`），以及周期安全检查判定 `high` 且 agent 已照做、或 `critical`
（见下）。周期检查默认是开着的，所以默认配置下 agent 也可能进入 hold。

### 默认配置是什么

默认 `mode = "shadow"`、`backend = "jev"`、`base_url` 为空：

- 开箱时输入 / 输出闸只跑规则层，且只记录、不拦截；
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
timeout_ms = 3000  # 默认的 800 对通用模型通常不够；超时算“无法判定”
```

用主 provider 发一个短 JSON 判定请求；适合不想额外托管 Jev 的部署。代价是每次
输入/输出判定都消耗一次主模型的 token。`[budget].guard_usd_per_day` 耗尽后分类器
降级为规则层（规则仍然生效），并在日志里告警。

## 周期安全检查 `[security]`

输入/输出闸只看单次内容；周期检查在回合边界用一次**独立的模型请求**（默认还是主模型，
`review_model` 可以另指定）找“是否有人尝试注入/社工，以及 agent 是否真的照做了”。它看的是
最近一段上下文——约 4.8 万字符，加上 `check_overlap_tokens` 折算的重叠部分——不是上次检查
以来的全部；两次检查之间进出上下文的内容如果比这个窗口长，较早的那部分不会被这一层看到。

| 键 | 默认 | 说明 |
|----|------|------|
| `check_every_tokens` | 262144 | 自上次检查新增这么多 token 后触发；0 = 关闭 |
| `check_on_compaction` | true | 压缩前也检查一次 |
| `check_overlap_tokens` | 4096 | 检查窗口在基本长度之外向前多看的近似 token |
| `review_model` | 空 | 检查模型；空 = 主模型 |
| `check_timeout_secs` | 60 | 超时放行（只告警，不暂停） |

结果只写 `security_events`，不写主 transcript。动作：

- `low`：记录；
- `medium`：记录 + 告警；
- `high` 且 `complied = true`：hold（关闭 QQ 出口）；
- `critical`：hold，并在下一回合开始前丢弃当前上下文（换到新的 transcript 分段，旧分段留档）。
  以前这里会“暂停所有新回合”，只有宿主机上的控制通道能解开；agent 无人值守，那等于永久停机，
  0.2.0-alpha.37 起改为现在的做法。

检查超时/失败按放行处理，那一段不会补查；连续失败计数只告警。

## hold

命中后：

- 写 `~/.sbot/SECURITY_HOLD`（重启仍然有效）。里面只记原因，不记被判危险的原文；周期检查
  触发的 hold，原因里带着审查模型写的几条发现，可能复述到原文的片段；
- 所有 `napcat.*` 动作返回 `Disabled during hold`，包括拉黑、禁言、踢人；
- `/config` 与 `/config.toml` 只读：上下文可能已被注入，此时改配置会把一次注入变成持久设置；
- 其余工具（文件、`WebSearch`、`WebFetch`、`MCP`）不受影响；
- 回合照常运行：`API` 的 `local.*`（下载、图片）、`agent.*` 与 `api.*` 仍可用，agent 可以
  看状态、隔离可疑内容、自查。

hold **没有超时**，但**由 agent 自己解除**：这个 agent 无人值守，不会有人来替它解开。

### 恢复

agent 调用 `API` 的 `agent.resume`（宿主机上的 `sbot security resume` 走同一条路径）：

1. 取**当前已提交上下文**的尾部，用同样的规则层/分类器复核（上下文是空的就直接解除）；
2. 干净就删除 `SECURITY_HOLD`，否则继续 hold，并在返回里写明原因。原因有两类：上下文里
   还有被判危险的内容（先隔离）；或者分类器本身不可用 / 超时（`classifier unavailable: …`，
   那是配置或网络问题，隔离解决不了）；
3. 输出闸命中时，危险输出从未进入上下文，通常可以直接恢复；输入闸命中时，可疑内容
   已在上下文里，agent 要先用 `agent.quarantine`（传入原文）把它换成占位符，
   或用 `agent.clear_context` 重新开始，再调用 `agent.resume`。

`agent.quarantine` 把原文另存到 `/quarantine/`，只替换内存中的上下文；磁盘上的 transcript
分段保留原样作为审计记录；重启恢复上下文时按 `/quarantine/` 里的文件再替换一次，隔离不会因
重启失效——所以**不要删 `/quarantine/` 里的文件**，删了隔离就失效。替换按原文精确匹配；
模型带签名的思考块与服务端工具的调用 / 结果不会被替换。

`sbot security resume` 需要 agent 在运行（`agent.state = running`），否则返回 `not_ready`。

不复核当初被判危险的那段原文：它永远是危险的，拿它复核等于 hold 永远解不开。

`provider stop_reason = refusal` 会直接 `force_hold`，不重试、不换模型。

## owner intent 与发送

owner intent 由 runtime 核实，不接受模型自报或聊天里的“主人同意了”：

- **control**：`sbot chat` 通过本地 IPC 触发，恒为 true；
- **owner_message**：触发批次里的非 `self` 消息**全部**来自 `owner_qq`（stand_in 下包括主人在本账号上手打、经发送账本确认不是本进程发出的消息，见 `sbot docs sbot-deploy configuration` 的“主人的消息”）；别人的消息同批进入时，模型照着谁的话行动无法区分，主人的授权不能被借用；文件与记忆里的文字永远不算；
- 回合进行中经 steering 进入的消息只要有一条不是主人的，owner intent 立即撤销，本回合内不再恢复；
- monitor 把主人的一条消息单独投递时带 owner intent；与别的事件合并成一批投递时不带。
  定时器等其它 monitor 回合不带；
- 子代理继承 spawn 时的证据（同一份状态，父回合撤销时子代理也随之失去）。

没有 owner intent 的回合里，各类动作的规则：

| 动作 | 规则 |
|------|------|
| 在触发本回合的 chat 里发消息 | 允许：回复有明确理由 |
| 私聊 `master_qq` | 允许：那是主人的通知通道 |
| 往别的 chat 发消息、私聊别人 | 拒绝，返回 `owner_intent_required` |
| 处理好友 / 加群请求、改资料 / 头像 / 在线状态、上传文件 | 拒绝 |
| 踢人、禁言 | 只能在触发本回合的那个群里做；作用于别的群拒绝 |
| 改群名片、改群名、给 / 撤管理、发群公告、全员禁言、退群、解散群 | 拒绝：这些恒需 owner intent，在本群也一样 |
| 撤回消息、表情回应、删除群文件 | 不要求 owner intent（仍受发送范围约束） |

monitor / 定时器回合不属于任何 chat：除了私聊 `master_qq`，发送都需要 owner intent。

## 动作门禁 `[gate]`

| 模式 | 行为 |
|------|------|
| `trusted` | 直接放行：不做发送规则检查，不受 `max_per_minute` 限制；`[budget].chat_sends_per_hour` 仍然生效 |
| `auto`（默认） | 速率限制 + 发送规则 + 可选 LLM 审查 |
| `approve` | 变更类动作需要主人通过 IPC 确认 |

`auto` 的规则（发送类规则作用于所有公开内容的动作：消息、群公告、上传文件；公告正文
按一段文本、上传的文件按一个文件段计算）：

- 每分钟 `max_per_minute` 次 `napcat.*`，超出拒绝；
- 30 秒内同 chat 重复同一内容（12 个字符以上；“好的”这类短应答不算）→ 拒绝；
- 2 分钟内同一内容发往 3 个以上不同 chat → 拒绝（除非有 owner intent）；
- 10 分钟内与发往其他 chat 的长文本（≥24 字）共享 5 个以上 5-gram → 拒绝（除非有 owner intent）；
- 本回合上下文里来自其他 chat 的片段（消息、pending history、Read/API 查询）被复述 →
  拒绝（除非有 owner intent）；
- 图片/文件/语音/视频第一次出现在别的 chat，转发到新 chat → 拒绝（除非有 owner intent）。
  按消息段里的 `url` / `file` 原值比对；
- 无 owner intent 的 @全体 → 拒绝；
- `llm_review = true`：对变更类动作追加一次审查，审查失败/超时按拒绝处理。

比较内容时不看写法：同一段文字写成字符串还是 `text` 段都算同一内容；比的是 agent 给出的
原文，不是 `before_send` 改写之后的。

`approve`：变更类动作返回 `approval_required` 和请求 id（60 秒内有效）；这个模式下不跑上面的
发送规则与 LLM 审查，每个动作都由主人过目。确认码没有尝试次数限制，靠 60 秒的有效期。机器人同时
私聊 `master_qq` 一条通知。只有 `master_qq` 与机器人登录账号不同时，通知才带 6 位
确认码、主人在那个私聊里回复确认码即批准；`master_qq` 就是本账号时（stand_in 默认），
本账号发出的消息与 agent 自己发的无法区分，私聊回复一律不算，只能用控制通道：

```bash
sbot gate pending
sbot gate approve <id>   # 批准后 120 秒内用相同参数重试一次
sbot gate reject <id>
```

批准是一次性的，并绑定 action 与参数哈希；普通群聊/他人回复不构成确认。发送范围检查
在门禁之前，越界的请求不会生成待确认项。队列持久化到 `gate_approvals`（存动作名、摘要、
参数哈希与确认码，不存参数原文），重启后未过期的批准仍可消费一次。`sbot gate` 的命令需要
agent 在运行，否则返回 `not_ready`。

## 主人保护

目标为 `owner_qq` / `master_qq` 的 destructive action 无条件拒绝：

- 踢人、禁言、改名片、给/撤管理；
- 目标 QQ 可以是数字或数字字符串；解析失败按拒绝处理，不会跳过保护；
- 全员禁言、退群、解散群没有用户目标，做不了“目标是主人”的判断，但会改变主人所在群
  的可达性，因此要求 owner intent；群公告同样需要。
- 踢人、禁言本身不要求 owner intent，但只能作用于触发本回合的那个群；在 A 群的回合里管理
  B 群需要 owner intent。改名片、改群名、给 / 撤管理恒需 owner intent。

## 发送范围

`[agent].send_scopes` 控制能对哪些 chat 产生影响——发消息、戳一戳、群管理、
上传文件、改群名片/群名/公告都算：

- 空列表 = 不允许对任何 chat 做动作（fail-closed）。不针对某个 chat 的动作——处理好友 /
  加群请求、改资料 / 头像 / 在线状态——不归发送范围管，它们由 owner intent 约束；
- `["*"]` = 不限制；
- 否则只允许列出的 `group:<gid>` / `private:<qq>`。

召回、表情回应这类只带 `message_id` 的动作，按消息归档反查它属于哪个 chat，再按上面的
范围判断；归档里查不到的 id 一律拒绝，不能拿任意 id 去召回别处的消息。

`napcat.send_msg` 必须恰好给出一个目标（`group_id` 或 `user_id`）。NapCat 在私聊对象
查不到时会改发到同时给出的 `group_id`，两者同时出现时无法确定真实目标，直接拒绝。

## 发出的文件与媒体

消息里的 image/record/video/file 段，以及 `upload_*_file`、`set_qq_avatar` 的 `file`，
只接受三种写法：VFS 绝对路径（如 `/tmp/a.png`）、公网 `http(s)://` URL、`base64://`。
NapCat 收到的永远是 `base64://`：VFS 路径由 sbot 自己读出，URL 由 sbot 自己下载（只连
公网、逐跳检查重定向、单个 20 MiB、30 秒；`[tools.download]` 的域名允许 / 阻止列表不作用于这里）。`file://`、宿主机路径、`data:` 一律拒绝——
NapCat 会读取宿主机上存在的任意路径，原样转交就等于让一次提示注入把
`~/.sbot/auth.json` 发进群里。单次调用内联的内容合计不超过 20 MiB。

action 只接受注册表声明过的参数（`api.help` 可查），未声明的字段直接报错，
不会被转交给 NapCat。

`[agent].receive_scopes` 控制能读哪些 chat：消息视图、收件箱、联系人视图与带
`group_id` / `user_id` 的 API 查询都会过滤；空列表 = 拒绝一切读取。`get_msg` 只有
`message_id`：限制了读取范围时按消息归档反查它属于哪个 chat，查不到或不在范围内就拒绝。
它只管 agent **主动读**什么，不是准入过滤：范围外的消息照样入库，被 monitor 命中时照样进
上下文；不想收用 `[filter]`。

## 网络

- `local.net.download` / `WebFetch` 在请求前校验协议、域名与解析后的公网地址；
- `local.net.download` 的 DNS 解析在连接层过滤非公网地址（防 rebinding），重定向逐跳校验；
- `WebFetch` 默认由你配置的后端执行抓取，返回前复核 `final_url`；设置
  `[tools.web_fetch].local = true` 后改为本地逐跳校验并直接抓取（HTML 转文本、
  2 MiB/字符上限）。敏感目标优先用 local。
- 下载、本地抓取与发送时内联媒体都直连，不走系统代理，重定向最多 3 跳。

## 数据与密钥

- 消息 JSONL、transcript、blob 内容用 XChaCha20-Poly1305；不同用途的密钥由
  HKDF 从主密钥派生。
- `[storage].encryption` 管消息归档、对话记录与回合缓冲（VFS 始终加密）。密文在任何配置下
  都读得出；明文只在配置为 `none` 时读——配置为加密时，一行没有认证的明文不会被当成历史，
  所以能写数据目录但拿不到密钥的人伪造不了对话记录或消息。
- `~/.sbot/key`（主密钥，权限 0600）与 `auth.json`（明文 API key / token）是两份秘密；
  `sbot backup` **两份都包含**，备份目录按机密文件保管（Unix 下权限 0700）。
- 日志写出前遮蔽 `access_token=…` 与 `Bearer …`（NapCat 连接地址会带 token）；`auth.json`
  解析失败只报错误类别与行列，不引用原文。
- 控制通道：Windows 命名管道的安全描述符只允许创建者、SYSTEM 与 Administrators 组，不给
  Everyone / 普通用户；Unix 套接字放在只有当前用户能进入的目录（`$XDG_RUNTIME_DIR` 或
  `<SBOT_HOME>/run`），不放共享的临时目录。
- `sbot.toml` 里唯一可能带密钥的是 `[[mcp.servers]].env`：agent 的配置视图遮蔽这些值；
  agent 写配置时临时文件只给属主读写，rename 前复制原文件的权限，不会放宽 `0600`。
- 索引（SQLite）与路径是明文元数据：能看出“谁在什么时候聊过”，看不到消息内容。数据库里
  还有两样明文：安全事件的原因（`security_events`），待审批动作的摘要与确认码
  （`gate_approvals`）。
- 发送账本 `outbound_intents` 在启动时把久悬的 `pending` 标为 `unknown`；发送超时
  也不会被当成“肯定没发出去”，避免重复发送。
- 威胁边界：不防同机管理员、不防进程内存读取、不防拿到 key 的人。

## 已知边界

| 项 | 现状 |
|----|------|
| `approve` 的确认通道 | 本地 IPC，或向 master QQ 发 nonce；待审批队列持久化在 `gate_approvals`，重启不丢 |
| `auto` 的发送规则 | 频率/重复/跨群同文/@all/5-gram/context_sources/媒体来源是启发式；Grep 不按来源拆分 |
| 周期检查的覆盖范围 | 只看最近一段上下文（见上）；检查失败按放行处理，那一段不补查 |
| 群级破坏动作 | 没有用户目标，靠 owner intent 而不是硬保护 |
| owner intent 的归属 | 只看本回合进入的消息；更早的上下文（pending history、之前的回合）里别人的话不会撤销 intent |
| WebFetch | 后端已经抓取后才复核 `final_url` |

启用前请不要把这些当成已有的防护。其它方面的限制见 `sbot docs sbot-operate limits`。
