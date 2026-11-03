---
name: release-status
description: 0.2 当前发布状态、已修复的审查项与仍未实现的配置/能力；接手下一轮工作时先读
metadata:
  type: project
  scope: 整个 0.2 分支，随版本更新
  status: active
  last_verified: 2026-11-04
---

当前版本：`0.2.0-alpha.13`（`main`）。数据库迁移到 v9；测试 287 项通过，
clippy 无警告。

alpha.13 已修复 2026-11-03 审查的 P0/P1 主体：

- 自动压缩越界 panic、消息 append 竞争与索引错位、per_chat 恢复串链、
  SSE 跨 chunk UTF-8 损坏。
- 安全闸 fail-closed、规则阈值、resume 真实复核、hold 覆盖网络工具。
- 队列锁、monitor 绕过准入、Copy 配额、死 action、owner 保护字符串目标、
  error/refusal 工具配对、索引重建/孤儿未读、MCP 行上限、Grep 已读。
- 结构：`config/sections.rs`、`tools/actions.rs`、`app::assemble_tools`、循环退出路径
  统一 `conclude`；移除未使用事件与映射方法。
- 文档：`docs/guide/` 全套、README 重写、`config.example.toml` 由测试校验、
  0.1 资产归档。

仍未实现（详见 `docs/guide/limits.md` 与 `.agents/TODO.md`）：

- `[gate]` 的逐动作审批、频率限制、`llm_review`；
- `[security].check_every_tokens` 周期检查与 `[security.guard].backend = "llm"`；
- `[compaction]` 的 `reserve_tokens` / `prune*` / `memory_flush` /
  `post_compact_recall` / `compaction_model`；
- `[memory].idle_minutes`、`review = every_turn` 的真实语义；
- `[monitor.lifecycle].catch_up`；
- WebFetch 的逐跳重定向复核与 DNS rebinding 防护；
- 仓库仍无 CI。

**Evidence:** `CHANGELOG.md` 的 alpha.13 条目、`cargo test --all-targets` 输出、
`.agents/TODO.md`。

**Recheck when:** 发布新 alpha、实现上述任一未实现项、或用户改变安全默认值。

**Why:** 审查结论分散在一次会话里；新会话需要知道“哪些已经修了、哪些只是记下来了”。

**How to apply:** 先跑测试确认基线，再从 TODO 取一项；不要把 limits 里的未实现项
在文档或提交信息里描述成已支持。
