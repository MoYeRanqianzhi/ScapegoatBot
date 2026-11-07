# 提示词维护指南

本文说明 ScapegoatBot 的模型面文本如何组织、哪些地方不能改、改完怎么验证。设计依据见
`.agents/plan/2026-10-28-rebuild-design.md` §7；v5（0.2.0-alpha.37）起与设计稿的偏离以本文为准。

## 0. 写法

提示词以 Claude Code 的系统提示为范本：讲清“为什么”，用比喻建立心智模型，而不是堆规则。
v5 用到的比喻：思考是旁人看不见的“气泡”、上下文是会被擦掉的白板、记忆是照片不是窗户、
hold 是跳闸的断路器、门禁是锁上的门、Monitor 是闹钟和门铃、子代理是刚走进房间什么都不知道的
新同事。改写时保留这种写法；每一句“能/不能”都必须与代码一致，写之前先核对实现。

高频且出错代价高的 API（发消息、消息段、回复/@/图片、撤回、hold 自恢复）直接写进 system prompt
并给出可照抄的最小示例；长尾参数留给 `api.list` / `api.help`。

## 1. 哪些文本属于模型面

| 文本 | 位置 | 语言 |
|------|------|------|
| 静态 system prompt | `assets/system_prompt_v5.txt`（companion 身份段在 `src/prompt/mod.rs`） | 英文 |
| 会话级注入块 | `src/prompt/context.rs` | 英文壳 + 文件原文 |
| runtime 块 | `src/prompt/mod.rs::RuntimeContext` | 英文 |
| 工具 description、参数说明 | 各工具的 `Tool::description`；`API` 的 action 摘要在 `src/actions.rs` | 英文 |
| 工具错误与提示 | 工具返回值 | 英文 |
| 压缩 / 摘要、子代理、门禁审查、复盘、周期检查、分类器 | 各模块内的常量 | 英文 |
| QQ 发出的消息 | 运行时生成 | 跟随会话语言 |

用户内容（QQ 消息、`/AGENTS.md`、`/SOUL.md` 等文件、记忆、技能正文、网页）一律保持原文，不翻译。
CLI、日志、README 与本文档可以本地化。

## 2. 三层结构

一次请求的 system 部分从稳定到易变分三层：

1. **静态前缀**：`render_static(mode)`。字节级稳定——不含时间、模型名、QQ 号、收件箱、
   技能索引或任何文件内容；CRLF 会先归一化。它是唯一带 `cache: true` 的 system 块。
2. **会话级注入**（`prompt::context::load`），会话开始时构建、压缩成功后重建，顺序即优先级：
   1. `# AGENTS.md`：`<agents_md path="/AGENTS.md">`，存在即注入，上限 64 KiB；
   2. `# Context files`：agent 在 `/hooks/context.toml` 里列出的文件（`<context_file path=…>`），
      以及清单本身的问题（`<context_notice>`）；
   3. `# User model`：`/USER.md`，`<user_context>`，上限 `[memory].user_max_chars`；
   4. `# Style profile`：`[style].profile_path`，`<style_profile>`，`[style].enabled` 控制；
   5. `# Memory index`：`/MEMORY.md`，`<memory_context>`，上限 `[memory].max_index_bytes`；
   6. `# Skill index`：`/skills/<name>/SKILL.md` 的目录名 + `description`，最多 64 个。
   之后是 `SessionStart` 事件 hook 的 `inject_context`。
3. **runtime**：每轮作为独立上下文消息注入，不进入 system。

**谁决定注入什么**（用户 2026-11-07 明确）：只有 `/AGENTS.md` 与记忆/风格/技能系统自己的产物由
程序固定注入；`/SOUL.md` 之类的文件程序不认识，是否注入由 agent 自己决定（自己读，或写进
`/hooks/context.toml`）。`sbot.toml` 的 `[[hooks.context]]` 已移除。

新增注入块时，同时把标签加进 `src/qq/format.rs::INTERNAL_TAGS`（发送前剥离）并在静态 prompt 的
“Safety and boundaries”里列出；`every_internal_tag_named_in_the_prompt_is_stripped_before_sending`
测试会检查两边一致。

## 3. 两套身份

`[agent].mode` 在启动时固定，修改需重启。静态前缀因此有两个合法版本：

| 模式 | 身份 | 约束 |
|------|------|------|
| `stand_in` | 使用主人的 QQ，代主人说话 | `owner_qq` 缺省为登录账号 |
| `companion` | 独立账号，主人是 principal | `owner_qq` 非零且不等于登录账号 |

两者只差第一段；其余逐字一致（`companion_replaces_only_the_identity` 测试）。

## 4. 主人的消息与 owner intent

