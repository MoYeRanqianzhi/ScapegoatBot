# Changelog

本文件记录 ScapegoatBot 的所有重要变更。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [0.2.0-alpha.34] - 2026-11-04

补齐事件审计、图片自动注入与剩余审计项。

### 新增

- `/system/events`：最近 100 条 notice/request/meta/unknown 原始事件（加密落库）。
- 入站 QQ 图片在唤醒回合内自动下载为 image prompt（仅 QQ 图片域名，≤3.5 MB）。
- `media.image_watermark`。
- `media.image_rotate` / `image_thumbnail` / `image_compress`。
- MCP image block 进入 `ToolResult.images`。

### 修复

- 唤醒预设重新走 `ChatQueue`，恢复去抖/并发/`idle_only`。
- 30 秒逐 chunk 流空闲超时。
- 旧 transcript 行号偏移自动尝试恢复。
- runtime context 不再写入 transcript。
- 截断 tool args 不再以 `{}` 执行；重试 jitter；`Retry-After` 不截断。
- session 创建改为 `create_new` 原子占位。
- monitor batch flush 竞态；移除不可达的 `cap_batches` 分支。

## [0.2.0-alpha.33] - 2026-11-04

用真实 API usage 估算上下文，避免图片把上下文撑爆。

### 修复

- `estimate_messages` 优先使用最近一次 assistant 响应的
  `input/output/cache_read/cache_write` 作为前缀基准，只估算尾部消息；
  无 usage 时才退回逐条估算。
- 图片仍按 Claude Code 的保守 2000 tokens/张计入尾部；实际计费以 provider
  返回的 usage 为准。
- 避免“图片 base64 很大但估算为 0”导致自动压缩触发过晚。

## [0.2.0-alpha.32] - 2026-11-04

图片附件随 transcript 持久化，重启后仍可恢复。

### 变更

- `ToolResult` / `ToolResultMessage` 的 `images` 不再 `skip_serializing`；非空时
  写进加密 transcript 与 turn buffer，`read_segment` 会还原 base64 image。
- 图片大小仍受 `Read` 的 3.5 MB / MIME 限制，避免单条 transcript 无限膨胀。

### 测试

- `cargo test --all-targets`：345 项通过；`clippy -D warnings` 通过。

## [0.2.0-alpha.31] - 2026-11-04

- 同步 `Cargo.lock` 版本，修复 `cargo build --release --locked` 失败。
- 合并 alpha.30 的回合串行化、transcript 行号、Chat 流截断与图片链路修复。

## [0.2.0-alpha.30] - 2026-11-04

按 pi / Claude Code 对照审计修复回合与图片链路。

### 修复

- 主 agent 回合加全局互斥：shared transcript 下控制、monitor、普通回合不再并发交错；
  `Monitor fire` 改为后台投递，避免在工具栈里重入回合锁。
- transcript append/read 按分段串行化，行号写入加密信封；修复并发 append 导致的
  AAD 行号错位和“skipping unreadable transcript line”。
- Chat 协议流没有 `finish_reason` 时不再伪造 `EndTurn`，改为 Network 错误，避免
  半截响应被当成成功提交。
- `run_injected` 计入 daily budget，monitor/control 回合不再绕过预算。
- 图片：Chat 协议真正发送 `image_url`；`Read` 限制 3.5 MB 与 png/jpeg/gif/webp；
  `media.image_resize`/`crop` 限制 8192 并做 checked 边界；图片计入 token 估算，
  旧工具结果裁剪时清理图片附件。
- `API` 返回值中的 base64/image 自动转成图片附件；描述补齐 `media.*` 第四个命名空间。

### 测试

- `cargo test --all-targets`：344 项通过；`clippy -D warnings` 通过。

## [0.2.0-alpha.29] - 2026-11-04

消息不再逐条唤醒 agent，改为 inbox + 可删除唤醒预设。

### 变更

- 所有非 self、非 backfill 的 QQ 消息只写入 `/inbox` 与 transcript，不再进入
  队列逐条触发回合，避免消息洪峰把 agent 拖进无休止的回复。
- 启动时安装两个可删除的 Monitor 预设：
  - `wake-on-mention`：非主人的 @ 消息；
  - `wake-on-owner`：`owner_qq` 的消息。
  删除会写入 `monitor.removed.<name>`，重启不再恢复；手动 `Monitor add` 同名可恢复。
- Monitor 谓词新增 `event.sender != 'qq'`，用于避免主人 @ 时两个预设重复唤醒。
- `MonitorEvent` 的 `mentions_self` 现在按登录账号判断，不再把任意 @ 当成 @ 自己。
- `Read` 读取 VFS 图片时返回 image attachment；工具结果新增图片通道，Chat 协议
  转成 `image_url`，Anthropic/OpenAI 走各自的 image block。
