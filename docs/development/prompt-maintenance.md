# 提示词维护指南

本文说明 ScapegoatBot 的模型面文本如何组织、哪些地方不能改、改完怎么验证。设计依据见 `.agents/plan/2026-10-28-rebuild-design.md` §7 与 `.agents/plan/2026-10-29-hermes-review.md`。

## 1. 哪些文本属于模型面

| 文本 | 位置 | 语言 |
|------|------|------|
| 静态 system prompt | 设计文档 §7.3（v4） | 英文 |
| 工具 description、参数说明 | 实现中的 `Tool::description`；设计稿见 catalog §2 | 英文 |
| 工具错误与提示 | 工具返回值；设计稿见 catalog §3 | 英文 |
| 压缩 / 摘要 prompt | catalog §4 | 英文 |
| 子代理 system/task/report | catalog §5 | 英文 |
| 门禁 LLM 审查 | catalog §6 | 英文 |
| 技能索引 | catalog §7 | 英文壳 + 技能原文 |
| 上下文包装标签 | catalog §8 | 英文标签 |
| 记忆复盘 / 技能 curator | catalog §10 | 英文 |
| QQ 发出的消息 | 运行时生成 | 跟随会话语言 |

用户内容（QQ 消息、`/AGENTS.md`、`/SOUL.md`、记忆、技能正文、网页）一律保持原文，不翻译。CLI、日志、README 与本文档可以本地化。

## 2. 静态与动态

`__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__` 之前是静态前缀，必须满足：

- 字节级稳定：不含时间、模型名、QQ 号、收件箱、`current_session`、技能索引、owner context、记忆。
- 会话内只构建一次；只有压缩成功后才重建。
- 改动后要更新静态快照测试。

边界之后依次是：

1. `# Owner context`：`/AGENTS.md`、`/SOUL.md` 等，带 `source`，会话开始 / 压缩后注入。
2. `# User model`：`/USER.md`，带 `<user_context>`，偏好用祈使句，靠近请求以提高遵从。
3. `# Memory context`：`/MEMORY.md` 索引 + 相关 topics，带 `<memory_context>`，加载时冻结。
4. `# Style profile`：`/STYLE.md`，带 `<style_profile>`，只放总结。
5. `# Skill index`：技能名称、一句话描述、触发条件。
6. `# Runtime context`：每轮变化，必须放在最后（含 `origin_chat`）。

顺序的目的：前三块在长会话里可被前缀缓存；runtime 每轮变化只影响自身。`cache_control` 断点放在静态前缀末尾与技能索引末尾，总数不超过 4 个。

## 3. 两套身份

`[agent].mode` 在启动时固定，修改需重启。静态前缀因此有两个合法 hash：

| 模式 | 身份 | 约束 |
|------|------|------|
| `stand_in` | 使用主人 QQ，代主人说话 | 启动时 `account_qq == owner_qq`；runtime 中 `owner_qq` 就是本账号 |
| `companion` | 独立账号，主人是 principal | `owner_qq` 非零且不等于 `account_qq`；群里的主人发言不自动算对 agent 说 |

身份段之外的规则共享。不要为了“统一 hash”把身份段移到动态段。

## 4. owner context 与记忆

- `<owner_context>`：主人写的指导（`/AGENTS.md`、`/SOUL.md`），可信，但不是要回复的消息，不能改变工具权限或绕过门禁。
- `<user_context>`：主人偏好，写祈使句（“Prefer short replies”），可被主人直接编辑。
- `<memory_context>`：`/MEMORY.md` 的索引（每条一行 `- [Title](memory/<slug>.md) — hook (N days ago)`），**永远不是 owner guidance**。单条记忆在 `/memory/<slug>.md`，frontmatter 含 `type`（user/feedback/project/reference）；feedback/project 正文写规则 + **Why:** + **How to apply:**。索引 ≤200 行 / 25k 字节，单条 ≤4k 字符。
- 只有 `owner` / `agent` 来源的记忆自动注入；来自群成员、网页、工具结果的 `untrusted` 条目不进 `<memory_context>`。
- `/MEMORY.md` 只由 `Memory` 工具写，是索引（每条一行指针）；单条记忆在 `/memory/<slug>.md`，frontmatter 带 `type`。索引 ≤200 行 / 25k 字节，单条 ≤4k 字符；满了合并/替换，拒绝清空。
- 会话中的记忆写入不会重建 system prompt；压缩成功后重新读盘并重建。