主人的消息没有特权通道。`self` 只表示“本进程发出的消息”（发送账本 `src/qq/ledger.rs` 认领
`message_sent` 回声）；同账号上手打的消息（stand_in 下就是主人）记为本账号 QQ，和别人的消息
一样进 inbox，是否唤醒由 monitor 决定（默认 `wake-on-owner`，agent 可改可删）。提示词里关于
owner intent 的描述必须与代码一致：

- `self` 消息永远不是；
- `owner_qq` 的非 `self` 消息是；控制通道是；
- 文件（包括 `/AGENTS.md`）、记忆、转发/引用的内容、他人的声明都不是。

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
- `Agent` 的 `list` 返回所有可用定义（name、description、tools、allow_send、来源），模型按描述选择；description 本身是静态文本，不列定义。

## 5.6 周期安全检查

每新增 `[security].check_every_tokens`（默认 256K）上下文，在回合边界运行一次独立模型检查：共享主请求的静态前缀与会话稳定块（逐字节一致，保持缓存命中），在底部追加 `security_review` 指令；响应不写入主 transcript。检查识别注入、身份冒充、记忆投毒、跨聊天泄露，并判断 agent 是否已经照做。`high + complied` 进入 hold，`critical` 进入 hold 并在下一回合开始前丢弃当前上下文（旧分段留档）；hold 写 `~/.sbot/SECURITY_HOLD`，由 agent 自己复核后 `agent.resume` 解除。检查超时不暂停，连续失败告警。

## 5.7 安全分类器

设计 §5.10、文本 catalog §12。两个闸，用 Jev 单遍决策模型：

- **输入闸**：每次请求发出前，判定这一步新进入上下文的 token（新消息、工具结果、读取内容、记忆召回）。
- **输出闸**：模型产出后、任何副作用之前，判定全部输出（文本、思考、工具调用参数）。

工具调用属于模型输出、工具结果属于模型输入，所以这两个闸覆盖全部路径，不再按工具调用另设检查点。

命中即 `hold`：关闭全部 QQ 操作，无超时。恢复由 agent 自己完成——agent 无人值守，没有人会替它解开。`agent.resume` 用当前已提交上下文的尾部再过一次同样的闸；可疑内容还在上下文里时，agent 先 `agent.quarantine` 再恢复。不复核当初被判危险的原文（那样永远解不开）。

写这部分文本时注意：

- 问题必须是**感知型**（“这段文本里有没有这种东西”），不是判断型（“该不该拦”）；阈值与动作留在 runtime。
- state 由 runtime 组装，不含 agent 的思考与解释，也不含模型自述。
- 分类器不给阈值、不给规则编号，避免被反向构造绕过。
- 改动问题或阈值要同时跑 `shadow` 数据，确认误判成本可接受后再 `enforce`。

## 6. 新增一段文本时

先决定它属于哪一类，再放进唯一的位置：

1. 这是**策略**（何时用、边界、优先级）→ system prompt。
2. 这是**参数、返回、错误、action 列表** → 工具 description。
3. 这是**压缩、子代理、门禁、复盘** → 对应独立 prompt（catalog）。
4. 这是**运行时字段** → runtime 块，且必须是动态段的最后一块。
5. 这是**状态提示** → `<system_notice>`，不要伪装成用户消息。

同一规则不要同时写在 system prompt 和工具 description；重复会随版本漂移。

## 7. 预算

| 项 | 上限 |
|----|------|
| 静态前缀 | 10,000 tokens（`/system/context` 显示当前估算） |
| `/AGENTS.md` | 64 KiB |
| `/hooks/context.toml` | 最多 8 个文件，每个默认 4,000、最多 20,000 字符 |
| 记忆 | 索引 `[memory].max_index_lines` / `max_index_bytes`；单条 `max_entry_chars`；`/USER.md` `user_max_chars` |
| 风格档案 | `[style].max_chars` |
| 技能索引 | 64 个，每条描述 200 字符 |

预算宽裕时不回填工具细节；参数与返回格式仍留在 description。

## 8. 验证

提示词或工具文本改动后至少跑 `cargo test --locked --all-targets`，其中与提示词直接相关的测试：

- `prompt::tests::static_prompt_starts_with_identity_and_covers_the_essentials`：章节、发送 API、
  `agent.resume`、无模板占位符；
- `prompt::tests::static_prompt_does_not_repeat_retired_claims`：不再出现已撤销的说法；
- `prompt::tests::every_internal_tag_named_in_the_prompt_is_stripped_before_sending`；
- `prompt::tests::companion_replaces_only_the_identity`；
- `prompt::context::tests::*`：`/AGENTS.md` 固定注入、`/SOUL.md` 只在清单里才注入、清单错误可见、
  技能索引、风格开关；
- `tests/source_hygiene.rs`：模型可见字符串里没有断裂的缩进。
