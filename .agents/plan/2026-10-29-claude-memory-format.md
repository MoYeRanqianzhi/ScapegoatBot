# Claude Code 记忆格式参考与适配（2026-10-29）

- 参考实现：`.ref/claude-code/src/memdir/`（`memoryTypes.ts`、`memdir.ts`、`memoryScan.ts`、`memoryAge.ts`）、`src/services/extractMemories/prompts.ts`。
- 目的：把 Claude Code 的“索引 + 单条记忆文件 + 类型 + 新鲜度”格式引入 ScapegoatBot，替换当前“单文件条目 + `§` 分隔”的做法。
- 结论：采纳索引与单文件结构、四类记忆、`Why`/`How to apply`、显式保存门、年龄与新鲜度提示、抽取子代理并行读写；不采纳其工程仓库语境（CLAUDE.md、git、file:line），换成 QQ 聊天语境。

---

## 1. Claude Code 的格式（带证据）

### 1.1 `MEMORY.md` 是索引，不是记忆

`src/memdir/memdir.ts`：

- `ENTRYPOINT_NAME = "MEMORY.md"`；`MAX_ENTRYPOINT_LINES = 200`；`MAX_ENTRYPOINT_BYTES = 25_000`。
- `truncateEntrypointContent` 在超过行数/字节时截断，并追加警告：“index entries are too long; keep index entries to one line under ~200 chars; move detail into topic files.”
- 抽取 prompt（`src/services/extractMemories/prompts.ts`）明确：
  - 每条记忆写进自己的文件（如 `user_role.md`、`feedback_testing.md`）；
  - 然后在 `MEMORY.md` 加一行指针：`- [Title](file.md) — one-line hook`；
  - `MEMORY.md` 没有 frontmatter，永远不写记忆正文；
  - `MEMORY.md` 始终加载，超过 200 行会被截断。

### 1.2 单条记忆的 frontmatter 与正文

`MEMORY_FRONTMATTER_EXAMPLE`：

```markdown
---
name: {{memory name}}
description: {{one-line description — used to decide relevance in future conversations, so be specific}}
type: {{user, feedback, project, reference}}
---

{{memory content — for feedback/project types, structure as: rule/fact, then **Why:** and **How to apply:** lines}}
```

- `description` 是未来判断相关性的唯一依据，必须具体。
- `feedback` / `project` 正文：先写规则或事实，再写 **Why:**（原因/事件）与 **How to apply:**（何时适用）。
- `project` 类型要求把相对日期转成绝对日期（“Thursday” → “2026-03-05”），否则时间过去后无法解释。

### 1.3 四类记忆

`MEMORY_TYPES = ['user', 'feedback', 'project', 'reference']`：

| 类型 | 内容 | 何时保存 | 作用 |
|------|------|----------|------|
| `user` | 用户的角色、目标、职责、知识、偏好 | 了解到用户的角色/偏好/知识时 | 调整解释深度与协作方式 |
| `feedback` | 用户对工作方式的指导，包括“别这样做”和“这样对” | 被纠正，或非显然的方法被确认 | 不重复犯同样的错，也不偏离已验证的做法 |
| `project` | 正在进行的工作、目标、bug、事件，代码/git 看不出来的背景 | 知道谁在做什么、为什么、何时 | 理解请求背后的动机与协调 |
| `reference` | 外部系统的指针（Linear 项目、Grafana 面板等） | 了解到外部资源及其用途 | 知道去哪里找最新信息 |

`feedback` 特别强调：**记录失败也记录成功**；只记纠错会让人越来越保守，并偏离用户已经认可的做法。

### 1.4 不要保存什么

`WHAT_NOT_TO_SAVE_SECTION`：

- 代码模式、约定、架构、文件路径、项目结构（当前状态可推导）。
- Git 历史、最近改动、谁改了什么（`git log` / `git blame` 是权威）。
- 调试解法、修复配方（修复在代码里，上下文在 commit message 里）。
- 已经写在 CLAUDE.md 里的内容。
- 临时任务细节：进行中的工作、临时状态、当前对话上下文。

并且有一条“显式保存门”：**即使主人明确要求保存，也先问“其中什么让你意外/不显然”**；只保存那部分，不保存活动流水账。

### 1.5 召回：清单 + 年龄 + 新鲜度

`memoryScan.ts`：

- 扫描记忆目录下的 `.md`（排除 `MEMORY.md`），读 frontmatter，按 mtime 倒序，最多 200 个。
- 清单行格式：`- [type] filename (ISO timestamp): description`。

`memoryAge.ts`：

- `memoryAge` 返回 `today` / `yesterday` / `47 days ago`；模型不擅长日期算术，相对年龄更能触发“可能过期”的判断。
- 超过 1 天的记忆附带新鲜度警告：记忆是时间点观察，不是实时状态；涉及代码行为或 file:line 的结论在断言前要核实；“记忆说 X 存在”不等于“X 现在存在”。

`WHEN_TO_ACCESS_SECTION` / `TRUSTING_RECALL_SECTION`：

- 用户明确要求回忆时必须访问记忆；
- 用户说“忽略/不要用记忆”时，完全当作 `MEMORY.md` 为空，不引用、不比较、不提及；
- 记忆与当前观察冲突时，以当前观察为准，并更新/删除过期记忆。

### 1.6 抽取子代理

`src/services/extractMemories/prompts.ts`：

- 作为主对话的 fork 运行，使用同一 system prompt 与消息前缀。
- 只允许 `Read` / `Grep` / `Glob` / 只读 `Bash` / 记忆目录内的 `Edit` / `Write`；MCP、Agent、可写 Bash 一律拒绝。
- 有轮次预算；建议第 1 轮并行读所有可能更新的文件，第 2 轮并行写；不要交叉读写浪费回合。
- 先给出已有记忆清单，要求“先检查再写，更新而不是重复”。