- `API` 新增 `media.*`：`image_info`、`image_resize`、`image_crop`、`image_convert`；
  NapCat/扩展 action 响应中的 `base64`/`image` 也会自动转成图片附件。
- 主人消息触发 monitor 时，投递带 `owner_intent`，回合拥有 owner 证据，不会被
  `send_private_msg` 的 owner-intent 闸拦下；这是之前“图片消息不回复”的直接原因。
- QQ notice（拍一拍、撤回等）、好友/群请求、带 chat 字段的未知事件都转成
  inbox 伪消息；正文含摘要与压缩原始 JSON，`raw` 完整保留。
  monitor 的 `event.text` 也能看到 `[poke]` / `[recall]` 文本。

### 测试

- `cargo test --all-targets`：350 项通过；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.28] - 2026-11-04

修正 Chat 协议的 reasoning effort 映射。

### 修复

- 之前把 `max`/`xhigh` 一律向下映射成 `high`，注释还写成“Chat 只有
  low/medium/high”。实际取值由供应商决定：OpenAI 是 `low/medium/high`，
  DeepSeek 是 `low/high/max`。
- `apply_reasoning` 现在直接透传模型声明的 `low/medium/high/max`，不再猜供应商；
  `thinking.enabled = false` 时也不发送。
- 测试改为断言 DeepSeek 的 `max` 原样到达请求体。
- 新增 `[agent].retry_mode`：`stepped`（默认，base、2×base…封顶 `retry_max_ms`）
  或 `fixed`（每次固定 `retry_base_ms`）；`Retry-After` 仍优先但受上限约束。

### 测试

- `cargo test --all-targets`：347 项通过；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.27] - 2026-11-04

修复 Windows checkout 的 CRLF 导致 companion 提示词丢失。

### 修复

- `static_prefix` 在首次渲染时把模板的 `

` 归一化为 `
`；此前 Windows
  CI 上 `render_static(Companion)` 找不到 `

`，会把身份之后的全部提示词截掉。
- 新增 `.gitattributes`，源码/文本文件统一 `eol=lf`，避免 Windows checkout
  再引入 CRLF。
- alpha.26 的 Windows 产物受该问题影响；alpha.27 为修复版本。

### 测试

- `cargo test --all-targets`：347 项通过；`cargo fmt --check`、
  `clippy -D warnings` 通过；CI 的 Windows job 是本次修复的回归验证。

## [0.2.0-alpha.26] - 2026-11-04

发布构建与流水线。

### 构建

- `[profile.release]` 改为性能优先：`opt-level = 3`、fat LTO、单 codegen unit、
  `panic = "abort"`、`strip = "symbols"`。本机 Windows 上体积从 17.17 MiB
  降到 10.19 MiB。
- 新增 `.github/workflows/release.yml`：推送 `v*` 标签时在 Linux/Windows 原生
  runner 上 `cargo build --release --locked`，上传二进制与 sha256，并创建
  GitHub Release；也可手动 `workflow_dispatch`。

## [0.2.0-alpha.25] - 2026-11-04

用模拟流量补长跑覆盖，关闭剩余 TODO。

### 测试

- 新增 `soak_many_turns_stay_bounded_with_compaction`：fake provider 下连续 300 个
  回合、持续触发压缩，断言历史长度保持上限内、无 panic；覆盖压缩/轮转/提交路径。
- MSRV 记录为 Rust 1.85（edition 2024）；`napcat-link` 固定 0.1.0-alpha.2。
- `cargo test --all-targets`：347 项通过（335 lib + 4 config + 1 live stub +
  7 NapCat，2 项真实 API 测试默认忽略）；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.24] - 2026-11-04

补齐最后两个非真实 API 项：审查前缀缓存与剩余 hook 注入。

### 新增

- **周期检查复用主 system 前缀**：`SecurityReviewer` 接收 Runner 当前的 system
  块，审查请求以相同前缀开头，provider 可命中 prompt cache；消息窗口仍独立。
- **剩余 `inject_context` 落点**：
  - Stop → 作为 System 消息进入本轮提交（下一轮可见）；
  - SessionEnd → 清空后写入新 history/session；
  - ConfigChange → 写入内存 history（下一次压缩落盘）。

### 测试

- 新增：审查请求的 system[0] 与主前缀一致且 `cache = true`。
- `cargo test --all-targets`：346 项通过（334 lib + 4 config + 1 live stub +
  7 NapCat，2 项真实 API 测试默认忽略）；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.23] - 2026-11-04

补齐 `context_sources`：按内容来源而不是目标 chat 判断跨 chat 泄露。

### 新增

- `TurnEvidence` 携带本轮上下文片段及来源 chat：触发批次、pending history、
  `Read /messages|/inbox`、`API get_*_msg_history` 都会记录（最多 32 条、每条 8k 字符）。