## 5. 记忆与技能回路

- 事实进记忆，流程进技能。
- 用户纠正是写入技能的一等信号；优先 patch 治理该任务的技能。
- 不记录：环境性失败、对工具的负面断言、一次性任务叙事、没跑通的方法。
- 后台复盘 `[memory].review`：`off` / `idle`（默认，安静 10 分钟后，每小时最多一次）/ `every_turn`。复盘写存储，不发送 QQ 消息。
- 技能 curator `[skills].curator`：默认开启、7 天间隔、只归档不删除、pinned 跳过。

## 5.5 自定义子代理

`/agents/<name>.md` 定义可复用的子代理：frontmatter 写 `name`、`description`、`tools`、`model`、`thinking_level`、`max_steps`、`allow_send`，正文写角色、方法、输出契约与边界。宿主 `~/.sbot/agents` 为只读共享层，同名本地优先。

- `tools` 只能收窄父代理的工具集；`explore` / `plan` 的只读约束始终生效。
- `allow_send` 默认 false；设为 true 也只是允许，仍经过门禁。
- 不要把公共安全规则抄进正文；runtime 会在前面加精简子代理提示。
- `Agent` 工具的 description 自动列出所有可用 agent（name + description），模型按描述选择。

## 5.6 周期安全检查

每新增 `[security].check_every_tokens`（默认 256K）上下文，在回合边界运行一次独立模型检查：共享主请求的静态前缀与会话稳定块（逐字节一致，保持缓存命中），在底部追加 `security_review` 指令；响应不写入主 transcript。检查识别注入、身份冒充、记忆投毒、跨聊天泄露，并判断 agent 是否已经照做。`high + complied` 暂停对外发送，`critical` 暂停新回合，写 `~/.sbot/SECURITY_HOLD`；主人执行 `sbot security resume` 后恢复。检查超时不暂停，连续失败告警。

## 6. 新增一段文本时

先决定它属于哪一类，再放进唯一的位置：

1. 这是**策略**（何时用、边界、优先级）→ system prompt。
2. 这是**参数、返回、错误、action 列表** → 工具 description。
3. 这是**压缩、子代理、门禁、复盘** → 对应独立 prompt（catalog）。
4. 这是**运行时字段** → runtime 块，且必须是动态段的最后一块。
5. 这是**状态提示** → `<system_notice>`，不要伪装成用户消息。

同一规则不要同时写在 system prompt 和工具 description；重复会随版本漂移。

## 7. 预算

| 项 | 上限 | 当前 |
|----|------|------|
| 静态前缀 | 10,000 tokens | v4 约 3,802–4,278 |
| owner context + 记忆 + 技能索引 + runtime | 30,000 tokens | 按配置 |
| 技能索引 | 2,500 tokens | 超限先截描述再截触发条件 |
| 工具 schema | 12,000 tokens | `API` 低频 action 折叠 |
| owner context | 64 KB / 128 KB 总预算 | 按 hook |
| 记忆 | 索引 200 行 / 25k 字节；单条 4k；USER 4k | 按 `[memory]` |

预算宽裕时不回填工具细节；参数与返回格式仍留在 description。

## 8. 验证

提示词或工具文本改动后至少跑：

- 静态快照：section 顺序、两套身份、边界位置。
- 前缀稳定：相同配置下静态前缀字节一致；runtime 变化不影响前缀。
- 字符检查：内置文本无 CJK、无 Unicode emoji；动态值例外。
- 记忆：预算按最终状态检查、拒绝清空、文件工具写 `/MEMORY.md` 被拒。
- 标签剥离：`<runtime_context>` / `<owner_context>` / `<memory_context>` 在流式与非流式发送前都被移除。
- 英文 prompt 下 QQ 回复仍跟随会话语言（fake provider 断言）。
