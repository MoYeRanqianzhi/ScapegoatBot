---
name: 历史压缩的不变量
description: 改动压缩、会话轮转、回合锁或压缩前安全复核时读取
metadata:
  type: project
  scope: Runner 的同步与后台压缩、会话轮转
  status: active
  last_verified: 2026-11-28
---

**Evidence:**

- `src/agent/runner/compaction.rs:304`（`Runner::prepare_compaction`）只从快照准备摘要，
  可以执行 PreCompact 和提取记忆，但不改历史、不轮转分段；同步与后台都只有在
  `src/agent/runner/compaction.rs:339`（`Runner::install_compaction`）才安装。
- `src/agent/runner/compaction.rs:339`（`Runner::install_compaction`）持有历史锁，
  既逐条校验当前历史仍以快照开头，也校验当前分段仍是快照时的文件；任一不符就作废，
  不写新历史或新分段。
- `src/agent/runner/compaction.rs:339`（`Runner::install_compaction`）将快照之后的消息
  原序原样（仅 `usage` 例外）接到保留窗口之后、PostCompact 上下文之前，再用同一份
  新历史写入分段并替换内存。
- `src/agent/runner/compaction.rs:339`（`Runner::install_compaction`）将保留窗口和后缀
  的 assistant `usage` 都清零。`src/session/compaction.rs:76`（`estimate_messages`）优先用
  最后一次非零用量做估算基准；这些用量描述的是压缩前的提示，保留可能让新历史仍被估为超限。
- `src/agent/runner/compaction.rs:103`（`Runner::maybe_start_precompaction`）先拿到
  `compaction_lock`，把锁交给后台任务，直到发布结果或记录失败才释放。低于硬阈值时不等待
  正在准备的摘要；超过时先等已有任务，再决定是否同步摘要（同文件 `Runner::maybe_auto_compact`）。
- `src/agent/runner/compaction.rs:103`（`Runner::maybe_start_precompaction`）与同文件
  `Runner::maybe_auto_compact` 共用 `compaction_failures` 和停用上限。`Runner::install_compaction`
  成功或 `Runner::clear_all_context` 清空时将它重置。
- `check_on_compaction` 打开时，`src/agent/runner/compaction.rs:64`（`Runner::adopt_precompaction`）
  在回合锁内把安装时的完整历史（含后缀）交给安全复核；critical 时丢弃准备结果。
  `Runner::compact_scope` 同样在回合锁内先复核，critical 时不请求摘要。后台不复核、不拿
  `review_lock`，不重置 `unchecked_tokens`（`src/agent/runner/review.rs:39` 的
  `Runner::run_security_review` 收到成功的复核结论时才重置）。

**Why:** 仅追加的回合不能丢；隔离能改已提交的旧消息而不改审计分段，轮转也会改变摘要的来源。
前缀和分段都要匹配。复核必须覆盖采用时的新消息；后台复核还可能把未复核的新增用量一起清掉。

**How to apply:** 保持校验、`rotate_with` 和替换历史在同一把历史锁内；保留后缀与 `usage` 的
例外。改锁时检查软阈值不等待、硬阈值不重复摘要，以及压缩的安全复核只在前台的回合锁内完成。

**Recheck when:** 历史追加或隔离、分段轮转、token 估算基准、调度锁或安全复核记账方式变化。