- `auto` 门禁发送前把外发内容与来源 chat 不同的片段做 5-gram 比对；共享 5 个以上
  即拒绝，owner intent 或发回来源 chat 放行。
- 工具执行器在执行后根据 `Read` 路径 / `API` 参数自动登记来源，不需要模型自报。

### 测试

- 新增：上下文来源拦截与 owner/来源 chat 放行、Read/API 来源解析。
- `cargo test --all-targets`：345 项通过（333 lib + 4 config + 1 live stub +
  7 NapCat，2 项真实 API 测试默认忽略）；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.22] - 2026-11-04

把 NapCat 模拟测试扩到全部 `napcat.*` action。

### 测试

- 新增 `every_napcat_action_round_trips_over_the_mock`：按 action 注册表的参数
  schema 生成最小请求，逐个经 `API` 工具发到本地 OneBot 11 模拟服务，断言：
  - action 名去掉 `napcat.` 后原样出现在 WebSocket 请求里；
  - 参数 schema 是对象；
  - 所有 action 都能完成一次请求/响应（含发送、查询、管理、文件、请求处理）。
- 模拟服务只在 `get_group_info` 且 `group_id = 404` 时返回失败，正常 action 全部成功。
- `cargo test --all-targets`：343 项通过（331 lib + 4 config + 1 live stub +
  7 NapCat，2 项真实 API 测试默认忽略）；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.21] - 2026-11-04

让 `Agent message` 真正进入运行中的子代理。

### 新增

- 子代理实例持有消息通道；`AgentHub::message` 在运行中把消息送进当前回合，
  `AgentLoop` 在回合边界排空，不再只堆在实例历史里等下一次运行。
- 子代理结束后把完整历史（含注入消息）写回实例，续聊不会丢上下文。
- 运行结束后清理通道；发送失败时回退为入历史并返回 `queued`。

### 测试

- 新增：运行中的子代理在 50ms 内收到 `Agent message`，响应标记 `delivered`。
- `cargo test --all-targets`：342 项通过（331 lib + 4 config + 1 live stub +
  6 NapCat，2 项真实 API 测试默认忽略）；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.20] - 2026-11-04

新增 WebFetch 的本地抓取模式，消除“先由后端取内容、再复核 URL”的窗口。

### 新增

- **`[tools.web_fetch].local = true`**：直接 GET，逐跳校验重定向（最多
  `max_redirects`），每一跳都走 scheme/域名/公网地址检查与公共 DNS 解析器；
  读取上限 2 MiB 或 `max_content_tokens × 4` 字符。
- HTML 转文本：去掉 `script`/`style`/标签，解码常见实体并折叠空白。
- 本地模式不要求 `base_url` 或 API key；默认仍为后端模式，行为不变。

### 测试

- 新增：HTML 转文本、302 跳转 + HTML 抓取（本地 TCP 服务器）、内容类型/最终 URL。
- `cargo test --all-targets`：341 项通过（330 lib + 4 config + 1 live stub +
  6 NapCat，2 项真实 API 测试默认忽略）；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.19] - 2026-11-04

补齐 `auto` 门禁的内容级规则；数据库迁移到 v11。

### 新增

- **跨 chat 5-gram 重叠**：长文本（≥24 字）在 10 分钟内与发往其他 chat 的内容共享
  3 个以上 5-gram 时，视为搬运/转发，要求 owner intent；短句与正常短回复不受影响。
- **媒体来源**：新表 `media_origins` 记录图片/文件/语音/视频第一次出现的 chat；
  转发到其他 chat 需要 owner intent，发回来源 chat 不需要。
- 规则顺序：@all → 媒体来源 → 重复/跨群同文 → 5-gram 重叠，全部在成功发送前。

### 测试

- 新增：5-gram 拦截与 owner 放行、媒体跨 chat 拦截、来源 chat 放行。
- `cargo test --all-targets`：339 项通过（328 lib + 4 config + 1 live stub +
  6 NapCat，2 项真实 API 测试默认忽略）；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.18] - 2026-11-04

补齐 `approve` 的第二个确认通道与持久化；数据库迁移到 v10。

### 新增

- **master 私聊 nonce**：`approve` 模式登记请求后，API 通过 NapCat 私聊 `master_qq`
  发送 6 位 nonce；主人在该私聊回复 nonce 即批准。只接受 owner/master 的私聊、
  非 self、非补消息；不进入 agent 回合。
- **审批持久化**：新表 `gate_approvals` 保存 action、摘要、参数哈希、nonce、状态与
  时间戳，不保存消息原文；重启后未过期的 pending/approved 记录仍可消费一次。
  批准窗口 120 秒、pending 60 秒不变。