---

## 2. 适配到 ScapegoatBot

### 2.1 目录

```text
/USER.md                     # 常驻：主人偏好与称呼（祈使句，≤4k）
/MEMORY.md                   # 索引：每条一行指针，≤200 行 / 25k 字节
/memory/<slug>.md            # 单条记忆：frontmatter + 正文
/memory/notes/<date>.md      # 每日观察（episodic），压缩前 flush / 复盘写入
/memory/topics/<topic>.md    # 可选的长主题材料，由多条记忆引用
```

### 2.2 单条记忆格式

```markdown
---
name: short-kebab-slug
description: One line, specific enough to decide relevance later.
type: user | feedback | project | reference
observed_at: 2026-10-29
source: group:123 / private:456 / control:<id> / self
---

Rule or fact.

**Why:** The reason the owner gave, or the incident behind it.

**How to apply:** When this kicks in, and where it does not.
```

- `source` 与 `origin`（owner / agent / untrusted / system）分开：前者是位置，后者是可信级别；来源由代码写入，正文不能伪造。`source: self` 表示 agent 自己的观察/反思（例如“子代理开太多会消耗余额”），不引用外部消息，`origin` 固定为 `agent`，不能写入 `/USER.md`。
- `feedback` / `project` 必须有 **Why** 与 **How to apply**；`user` 可以只有事实；`reference` 写“去哪里找什么”。
- 所有相对日期在写入时转成绝对日期。

### 2.3 索引格式

```markdown
- [Title](memory/feedback-reply-length.md) — the owner dislikes long replies in groups (47 days ago)
```

- 一行、≤200 字符、只放标题、链接、一句钩子与年龄。
- 索引文件超过 200 行或 25k 字节时截断并注入警告；提示把细节移进单条记忆。
- `/MEMORY.md` 由 `Memory` 工具维护；`Write` / `Edit` 不允许直接写，避免索引与文件不一致。

### 2.4 注入

- `<user_context>`：`/USER.md` 全文（≤4k 字符），始终注入。
- `<memory_context>`：`/MEMORY.md` 索引 + 年龄徽标；索引超限截断并警告。
- 相关记忆：压缩后、触发词命中或主人明确问“你记得吗”时，按描述/关键词检索最多 3 条单条记忆，注入 `<memory_recall>` 全文并附新鲜度提示；未命中不注入。
- 新鲜度：每条召回附 `This memory is N days old; verify against current chats or history before asserting it.`；`N ≤ 1` 省略。

### 2.5 `Memory` 工具

- `action: add | replace | remove`，`type` 四选一，`name` 为 slug，`description` 一句话，`content` 正文；`add` 同时写 `/memory/<slug>.md` 与 `/MEMORY.md` 一行；`replace` 更新正文与索引描述；`remove` 删文件并删索引行。
- 预算：索引 ≤ 200 行 / 25k 字节；单条记忆 ≤ 4k 字符；`/USER.md` ≤ 4k 字符；超限拒绝并提示合并。
- 来源：`origin` 由写入代码判定（主人陈述 / 从主人内容提炼 / 他人内容 / 系统）；`untrusted` 可写但不自动注入。
- 显式保存门：主人明确说“记住”时立即保存最合适的类型；主人说“忘了”时找到并删除；主人要求保存流水账时，先问“哪一点让你意外/不显然”，只保存那一点。
- 自我观察：agent 可以记录没有外部来源的经验（预算、工具、流程），标 `source: self`；这类记忆是 agent 的笔记，不能当主人偏好或 owner intent。

### 2.6 后台复盘

- 复盘 prompt 先给现有记忆清单（`[type] slug (age): description`），要求“先查再写，更新而不是重复”。
- 工具限制为只读工具 + 记忆目录内的 `Memory`；不允许发送、不允许改配置、不允许写 owner context。
- 第 1 轮并行读可能相关的记忆，第 2 轮并行写；不要在多轮之间来回读。
- 复盘只处理 `owner` / `agent` 候选；`untrusted` 只记录、不注入。

### 2.7 与当前设计的差异

| 当前 | 改为 | 理由 |
|------|------|------|
| `/MEMORY.md` 单文件、`§` 分条 | 索引 + 单条 `/memory/<slug>.md` | 单文件会膨胀；索引可预算，细节按需读取 |
| 记忆条目无类型 | 四类 `user/feedback/project/reference` | 类型决定保存时机、正文结构与召回价值 |
| 只有陈述句规则 | `feedback`/`project` 用“规则 + Why + How to apply” | 知道为什么，才能在边界情况下判断 |
| 无年龄 | 索引与召回带 `N days ago` + 新鲜度提示 | 模型不擅长日期算术；旧记忆容易被当成现状 |
| 复盘直接写条目 | 复盘先读清单，再写单文件 + 索引 | 避免重复文件与索引漂移 |
| `Memory` 写单文件 | `Memory` 同时维护文件与索引 | 索引与内容必须一致 |

### 2.8 不采纳

- `CLAUDE.md` 层级与 `@import`：我们的 owner context 已由 hook 处理，不需要文件导入语法。
- 代码/git 语境（file:line、git log）：QQ 场景改为“先查 `/messages`、`/sessions` 或当前聊天，再断言”。
- `Bash` 只读工具：我们没有 shell；用 VFS 的 `Read` / `Grep` / `Glob` / `LS` 代替。
- team/private 双目录：我们的 owner context 与 agent 记忆已经分离；不引入团队共享层。
