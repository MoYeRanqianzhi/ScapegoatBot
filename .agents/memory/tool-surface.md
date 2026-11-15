---
name: tool-surface
description: agent 的工具表由用户决定：没有 Memory / Download 工具；API 的前缀只是命名空间（local 要改名 native）、要可扩展到任意渠道；新增工具、改 action 命名或策略前先读
metadata:
  type: feedback
  scope: src/tools 的工具表、子代理工具表、system prompt 里的工具说明
  status: active
  last_verified: 2026-11-16
---

用户 2026-11-10 明确：**从未同意过 `Memory` 与 `Download` 这两个工具**，它们是 agent 私自
加进设计文档与代码的。

- **记忆文件由文件工具直接读写**：`/MEMORY.md`、`/USER.md`、`/memory/**` 用 `Read` /
  `Write` / `Edit`，没有专门的记忆工具。
- **下载是非必要功能，放进 `API`**（`local.*` 下的 action），不占一个顶层工具。
- 推论：顶层工具表只放 agent 时刻都要用的基本能力；其余功能做成 `API` 的 action（命名
  要体现完整归属，见 [[design-decisions]] 的“工具命名分层”）。**新增顶层工具必须先得到用户
  同意**，设计文档里写了不等于用户同意。

用户 2026-11-13 对 `API` 的 action 命名与扩展方向的要求（**尚未实现**，见 `.agents/TODO.md`）：

- 本机动作的前缀要从 `local.*` 改成 **`native.*`**（用户更喜欢这个词）。
- **前缀只是命名空间，方便区分，不承载任何强制约定**（比如“`local.*` 只能碰 VFS”这种说法
  是 agent 自己的误读，用户纠正过）。所以策略不能挂在前缀上：现在按前缀判断的地方
  （`src/tools/api/policy.rs` 的 `strip_prefix("napcat.")`、`src/actions.rs` 的
  `starts_with("napcat.get_")` / `"napcat.upload_"`、子代理只给 `local.*` / `api.*`、hold 只拦
  `napcat.*`）要先改成每个 action 自带的属性（是否对外发送、是否只读、是否需要 QQ 连接……）。
- `API` 要**可扩展**：用户能自己增加工具（action）；能接**任意即时通讯渠道**并映射成 `API`
  的 action；**NapCat 可以禁用**。目标是更高的自由度，而不是写死 QQ。

顺序（agent 的计划，用户没有逐条确认）：先把策略从前缀挪到属性上 → 写渠道与自定义 action 的
设计稿给用户确认 → 最后统一改名（代码、提示词、测试、文档、CHANGELOG 的破坏性变更）。改名
本身是破坏性变更，不要在设计确认之前零散地改。

**Evidence:** 用户 2026-11-10 原话：“我从来没有同意有工具Memory和Download，这都是私自
加入的，记忆文件直接文件工具读写，下载这种非必要功能明显要放在API工具中”。用户 2026-11-13：
“我感觉这个本地工具，当前叫local，是local.xxx.xxx，但是我更倾向用native这个单词”；“之前对
命名空间理解有误，这只是命名空间方便区分，并没有什么只能碰VFS什么的强制约定。还需要考虑API
的可扩展性，用户可自行增加工具。对更多即时通讯的支持性，用户可自定义任意渠道并映射为API工具，
也可以选择禁用napcat，更高自由度”。

**Recheck when:** 用户同意新增某个顶层工具，或改变记忆文件的写入方式。

**Why:** 工具表越大，每个请求的 schema 越长，模型选错工具的机会越多；专门的记忆工具把
“写一个文件”包成了一套参数与预算规则，用户不要这层包装。

**How to apply:** 评估新能力时先问“能不能是 `API` 的一个 action”；不能时把理由讲给用户，
等同意再加。`.agents/plan/2026-10-28-rebuild-design.md` 里关于 `Memory` / `Download` 工具的
段落是被否决的旧设计，不要照着恢复。