- **请求 id 防重启碰撞**：改为 `g<毫秒>-<纳秒>-<计数器>`，避免进程计数器归零后覆盖
  数据库中仍有效的旧请求。

### 测试

- 新增：nonce 批准与一次性消费、重启后按参数哈希恢复、API 私聊通知、以及
  NapCat 本地 WebSocket 上的完整 nonce 往返（通知 → 批准 → 成功发送）。
- `cargo test --all-targets`：337 项通过（326 lib + 4 config + 1 live stub +
  6 NapCat，2 项真实 API 测试默认忽略）；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.17] - 2026-11-04

补完剩余事件 hook 与配置字段，并新增本地 NapCat 协议模拟测试。

### 新增

- **全部 `[hooks.events]` 事件触发**：SessionStart/End、Pre/PostCompact、
  MessageReceived、BeforeSend、Stop、ConfigChange。`log`/`block`/`inject_context`/
  `notify` 生效；`inject_context` 落到 SessionStart（system）、MessageReceived（回合提示）、
  Pre/PostCompact（摘要输入/新分段）；`notify` 支持 owner/master 私聊。
- **`guard.max_state_tokens`**：用于截断闸看到的 state（不再固定 32k 字符）。
- **`models.json.default_thinking_level`**：`[agent].thinking_level` 为空时作为默认 effort。
- **NapCat 协议集成测试**：本地 WebSocket 模拟 OneBot 11/NapCat 的
  `{action, params, echo}` 与 `{status, retcode, data, echo}`，覆盖登录、版本、群/私聊发送、
  API 工具请求形状、失败响应与事件推送；测试按 napcat-link 源码的请求/响应结构实现。

### 变更

- 删除无效字段 `security.guard.delta_every_tokens` 与 `style.sample_retention_days`
  （严格 schema，旧配置含这两个键会报未知字段）。
- `QqClient::call` 的错误信息现在包含 NapCat 返回的 message，而不是只留方法名。

### 测试

- `cargo test --all-targets`：334 项通过（324 lib + 4 config + 1 live stub + 5 NapCat，
  2 项真实 API 测试默认忽略）；`cargo fmt --check`、`clippy -D warnings` 通过。

## [0.2.0-alpha.16] - 2026-11-04

把上一轮列出的“配置写了但不生效”补齐。数据库无新迁移。

### 新增

- **`[budget]` 真正执行**：每日 token/USD 在回合开始检查，超限跳过新回合（消息已入库）；
  `turn_tokens` / `round_trip_wall_secs` 在循环内硬停；`chat_sends_per_hour` 只统计
  成功发送；`chat_turns_per_hour` 限制每 chat 新回合；`guard_usd_per_day` 用估算成本，
  耗尽后分类器降级为规则层（规则仍生效）。
- **`[hooks.events]` 的 BeforeSend**：`log`、`block`、`rewrite` 接线到 API 发送路径，
  支持 `group_id` / `user_id` / `message_type` 匹配与 `scope = model|all`。
- **`[tools].max_schema_tokens`**：超限时把过长描述压到 160 字符；仅 schema 超限则
  告警而不删工具。
- **monitor `cc` / `on_missing`**：绑定子代理的投递可抄送主代理；目标缺失时按
  `drop` / `queue`（最多 60 秒）/ `spawn`（新建 general 子代理）处理。
- **`get_msg` 的读范围**：通过消息索引反查 chat，查不到且在限制读取时保守拒绝。

### 文档

- `limits.md` 只保留仍存在的边界：BeforeSend 之外的事件 hook、`max_state_tokens`、
  周期检查缓存、WebFetch 后端抓取、approve 的 IPC-only 确认等。

### 测试

- 新增：预算累计/USD/guard 降级、每 chat 发送上限、BeforeSend block/rewrite/匹配、
  schema 预算、monitor cc、get_msg 范围。
- `cargo test --all-targets`：327 项通过（2 项真实 API 测试默认忽略）；
  `cargo fmt --check`、`clippy -D warnings` 通过。

## [0.2.0-alpha.15] - 2026-11-04

第二轮审查：修掉 alpha.14 新代码里的边界问题，并把两个“配置写了但不生效”的
安全/资源项接上。数据库无新迁移。

### 修复

- **记忆复盘在压缩后停摆**：压缩会把 transcript 截短，`last_reviewed` 不回落时
  会永远认为“没有新增”；长度回退时重置基线。
- **发送记录早于结果**：门禁在调用 gateway 前就写入“已发送”，一次超时会让重试被
  重复规则挡住。改为只在成功后 `note_send`，失败/超时不再污染记录；短应答
  （<12 字符）不参与同 chat 重复判定。
