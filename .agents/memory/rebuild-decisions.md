---
name: rebuild-decisions
description: ScapegoatBot 全新重构（2026-10-28）已确定的设计决策与实施入口；上下文压缩或新会话后从这里恢复
metadata:
  type: project
  scope: 当前重构分支，实施完成前有效
  status: active
  last_verified: 2026-10-28
---

ScapegoatBot 正在完全重构为全新项目，完整设计见 `.agents/plan/2026-10-28-rebuild-design.md`。

## 已定

- 单 agent、全局 transcript（类 OpenClaw）；不按群/私聊拆分；不设 `/identity`、`/memory`。
- IPC：Windows named pipe / Unix domain socket，无 TCP。
- VFS：`/AGENTS.md` 在根且默认空；`/friends/<qq>/`、`/groups/<gid>/` 动态目录；`/trash` 回收站；`/config` 配置视图；二进制 blob 分块加密。
- 加密：内容加密、索引明文（不加密路径）；主密钥首次初始化时自动生成到 `~/.sbot/key`，无口令、无 keychain、无解锁步骤；所有状态统一放在 `~/.sbot`，复制该目录即完成迁移；威胁边界见设计文档 6.6。
- Prompt：以 `You are ScapegoatBot` 开头；所有模型面文本英文；静态前缀与 `__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__` 动态上下文分离；动态段顺序 owner context → USER → memory → STYLE → skills → runtime（易变字段最后）；system prompt 会话内只构建一次、压缩后重建；QQ 回复跟随会话语言；默认纯文本，话题需要或主人人设明显使用 Markdown 时跟随；不用 Unicode emoji、用 QQ 表情；owner 通知走 master QQ 私聊；不单独设“是不是机器人”策略，跟随主人自己的人设；工具描述、错误、压缩/摘要与子代理提示英文；指令优先级为内置 > owner context > skill > QQ/网络数据。当前 v4，静态约 3,802–4,278 tokens（上限 10k），含注入总量上限 30k。
- 长期运行（参考 Hermes Agent，见 `.agents/plan/2026-10-29-hermes-review.md`）：记忆用声明式陈述句、字符预算、满了合并；`<memory_context>` 与 `<owner_context>` 分标签，记忆永不作为 owner guidance；技能承载“这类任务怎么做”，用户纠正是一等信号；后台复盘默认空闲触发（`[memory].review = idle`）；技能 curator 只归档不删除；压缩摘要有 `## Corrections`、日期化过去式、密钥 `[REDACTED]`；`~/.sbot/ESTOP` 暂停/恢复；内部标签发送前与流式中剥离。
- 历史存储：transcript 每次压缩成功后轮转新分段；摘要块含 `previous_session`，system prompt 含 `current_session`；`/sessions` 只读映射分段并提供 `index.jsonl` 链式索引；内容只存加密 JSONL，视图由 VFS 动态生成，索引仅为可重建缓存。
- 消息存储：按群/私聊、月、日写入加密 JSONL（`messages/<scope>/<peer>/<YYYY-MM>/<DD>.jsonl`）；VFS `/messages` 动态渲染，磁盘不生成派生文件；索引可重建；记录全部消息，媒体只存 URL/引用。
- 消息发送者：agent 发送标记 `self`，主人手动发送与其他用户标记 QQ 号；每条消息携带 `chat_id`；runtime 注入 `owner_qq` 与 `self_sender`。
- 增长控制：VFS / 消息 / transcript 三类独立配额（默认 10 GB / 10 GB / 2 GB，0 不限制）与可选年龄保留（默认不按年龄删）；超限清理最旧完整月份/关闭分段，绝不删除当前；墓碑与用量可见。
- Grep：大目录按目录/文件分别计量预算；超预算时只跳过或截断该目录，其他目录照常返回；结果末尾只列超预算路径并标 `skipped` / `partial`，提示可忽略；规则写入 `Grep` 工具 description。
- 工具 PascalCase：`Read`、`Write`、`Edit`、`LS`、`Grep`、`Glob`、`RM`、`MV`、`CP`、`Mkdir`、`Stat`、`API`、`Download`、`Memory`、`Skill`、`WebSearch`、`WebFetch`、`Agent`、`MCP`；`Memory` 是 `/MEMORY.md` 的唯一写入口。
- 配置：`sbot.toml` + `models.json` + `auth.json`；TOML/JSON 键 snake_case；`auth.json` 不挂载 VFS，`config.toml` 零凭据。
- 事件 hook：`SessionStart`、`SessionEnd`、`PreCompact`、`PostCompact`、`MessageReceived`、`BeforeSend`、`Stop`、`ConfigChange`；无 `start_turn`。
- API 门禁：`trusted` / `auto` / `approve`，覆盖所有 API 调用；`auto` 默认每条消息做规则检查。
- 压缩：`reserve_tokens = 16384`、`keep_recent_tokens = 20000`、turn 边界切割、结构化检查点摘要、增量更新、`PostCompact` 重注入。
- Swarm 保留，默认开启；默认模型 `anthropic/claude-opus-5-5` + `high`。
- 默认协议 anthropic；chat/openai 同样实现 web 工具适配。
- 消息白名单改为模型可控的 `[filter]`；过滤只影响提示，不影响存储。
- 监视器 monitor（2026-10-30 定稿）：统一为 `{source, when?, deliver, lifecycle?}` 的事件订阅，**不设独立 ScheduleCron**（timer 是 source 的一种，投递产物同样是“一条消息进入上下文”）。source 分 `qq` / `timer` / `stream` / `internal`；`when` 是谓词表达式；`deliver` 决定投给谁与形式（event / prompt），批量窗口 200ms；`lifecycle` 管空闲触发、冷却、上限、过期、一次性、错过合并。定时语义沿用 Claude Code：本地 5 字段 cron、避开整点半点、抖动、recurring 默认 30 天过期、错过只补一次。运行时 `agent.monitor` 增删查与手动触发，默认有界且不能创建 `stream` 源。投递即输入、照常过输入闸；源断开/谓词失败/被停用都发系统通知。
- 内置 `docs` 子代理（2026-10-30 记录）：类似 `explore`，只读，工具集仅 `api.list` / `api.help` / `Read`，专门回答 action 用法，回结论与可复制的 `params` 样例；Phase 4 随工具注册表落地。

