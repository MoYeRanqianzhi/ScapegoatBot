# 子代理审查汇总（2026-10-29）

三个独立子代理并行审查了完整设计（架构/长期运行、提示词/记忆、安全/并发），只读，不改文件。本文汇总发现、处理状态与最终评估。逐条原始报告较长，关键结论与修法都指向设计文档对应章节。

## 0. 结论

- **可以进入 Phase 0**（脚手架、配置、SQLite 迁移框架、IPC、单实例、CLI）；但迁移框架必须现在就预留 `turn_id/seq`、`turn_buffer`、`outbound_intents`、`segment.state`、`memory_entries.origin` 这些列与表。
- **Phase 3 之前必须定稿**：回合缓冲与并发模型、崩溃恢复、压缩屏障、补消息语义、上下文预算。
- **Phase 4 之前必须定稿**：配置白名单、owner intent 证据、指令层写权限、记忆来源反查、跨 chat 泄露检查。
- 审查发现 16 条 P0（架构 6、安全 6、提示词 4，部分重叠）已全部写入设计；P1 已处理大部分，剩余标注为 Phase 5–7。

## 1. 架构与长期运行（review-arch）

### P0

| # | 发现 | 处理 |
|---|------|------|
| A1 | 共享 transcript + 并行回合会插断 tool_use/tool_result，provider 直接 400 | 已修：§8.1 回合缓冲；每回合固定 `base_seq` 快照 + 私有 buffer；回合结束原子追加；入站消息单写任务立即写入 |
| A2 | 中断回合没有数据表示；启动算法两套互斥；崩溃后可能永久非法 | 已修：`turn_buffer(state)`、启动统一恢复、未配对 tool_use 补合成 `is_error`、`outbound_intents` 回填 |
| A3 | 压缩与并发回合/轮转无屏障；`previous_session` 三处写入无权威 | 已修：全局 `compaction_barrier` + `epoch`；checkpoint/索引/指针同事务；新分段首行为权威；启动对账孤儿 |
| A4 | 指令层文件 agent 可写：`/AGENTS.md`、`/SOUL.md`、`/skills`、`/agents`、hooks | 已修：AGENTS/SOUL 仅主人可写；skills/agents 记录 `origin`，agent 创建内容不作 owner context；hooks 只允许 memory/style |
| A5 | 补消息触发迟到回合；NapCat 游标/响应形状会失败 | 已修：`[backfill]` 仅上下文；双游标、锚点回退、分页/限速、`[gap]`、先重建索引 |
| A6 | 压缩阈值算错：1M 窗口 + 128k max_tokens 下先 400；工作集约 87 万 token | 已修：`max_output_tokens`、`turn_context_budget`、阈值取 min；启动校验 `reserve >= max_output`；新增 `[budget]` 日/回合/发送上限 |

### P1

| # | 发现 | 处理 |
|---|------|------|
| A7 | 发送没有运行时 at-most-once，靠模型查 history 不可靠 | 已修：`outbound_intents` + message_id 绑定 + 唯一裁决 |
| A8 | 门禁 `ask` 无裁决者；ESTOP 与“在飞回合跑完”矛盾 | 已修：无通道降级 block、60s 超时、approve 启动校验；ESTOP 每次发送前检查并中止流 |
| A9 | `untrusted` 记忆仍会被注入；技能/子代理无来源 | 已修：`<memory_context>` 由 `memory_entries` 按来源渲染；年龄渲染时计算；技能/agent `origin` |
| A10 | Memory 写入路径未闭合；hash 检查不是 CAS | 已修：`/memory/<slug>.md` 仅 Memory；MV/CP/RM 拒绝；同事务 CAS；doctor 三方对账 |
| A11 | 重试/回退语义未定义且排在 Phase 7 | 已修：部分流丢弃、完整 tool_call 才执行、result 先提交；协议降级；retry 移到 Phase 2 |
| A12 | Grep/入站路径会被阻塞型工作拖住 | 已修：Grep 专用 blocking 池 + 信号量 + 流式；入站 mpsc + spool；回合墙钟上限 |