- **主人通知被误拦**：给 `master_qq` 的私聊是通知通道，现在即使没有 owner intent
  也放行（仍受发送范围、门禁与 hold 约束）。
- **超时只包住单次读取**：安全审查/LLM 判定在事件持续到达时可以绕过总超时；
  三个循环都增加固定 deadline 检查。
- **模型未声明上下文窗口**：`context_window = 0` 不再把压缩阈值压到 1，回退到
  `turn_context_budget`。
- **保留天数未执行**：`message_retention_days` / `session_retention_days` 之前只有
  `/system` 展示。消息在 append 时、会话在 rotate/启动时真正清理过期分段。
- **工具结果无上限**：`[tools].max_result_bytes` 现在截断单次工具结果（按字符边界），
  避免一次 Read/Grep 撑爆上下文。
- **`receive_scopes` 只在 prompt 里**：消息/收件箱/联系人视图与带 `group_id`/
  `user_id` 的 API 查询现在按范围过滤；越界读取返回 NotFound。

### 文档

- `limits.md` 增加仍未执行的 `[budget]`、`[hooks.events]`、`max_schema_tokens`、
  monitor `cc`/`on_missing`，以及 `get_msg` 无法按 chat 过滤的边界。

### 测试

- 新增：压缩后复盘基线、成功/失败发送与短应答、master 通知、保留天数、
  工具结果截断、视图与 API 的 receive scope。
- `cargo test --all-targets`：316 项通过（2 项真实 API 测试默认忽略）；
  `cargo fmt --check`、`clippy -D warnings` 通过。

## [0.2.0-alpha.14] - 2026-11-04

补齐 alpha.13 文档里列出的未实现项，并完成结构收尾。数据库无新迁移。

### 新增

- **动作门禁 `[gate]`**：`trusted` / `auto` / `approve`；每分钟速率上限、
  同 chat 重复发送、跨群同文、无 owner intent 的 @全体；可选 LLM 审查（失败 fail-closed）。
  `approve` 返回待确认 id，`sbot gate pending/approve/reject` 经 IPC 裁决，
  批准后 120 秒内可重试一次，60 秒未确认失效。
- **owner intent 证据**：runtime 生成 `TurnEvidence`（控制通道 / 主人消息 / origin chat），
  经工具执行器传给 `API` 并在子代理间继承。跨 chat 发送、加好友、改资料、上传文件、
  改群名片/群名/授予权限/发公告需要证据；收缩动作与同 chat 回复不需要。
- **`backend = "llm"` 安全分类器**：复用主 provider 做输入/输出判定；`Gate` 支持在
  provider 就绪后后置注入判定器，装入前 enforce 保持 fail-closed。
- **周期安全检查**：`check_every_tokens` 在回合边界用 `review_model`（默认主模型）
  检查注入/社工与是否已照做；`high + complied` 进入 hold，`critical` 暂停新回合
  （重启从 hold 文件恢复）；超时放行，连续失败告警。
- **压缩调优**：按模型窗口减 `reserve_tokens` 计算阈值；旧工具结果按
  `prune_keep_turns` / `prune_soft_limit` 裁剪；压缩前 `memory_flush`；
  压缩后 `post_compact_recall` 注入记忆索引；`compaction_model` 可用独立模型。
- **记忆复盘语义**：`idle` 使用 `idle_minutes`，`every_turn` 按 transcript 增量触发；
  与压缩前 flusher 共用同一套提取/落盘逻辑。
- **monitor catch_up**：定时源把上次触发时间写进 `meta`，重启后 `coalesce` 补投一次
  （并说明错过次数），`skip` 放弃；运行时创建的 monitor 不参与。
- **出站账本核对**：启动时把久悬 `pending` 标为 `unknown`；发送超时/断线也标
  `unknown` 并提示先查历史，避免重复发送；`status` 暴露未确认数量。
- **网络加固**：`Download` 使用只返回公网地址的自定义 DNS 解析器（关闭 rebinding
  TOCTOU）；`WebFetch` 复核后端返回的 `final_url`。
- **群级破坏动作**：全员禁言、退群、解散群会改变主人所在群的可达性，改为要求
  owner intent（没有用户目标可做硬保护）。
- **MCP 分页**：`tools/list` 达到 10 页上限时告警，不再静默截断。

### 变更

- `tools/views.rs` 拆为 `views/{messages,inbox,sessions,contacts,config,system}.rs`。
- `Runner` 的 per-chat 缓存收进 `ScopeCache`；`Runtime::start` 的存储初始化与启动对账
  抽到 `open_stores`。
- 全仓 `cargo fmt`；新增 GitHub Actions CI（Ubuntu 上 fmt/clippy/tests，Windows 上
  clippy/tests）。
- 文档同步：`docs/guide` 的配置/安全/限制/使用/记忆页与 README 更新；
  `limits.md` 只保留仍存在的边界。