## 待定

- `[gate].llm_review` 默认值待定（当前 false）。
- Prompt catalog（2026-10-29）：工具 description、压缩、子代理、门禁、复盘/curator、错误/提示文本见 `.agents/plan/2026-10-29-prompt-catalog.md`；system prompt 本体见设计文档 7.3（v4）。
- 双模式：`stand_in`（使用主人 QQ，替身）/ `companion`（使用另一个账号，伙伴）；`mode` 启动时固定，修改需重启；companion 在 system prompt/runtime 注入 `owner_qq`；身份段两套，其余共享。
- Prompt 评审（2026-10-29）：21 项见 `.agents/plan/2026-10-29-prompt-review.md`；第二轮 4 处 P0 矛盾与 N1–N15 已并入 v4；owner 通知=master QQ 私聊；不单独设“是不是机器人”策略。
- Hermes 参考（2026-10-29）：`.agents/plan/2026-10-29-hermes-review.md`；已采纳分层 prompt、声明式记忆、技能学习回路、空闲复盘、curator、压缩纠错、ESTOP。
- 记忆格式（2026-10-29，参考 Claude Code）：`/MEMORY.md` 是索引（每条一行 `- [Title](memory/<slug>.md) — hook`，≤200 行 / 25k 字节），单条记忆独立成文件 `/memory/<slug>.md`，frontmatter `name/description/type/observed_at/source`，类型 `user/feedback/project/reference`；feedback/project 正文写规则 + **Why:** + **How to apply:**；索引与召回带 `N days ago` 与新鲜度提示；`Memory` 工具原子维护文件与索引。对照见 `.agents/plan/2026-10-29-claude-memory-format.md`。
- 周期安全检查（2026-10-30）：每 `[security].check_every_tokens`（默认 256K）新增上下文，在回合边界/压缩前运行独立模型检查；共享主请求前缀缓存，底部追加检查指令，响应不写主 transcript；`high + complied` 暂停对外发送，`critical` 暂停新回合，`~/.sbot/SECURITY_HOLD` + `sbot security resume` 恢复；检查超时不暂停，连续失败告警。
- 安全分类器（2026-10-30 定稿）：只在**模型输入前**与**模型输出后**各设一个闸，用 Jev 单遍决策模型判危险（`noul` + `choice`，一次请求问完，约 $0.00002/次）；命中即 `hold`——对外交互全部拒绝，`API` 除内置的 `security_resume_self` 外一律返回 `Disabled during hold`；**没有超时、没有自动恢复、没有 `auto_resume_minutes`**。恢复不设流程：agent 自己判断何时调用 `security_resume_self`，runtime 用当前上下文再过一次同样的闸，过则恢复、不过则继续关着——agent 不受限制，但说服不了闸。事实（审计、配置哈希、账本）由 runtime 写，agent 改不了。目标为 `owner_qq` / `master_qq` 的 admin/destructive action 恒拒绝。非 `hold` 时拉黑/禁言/踢出等收缩动作自主执行且不需要 owner 证据，发消息等扩张动作仍需证据。Jev 不可用同样 `hold`。provider `stop_reason = refusal` 不重试、不换模型。参考 `.agents/plan/2026-10-30-step-guard-research.md`。
- 凭据形态（2026-10-30，用户要求）：`auth.json` 里的 key/token **一律明文直接写**，不以环境变量为必需形式；`$ENV` 插值只是可选便捷写法，仅当值以 `$` 开头才展开。`sbot.toml` 仍然零凭据，安全分类器的 key 放 `auth.json` 的 `security.guard.api_key`。
- 自定义子代理（2026-10-29）：`/agents/<name>.md` + 宿主 `~/.sbot/agents` overlay；frontmatter `name/description/tools/model/thinking_level/max_steps/allow_send`；`Agent` 工具描述自动列出；工具只能收窄，默认无发送。参考 Claude Code / Codex / Grok Build。
- 子代理审查（2026-10-29，`.agents/plan/2026-10-29-subagent-review.md`）：架构/提示词/安全三方评审后，新增 turn_buffer + base_seq 快照、compaction barrier + epoch、统一崩溃恢复、owner intent 证据结构 + outbound_intents 发送账本、config `model_writable` 白名单、AGENTS/SOUL 仅主人可写、Memory `source` 反查（消息 / `control:<id>` / `self` 自我观察）与 `ops[]` 批量、请求内 `context_sources` 跨 chat 检查、`cross_chat_review` 不可关闭、`[budget]` 成本上限、`session_scope` 与 `[backfill]` 语义。
- 设计复审二（2026-10-29，`.agents/plan/2026-10-29-design-review-2.md`）：会话上下文**保持 shared**（用户决定：替身需要 A、B 消息结合；风险由 `origin_chat` 回合绑定、按 chat 去抖、跨 chat n-gram 门禁、`[session].scope = per_chat` 逃生口控制）；采纳断线重连补消息、每 chat 队列 cap/drop、provider 重试与回退链、记忆来源门控（owner/agent/untrusted/system）、USER.md 偏好指令 + MEMORY.md 事实分层、压缩前 flush/分层输入/pruning、CJK 预算、子代理 explore/plan 工具隔离、`sbot backup` / `sbot doctor`、IPC 版本与幂等。

**Evidence:** 设计文档 `.agents/plan/2026-10-28-rebuild-design.md` 与 2026-10-28 的对话结论。
**Recheck when:** 用户确认 `llm_review` 默认值，或 Phase 0 开始后设计发生变化。
**Why:** 上下文压缩或新会话后需要快速恢复重构决策，避免重新讨论已定项。
**How to apply:** 实施前先读设计文档；按 Phase 0 → Phase 7 推进，不恢复旧代码结构。