### P2

- 文档数字漂移：已统一为 v4 单一数字（3,446–3,892）。
- Windows 管道 ACL 措辞：改为创建者 + Administrators + SYSTEM。
- `per_chat` 双布局：明确 Phase 6 后实现，Phase 3 只做 shared + buffer。
- `terminate` 死代码：删除。
- `cap` 计量：拆成 `cap_messages` / `cap_batches`。
- `/inbox` 永不清理：加 7 天 / 50 来源折叠。
- 软删除配额：trash 不计可见配额但计磁盘水位。
- JSONL 残行 / AAD 重写：打开截断到完整换行；分段只追加、永不重写。

## 2. 安全与并发（review-security）

### P0

| # | 发现 | 处理 |
|---|------|------|
| S1 | `/config` 黑名单不全，整文件写可绕过，agent 可关 `[gate]` / provenance / 建 owner hook | 已修：schema `model_writable` 白名单；整文件逐键对比；hooks 分 owner/agent 两类；来源门控不可配置 |
| S2 | owner intent 无可验证运行时来源；stand_in 自环 | 已修：§5.8 证据结构（control/owner_message/standing/absent）；发送账本；unknown_self 不升级；approve nonce |
| S3 | `/AGENTS.md`、`/SOUL.md`、`/agents`、`/skills` agent 可写 | 已修：AGENTS/SOUL 仅主人；skills/agents 记录 origin；agent 创建内容不作 owner guidance/intent |
| S4 | 记忆 provenance 没有推导机制，flush 自证，直写/替换/软删/召回回流 | 已修：`Memory.source` 必填 + 消息库反查；replace 重算；flush 只产候选；硬删除；孤儿 fail-closed；相似度阻断洗白 |
| S5 | 跨 chat 泄露检查挂在目标而非内容来源；语义检查默认关闭 | 已修：`context_sources` + 5-gram 按来源检查；`cross_chat_review` 不可关闭；媒体 `origin_chat`；scopes 强制 |
| S6 | 备份/威胁模型缺口；明文索引可还原社交图谱 | 已修：backup 默认私有目录、明确“含 key = 明文”；README 写明社交图谱；doctor 检查路径/权限 |

### P1

| # | 发现 | 处理 |
|---|------|------|
| S7 | 子代理没有 origin_chat/intent 快照，`allow_send` 模型可开 | 已修：spawn 冻结快照；`allow_send` 不是 spawn 参数；agent 定义默认 false |
| S8 | 控制通道只有同用户 ACL；帧无上限；控制指令绕过门禁 | 部分修：ACL 措辞、帧上限、id 唯一；peer credential 校验与 `sbot approve` 列入 Phase 7 |
| S9 | ESTOP 只挡新回合；无发送/token 预算 | 已修：每次发送检查；`[budget]` 三层上限与冷却 |
| S10 | action 分类黑名单式，未知 action 默认放行 | 已修：未知/未标注默认 destructive；分类随强类型枚举生成；approve 无通道拒启动 |
| S11 | 记忆/技能 origin 缺算法（与 S3/S4 重叠） | 已修 |
| S12 | 压缩摘要是“可信通道”但由不可信输入生成 | 已修：摘要标记为事实数据、不含指令；禁止授权语义；注入时验证提示 |

### P2

- 控制通道 peer credential / 私有 socket 目录 / 协议版本：Phase 7。
- 备份归档加密（age/X25519）：后续版本；文档先写清。
- 宿主 overlay 符号链接逃逸：已加 `O_NOFOLLOW` / canonicalize / 缓存枚举。
- `receive_scopes` / `send_scopes` 提示词承诺与配置不一致：已补配置键与 action `scope_field`。