### 修复

- 发送失败不再被标记为 `failed`（那会诱导重试），改为 `unknown`。
- `sbot status` 增加 `gate` 与 `unconfirmed_sends`。

### 测试

- 新增：动作门禁（速率/重复/跨群/@all/审批/LLM 审查）、owner intent 放行与拒绝、
  LLM judge 安装、周期检查的 hold/pause、工具结果裁剪、monitor 补投计数、
  账本核对、DNS 解析过滤、views 拆分后的既有用例。
- `cargo test --all-targets`：311 项通过（2 项真实 API 测试默认忽略）；
  `cargo clippy --all-targets -- -D warnings` 与 `cargo fmt --check` 通过。

## [0.2.0-alpha.13] - 2026-11-04

审查驱动的一轮修复与文档建设。没有破坏性配置变更；数据库迁移追加到 v9。

### 修复

- **压缩**：自动压缩后切片基点改为在压缩之后取，修复越界 panic 与旧块重复提交；
  自动压缩失败按 `[compaction].max_failures` 停止重试；压缩成功后重新读取 hooks 并
  重建 system prompt；控制/monitor 注入的回合也走同一套压缩。
- **消息归档**：append 串行化，行号不再竞争；读取时校验索引行号与行内 id；
  按文件头记录的算法解密；`rebuild_index` 同时重建 `message_files` 与 `inbox_state`；
  保留清理删除孤儿未读记录。
- **会话恢复**：`turn_buffer` 增加 `scope`（迁移 v9），per_chat 只重放自己的回合，
  不再把崩溃回合写进别的 chat。
- **Provider**：SSE 字节流增量 UTF-8 解码，跨 chunk 的中文不再变成 U+FFFD；
  `

` 跨 chunk 也能正确切事件；工具参数先于 start 到达时不再丢弃。
- **安全**：`enforce` 但没有分类器时按 fail-closed 处理（每个回合 hold，启动与
  doctor 提示）；规则命中不再被概率阈值吞掉；`agent.resume` 用真实被判内容复核；
  hold 同时拒绝 Download / Web / MCP；`backend = "llm"` 在配置校验阶段明确拒绝。
- **母语/QQ**：准入过滤提前到 monitor 之前；`delete_msg` / `set_msg_emoji_like`
  恢复可用；`send_msg` 按群发送可用；owner 保护接受字符串 QQ 号、解析失败即拒绝；
  全员禁言/退群/解散不再被误当成用户目标。
- **工具**：`Copy` 受配额约束；`Grep` / `Stat` 不再标记已读；MCP stdio 单行与
  结果有大小上限；SSRF 过滤覆盖 IPv4-mapped / 6to4 / NAT64 / CGNAT。
- **子代理**：`docs` 与未开 `allow_send` 的自定义定义拿到只读 `API`，只能
  `api.list` / `api.help`；工具参数不合法时不再把 `input: null` 写进历史；
  error/refusal 路径补齐未执行的工具结果。
- **队列**：等待全局并发时不持 per-chat 状态锁；修复批次预约计数泄漏。
- **配置**：`agent.compaction_model` 默认改为空（表示用主模型），修复默认值与
  最小配置无法通过校验、导致 `sbot start` 直接失败的问题。

### 变更

- 配置 schema 拆到 `config/sections.rs`，API action 表拆到 `tools/actions.rs`，
  `app::build_agent` 的工具装配拆为 `assemble_tools`；行为不变。
- 移除未使用的 `AgentEvent::TurnEnd` / `ToolExecutionUpdate` 与
  `ModelDef::map_thinking_level`。
- 根目录旧资产 `prompt.md`、`llm.example.toml` 移到 `docs/archive/0.1/`。

### 文档

- 新增 `docs/guide/`：安装、配置、使用、人设与记忆、工具与 VFS、安全、故障排查、
  当前限制。
- `README.md` 重写并与代码对齐；`config.example.toml` 改为可运行的 0.2 样例，
  由 `tests/config_example.rs` 校验。
- 开发文档更新模块地图（迁移 v9、config/sections、tools/actions）与结构约定。

### 测试

- 新增回归测试：自动压缩、并发 append、索引错位、文件头加密、turn buffer 作用域、
  SSE UTF-8 分片、闸的 fail-closed/阈值/resume、只读 API、工具配对、队列预约、
  复制配额、Grep 不标已读、MCP 行上限、示例配置校验。
- `cargo test --all-targets`：289 项通过（2 项真实 API 测试默认忽略）；
  `cargo clippy --all-targets`：无警告。

## [0.2.0-alpha.12] - 2026-10-30

