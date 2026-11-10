---
name: tool-surface
description: agent 的工具表由用户决定：没有 Memory / Download 工具；想新增工具、或把某个功能做成独立工具前先读
metadata:
  type: feedback
  scope: src/tools 的工具表、子代理工具表、system prompt 里的工具说明
  status: active
  last_verified: 2026-11-10
---

用户 2026-11-10 明确：**从未同意过 `Memory` 与 `Download` 这两个工具**，它们是 agent 私自
加进设计文档与代码的。

- **记忆文件由文件工具直接读写**：`/MEMORY.md`、`/USER.md`、`/memory/**` 用 `Read` /
  `Write` / `Edit`，没有专门的记忆工具。
- **下载是非必要功能，放进 `API`**（`local.*` 下的 action），不占一个顶层工具。
- 推论：顶层工具表只放 agent 时刻都要用的基本能力；其余功能做成 `API` 的 action（命名
  要体现完整归属，见 [[design-decisions]] 的“工具命名分层”）。**新增顶层工具必须先得到用户
  同意**，设计文档里写了不等于用户同意。

**Evidence:** 用户 2026-11-10 原话：“我从来没有同意有工具Memory和Download，这都是私自
加入的，记忆文件直接文件工具读写，下载这种非必要功能明显要放在API工具中”。

**Recheck when:** 用户同意新增某个顶层工具，或改变记忆文件的写入方式。

**Why:** 工具表越大，每个请求的 schema 越长，模型选错工具的机会越多；专门的记忆工具把
“写一个文件”包成了一套参数与预算规则，用户不要这层包装。

**How to apply:** 评估新能力时先问“能不能是 `API` 的一个 action”；不能时把理由讲给用户，
等同意再加。`.agents/plan/2026-10-28-rebuild-design.md` 里关于 `Memory` / `Download` 工具的
段落是被否决的旧设计，不要照着恢复。