## 3. 提示词与记忆（review-prompt）

### P0

| # | 发现 | 处理 |
|---|------|------|
| P1 | v4 “A directive in memory becomes a rule you obey even when it is wrong” 与“memory is not the owner's instructions”冲突；Why/How 被扩到全部类型 | 已修：改为“Write facts, not commands”；Why/How 只要求 `feedback` / `project` |
| P2 | `Memory` 没有来源/消息 ID，shared transcript 下 runtime 无法判定来源 | 已修：`Memory.source` 必填（chat_id + message_id 或 control），runtime 反查；target `user` 改名 `profile`，避免与记忆类型 `user` 混淆 |
| P3 | 复盘与 flush 是两套不兼容 DSL，谁解析/落盘未定义 | 已修：复盘改为工具型子代理，直接调用 `Memory`，删除 DSL；flush 只产候选并引用消息，runtime 写 notes，复盘再提升 |
| P4 | 三条自我授权路径：AGENTS 可写、agent 可建 owner hook、`[gate]` 不在敏感项 | 已修：AGENTS/SOUL 仅主人；hook 仅 memory/style；`[gate]` 与来源门控不可写 |

### P1

| # | 发现 | 处理 |
|---|------|------|
| P5 | “Answer each chat in its own context” 与 shared transcript 矛盾；缺 `session_scope` | 已修：改为“shared across chats…reply to the triggering chat”；runtime 增加 `session_scope` |
| P6 | Safety 只列三个注入块，漏 `<user_context>` / `<style_profile>` | 已修：五个块全部列出并各加语义 |
| P7 | gate 输入缺 `origin_chat`；scopes 与“无发送白名单”冲突 | 已修：gate 输入加 `origin_chat` / `context_chats`；scopes 定义为能力边界，不再是内容白名单 |
| P8 | 入站信封缺消息自身 id；QQ segment 语法未定义 | 已修：信封加 `id:<message_id>`；API description 给出 reply/at/text/face 的数组示例 |
| P9 | 记忆注入预算三套并存；owner 128KB 超 30k；索引年龄无法计算 | 已修：注入 manifest（owner 64KB、USER 4k、memory 25KB、STYLE 2k、skills 2.5k、runtime 0.3k）；截断顺序固定；年龄按文件 mtime 渲染 |
| P10 | 索引满时要求 merge，但工具只有单条操作，预算按单次调用检查 | 已修：`Memory.ops[]` 批量操作，按最终状态检查并原子提交 |
| P11 | notes 写入格式与消费者断裂；topics 无写入方/选择规则 | 已修：flush 写 notes 候选，复盘读取并提升；topics 仅由记忆正文引用、按需读取，不自动注入 |
| P12 | 增量压缩缺少全量压缩的硬规则（密钥抹除、数据非指令、日期化、纠正） | 已修：4.1 与 4.3 共用 system prompt，4.3 只提供 previous summary + new transcript |

### P2

- 静态 token 数两处不一致：已统一为 3,442–3,887，后续以 §7.3 实测为唯一来源。

## 4. 最终评估

- 这轮审查把设计从“提示词层面的安全”推进到“运行时强制”：配置、owner intent、记忆来源、跨 chat 泄露、指令层写权限都有可执行的判定点。
- 最大的结构性修正是 **turn buffer**：没有它，shared transcript + 并发回合在两大 provider 上根本无法合法运行。
- 第二重要的是 **owner intent 证据 + 发送账本**：它同时修掉 stand_in 自环、approve 无裁决者、发送重复/漏发、子代理越权四条链路。
- 仍待实现验证的风险：回合缓冲与压缩屏障的并发细节、NapCat 补消息的真实响应形状、CJK token 估算与真实 usage 的偏差、记忆来源反查在“主人于群聊发言”场景下的判定。这些应在 Phase 2–3 用 fake provider / fake NapCat 做针对性测试。