0.2 是完整重写：旧 0.1 的模块划分、数据结构与调用方式全部放弃，按
`.agents/plan/2026-10-28-rebuild-design.md` 重新实现。量化对比见
`docs/development/refactor-report.md`（复用 0 行，重写 97.5%，废弃 2.5%）。

### 新增

- **配置与凭据**：严格 `sbot.toml` schema（`version = 1`、未知字段拒绝、字段路径校验）、`SBOT__SECTION__KEY` 覆盖、`models.json` 模型清单、明文优先的 `auth.json`（掩码 Debug）。
- **存储**：SQLite 迁移 v1–v8、主密钥 `~/.sbot/key`、HKDF 域分离、XChaCha20-Poly1305、canary 校验、`SBOT_HOME`。
- **VFS**：POSIX 路径与 NFC、文本加密、1 MiB blob 分块、回收站、Read-before-Edit、配额与磁盘水位、种子布局。
- **Provider**：Anthropic Messages（自适应 thinking、effort）、OpenAI Chat Completions、OpenAI Responses、SSE 解析、工具参数按 index 组装、`Retry-After`、按模型回退链。
- **Agent 循环**：pi 风格内/外循环、事件流、steering / follow-up、配对的工具结果、`max_tokens` 带工具时不执行、输出闸、parallel/sequential 工具执行、refusal 进入 hold。
- **QQ**：NapCat 连接、事件归一化、内部标签剥离、准入门（拒收）、断线重连补消息（不触发回合）、JSONL 按群/私聊/月/日分区、索引重建、inbox 状态、发送账本。
- **安全**：规则层 + Jev `/v1/systemone` 分类器、永久 hold、`SECURITY_HOLD`、安全事件审计、主人/管理员目标保护、hold 期间拒绝全部 `napcat.*`。
- **会话**：加密 transcript 分段与 `prev_file` 链、turn buffer 崩溃恢复、CJK 感知压缩、回合边界切割、加密 `compaction_checkpoints`。
- **工具**：文件工具、动态视图（`/messages`、`/inbox`、`/sessions`、`/friends`、`/groups`、`/config`、`/system`）、`API` 命名空间、`Memory`（含 `memory_entries`）、`Skill`、`Download`（SSRF）、`WebSearch` / `WebFetch`、`Monitor`（qq/timer/internal、cron、batching、idle_only、backlog）。
- **子代理（swarm）**：`Agent` 工具的 spawn / message / check / wait / list；`general` / `explore` / `plan` / `docs` 内置类型；`/agents/*.md` 自定义定义；工具集强制为父代理子集；并发上限；独立上下文。
- **MCP**：可选、默认关闭；stdio JSON-RPC `initialize` / `tools/list` / `tools/call`；单个 `MCP` 工具；服务器启动失败只告警。
- **提示词**：v4 静态/动态边界模板、context hooks、每回合 runtime 上下文。
- **IPC**：Windows 命名管道（DACL）/ Unix UDS（0600）、长度前缀 JSON、幂等表。
- **CLI**：`start/stop/restart/status`、`ai`、`chat`、`pause/resume`、`security`、`model`、`effort`、`backup`、`doctor`。
- **备份与诊断**：`sbot backup` 在线 SQLite 快照（Busy/Locked 重试）+ 文件归档，拒绝写入被复制目录；`sbot doctor` 只读检查配置、canary、索引、孤儿 blob、记忆三方对账、权限与路径边界。
- **模型热切换**：`sbot model set` / `effort set` 通过共享 provider 槽位在下一回合生效。

### 变更

- CLI 行为与 0.1 不兼容；不提供旧数据迁移，0.2 从空存储开始。
- 文件能力改为 VFS，不再使用 `cap-std` 工作区。
- 白名单语义改为**拒收门**：被拒消息不进入任何存储。

### 已知限制

- **NapCat 协议实证**：修复 `call_api` 返回 `data` 的解包错误；新增 `tests/live_napcat.rs`（真实 WebSocket 收发）与 `examples/mock_onebot.rs`；完整管线用真实模型回复群消息并发送成功。
- **后台复盘**：空闲时从 transcript 提取记忆候选（只追加、来源 `self`）、更新 `/STYLE.md`；技能使用记录 + curator 归档长期未用技能（不删除，`pinned` 跳过）。
- **`per_chat` 会话隔离**：每个 chat 一条独立 transcript 链与历史，压缩/轮转/保留/崩溃恢复按 chat 执行；迁移 v8 增加 `scope` 列。
- **子代理嵌套**：`max_depth` 深度限制；`Agent message` 续聊复用原定义的提示、工具集与步数；`Agent spawn` 的 `monitor` 参数与 `deliver.to = <spawn_id>` / `agent:<name>` 把事件绑定到子代理实例。
- `/config` 写入支持白名单键并原子落盘；`[filter]` 与 `[queue]` 写入立即热更新（`max_concurrent` 的信号量上限需重启）。
- 真实 Chat 端点与真实 Jev 的测试默认 `#[ignore]`，需要环境变量显式启用。

