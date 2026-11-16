---
name: prompt-structure
description: 模型面文本怎么分层、一段新文本该放哪里、哪些测试守着它；新增注入块、改 system prompt 结构或加任何模型可见文本前先读
metadata:
  type: project
  scope: assets/system_prompt_v5.txt、src/prompt/、各工具的 description 与模型可见的错误文本
  status: active
  last_verified: 2026-11-17
---

写法见 [[prompt-style]]；这里是结构上的规则。

**三层，从稳定到易变。** 顺序是为了 prompt cache：越靠前越不能变。

1. 静态前缀 `prompt::render_static(mode)`：字节级稳定，不含时间、模型名、QQ 号、收件箱、
   技能索引或任何文件内容。
2. 会话级注入 `prompt::context::load`：会话开始时构建、压缩成功后重建。`/AGENTS.md`、
   agent 自己在 `/hooks/context.toml` 列出的文件、`/USER.md`、风格档案、`/MEMORY.md`、技能
   索引，然后是 `session_start` hook 的 `inject_context`。顺序与各自上限以代码为准。
3. runtime 块：每轮作为独立的上下文消息注入，不进 system；它必须是动态部分的最后一块。

**谁决定注入什么**（用户 2026-11-07）：只有 `/AGENTS.md` 与记忆 / 风格 / 技能系统自己的产物由
程序固定注入。`/SOUL.md` 之类的文件程序不认识，注不注入由 agent 自己决定（自己读，或写进
`/hooks/context.toml`）。

**两套身份。** `[agent].mode` 启动时固定。`stand_in` 与 `companion` 的静态前缀只差第一段身份，
其余逐字相同。

**新增一个注入用的标签**时三处一起改：`src/qq/format.rs` 的 `INTERNAL_TAGS`（发送前剥离，
否则内部标记会被发到群里）、静态 prompt 里列出内部标签的那一段、对应测试。

**一段新文本放哪里**——同一条规则只写一处，写两处一定会漂移：

1. 策略（何时用、边界、优先级）→ system prompt；
2. 参数、返回、错误、action 列表 → 工具 description；
3. 压缩、子代理、门禁审查、复盘 → 各自的独立 prompt（`.agents/plan/2026-10-29-prompt-catalog.md`
   是当初的英文稿，以代码里的常量为准）；
4. 运行时字段 → runtime 块；
5. 状态提示 → `<system_notice>`，不要伪装成用户消息。

**分类器的问题要是感知型的**（“这段文本里有没有这种东西”），不是判断型（“该不该拦”）；阈值与
动作留在 runtime，不告诉分类器阈值或规则编号。

用户内容（QQ 消息、VFS 文件、记忆、技能正文、网页）保持原文，不翻译。

**守着这些的测试**（`src/prompt/mod.rs`）：
`static_prompt_starts_with_identity_and_covers_the_essentials`、
`static_prompt_does_not_repeat_retired_claims`、
`every_internal_tag_named_in_the_prompt_is_stripped_before_sending`、
`companion_replaces_only_the_identity`；另有 `prompt::context::tests` 与
`tests/source_hygiene.rs`（模型可见字符串里没有断裂的缩进）。

**Evidence:** 原 `docs/development/prompt-maintenance.md`（2026-11-17 随目录删除，git 历史可查）；
上面的函数、常量与测试名于 2026-11-17 在代码里核对存在。注入归属是用户 2026-11-07 的决定，
`sbot.toml` 的 `[[hooks.context]]` 因此被移除。

**Recheck when:** system prompt 换版本文件；注入块的组成或顺序改变；上面任何一个测试被改名
或删除。

**Why:** 静态前缀变一个字节，所有会话的缓存一起失效；同一条规则写在两处，改的人只会改到
一处。

**How to apply:** 加文本前先按上面五类归位；改完跑 `cargo test --lib prompt`。
