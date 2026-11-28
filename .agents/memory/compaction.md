---
name: compaction
description: 历史压缩（同步与后台提前准备）必须守住的不变量；改压缩、会话轮转、回合锁或压缩前安全复核时读
metadata:
  type: project
  scope: src/agent/runner/compaction.rs 的 Runner 压缩路径、会话轮转
  status: active
  last_verified: 2026-11-28
---

一次压缩分两步：**准备**（切分、PreCompact hook、记忆提取、请求摘要；只读快照，不改历史与分段）
和**安装**（校验后轮转分段、替换历史）。同步压缩是“拿锁 → 准备 → 安装”；后台提前准备在回合
结束后启动，下一个回合开头安装（用户 2026-11-17 选的“空闲预压缩”）。完成后计划文件已删，设计与
逐步的决定记录在 git 历史里：`git show 5c839ea:.agents/plan/2026-11-28-idle-precompaction.md`。

必须守住的：

1. **安装前校验两样东西**，在同一把历史锁里：当前历史仍以快照逐条开头（清空、隔离、别的压缩
   都会让它不成立），当前分段仍是快照时的那个。任一不符就作废，历史与分段都不动。
2. **后缀原样接回**：快照之后追加的消息按原顺序接在保留窗口之后、PostCompact 上下文之前；
   校验、`rotate_with` 与替换内存历史在同一把历史锁里完成。
3. **保留窗口与后缀里 assistant 消息的 `usage` 清零**：`estimate_messages` 拿最后一个非零
   用量当基准，那是压缩前的提示长度，留着会让新历史仍被估为超限、下次又压缩一次。
4. **后台准备在整个准备期间持有 `compaction_lock`**（拿到锁再 spawn，把 guard 交给任务，先
   发布结果或记下失败再释放）：“有后台准备在跑” ⇔ 锁被占着。硬阈值时等这把锁一次再试安装，
   不重复发摘要请求；软阈值时用 `try_lock`，不等。
5. **后台准备不做安全复核**。复核只在回合锁里做：同步压缩在摘要前复核快照，后台结果在安装前
   复核安装时的完整历史（快照 + 后缀）；判 critical（`discard_context` 置位）就不摘要 / 不安装。
   原因：后台复核的结论会晚于下一回合开头的丢弃检查，而且 `run_security_review` 结束时把
   `unchecked_tokens` 清零，会抹掉复核期间回合新累计、快照里没有的那部分。
6. 后台与同步共用 `compaction_failures` 和 `max_failures` 上限；任何一次成功安装或清空上下文
   都清零。

**Evidence:** `src/agent/runner/compaction.rs`：`prepare_compaction`、`install_compaction`、
`maybe_start_precompaction`、`maybe_auto_compact`、`adopt_precompaction`、`compact_scope`；
`src/agent/runner.rs` 测试模块里的 `idle_compaction_*`、`hard_threshold_compaction_waits_for_the_background_summary`、
`synchronous_compaction_does_not_request_a_summary_after_critical_review`（Codex 2026-11-28 临时
去掉后缀接回、前缀校验、清空时丢弃、硬阈值等待，四条测试各自失败，恢复后通过）。第 5 条的两处
竞态是 Codex 2026-11-28 核对源码时指出的。

**Why:** 压缩会整体替换历史与分段；校验或锁的次序一错，就会悄悄丢消息、复活被隔离的原文，
或把被判 critical 的上下文摘要进新分段。

**How to apply:** 改压缩相关代码时逐条对照上面六条；新增会改写已提交历史的操作（类似隔离）时，
确认它会让前缀校验失败，而不是被安装覆盖。

**Recheck when:** 历史追加或隔离的方式、分段轮转、token 估算基准、压缩或回合锁的用法、安全
复核的记账方式变化。