## [0.1.0-alpha.2] - 2026-03-23

历史版本（0.1 系列）。以下内容已被 0.2 取代，保留用于对照。

### 新增

- **蜂群子代理系统** — 主 Agent 可通过 SpawnAgent 生成子代理并行执行任务
  - SwarmManager 管理子代理完整生命周期（创建、消息传递、状态查询、等待完成）
  - SubAgentRunner 轻量级 agentic 循环（独立 tokio 任务，独立 LLM 对话）
  - 子代理可嵌套 spawn 子代理，深度受 `swarm.max_depth` 限制
  - 完成通知机制：子代理通过 SwarmNotification 唤醒主代理
- **4 个蜂群工具** — SpawnAgent / MessageAgent / CheckAgent / WaitAgent
  - SpawnAgent：生成子代理（支持指定备选模型和额外上下文）
  - MessageAgent：向运行中子代理发送消息
  - CheckAgent：查询子代理状态（支持单个或全部列出）
  - WaitAgent：等待子代理完成并获取结果（支持单个/多个/全部）
- **备选模型配置** — `[[models]]` 段定义子代理可用的 LLM 模型
  - 支持不同提供商、API 地址、上下文窗口等独立配置
  - 可选字段为空时自动继承主 `[llm]` 配置
- **蜂群配置** — `[swarm]` 段控制子代理系统安全参数
  - `enabled`: 功能总开关（默认 true）
  - `max_depth`: 最大嵌套深度（默认 32）
  - `max_concurrency`: 最大并发子代理数（默认 1024）
- **MCP 支持** — 通过 Model Context Protocol 接入外部工具服务器
  - Stdio 传输层（子进程 stdin/stdout JSON-RPC）
  - 工具自动发现，命名格式 `mcp.<server>.<tool>`
  - 配置: `[mcp.<name>]` 段
- **Fetch 工具** — Agent 网络访问能力
  - HTML 自动转换为 Markdown
  - SSRF 防护：私有 IP 地址拦截
  - 域名白名单/黑名单过滤
  - 默认禁用，需在 `[fetch]` 段显式启用
- **群管理工具** — 18 个群组管理工具（禁言/踢人/公告/精华等）
- **管理员状态同步** — 启动同步 + 事件驱动 + 定时轮询
- **开发文档** — architecture.md, tools-reference.md

### 改进

- **ToolDispatcher 提取重构** — 工具分发逻辑从 AgentRunner 提取为独立的 ToolDispatcher，主 Agent 和子 Agent 共享同一套工具调用能力
- **Edit 工具** — 改为 Claude Code 模式（Read 一次后可多次 Edit）
- **CLI 命令注入** — Codex 风格消息插队
- **消息自动标记已读** — 首次 LLM 请求成功后标记
- **消息段 Schema** — ToolSpec 详细描述每种消息段类型及其 data 字段

### 修复

- 修复 cmd_rx 通道关闭后 CPU 100% 空转的 busy-spin bug

## [0.1.0-alpha.1] - 2026-03-10

首个 Alpha 版本，核心架构就绪。

### 新增

- **Agent 核心循环** — 消息去抖 → LLM 调用 → 工具分发 → 多轮对话
- **OpenAI 兼容提供商** — 支持 OpenAI、DeepSeek、Moonshot 等所有兼容 API
- **原生工具调用** — 16 个内置工具（Read/Write/Edit、消息收发、群友查询、好友请求处理、Skill 加载）
- **cap-std 安全沙箱** — 文件操作硬限制在 workspace 内，含 7 层路径校验
- **写入安全防护** — 单文件 1 MB 上限、磁盘空间检查（100 MB 底线）、工作区容量限制（2 GB / 50000 文件）
- **上下文压缩** — 对话超过 token 阈值时自动压缩历史为摘要
- **收发分离白名单** — 精确控制 AI 可见范围和发言范围
- **Skill 系统** — 通过 SKILL.md 注入技能，每个 skill 目录独立沙箱
- **消息存储** — 按时间线统一存储（JSONL + Markdown），支持文件拆分
- **系统提示词构建** — 10+ 节结构化 prompt，支持 AGENTS.md / SOUL.md 人格注入
- **CLI 进程管理** — `sbot start/stop/status/run` 守护进程模式
- **控制通道** — TCP 端口控制 Agent 暂停/恢复/重载

### 依赖

- NapCatLink v0.1.0-alpha.2（OneBot 11 WebSocket SDK）
- cap-std 3（能力安全文件系统）
- fs2 0.4（磁盘空间查询）
- tokio, reqwest, serde, chrono, clap, tracing 等
