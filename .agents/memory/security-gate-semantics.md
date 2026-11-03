---
name: security-gate-semantics
description: 安全闸在 shadow/enforce 下的确切行为、hold 覆盖范围与恢复条件；改安全策略或排查「为什么被 hold」时先读
metadata:
  type: project
  scope: src/security、src/agent/runner.rs 与 docs/guide/security.md
  status: active
  last_verified: 2026-11-04
---

安全闸的当前语义（alpha.13 起）：

- 默认 `[security.guard].mode = "shadow"`：规则层与分类器照跑、事件照记，但不 hold。
  README/指南里的「命中即 hold」只对 `enforce` 成立。
- `enforce` 且没有可用分类器（jev 缺 base_url/key，或 backend 未实现）时 **fail-closed**：
  每个新回合都会被 hold。只想跑规则层必须显式 `backend = "off"`。
- 规则命中（`rules` 后端）不受概率阈值影响；阈值只收窄分类器概率。
  `backend = "llm"` 在配置校验阶段直接拒绝。
- `agent.resume` / `sbot security resume` 用「最近一次被判危险的原始增量 + 当前历史尾部
  + hold 原因」重新过规则层与分类器；通过才删除 `~/.sbot/SECURITY_HOLD`。不再用固定串
  过闸。被判内容只存在内存（`Gate::flagged_content`），不落盘。
- hold 期间：所有 `napcat.*`、`Download`、`WebSearch`、`WebFetch`、`MCP` 拒绝；
  `agent.*` 与 `api.*` 只读查询可用。
- provider `stop_reason = refusal` 直接 `force_hold`，不重试、不换模型。

**Evidence:** `src/security/gate.rs`（`Gate::evaluate` / `review` / `enter_hold`）、
`src/agent/runner.rs`（`RuntimeOps::resume`）、`src/tools/mod.rs`（`hold_active`）、
`src/tools/{download,web}.rs`、`src/mcp/mod.rs`；测试
`security::gate::tests::{enforce_without_a_classifier_holds_instead_of_allowing,
explicit_off_backend_stays_rules_only, rule_hits_are_not_suppressed_by_the_threshold,
resume_review_uses_the_flagged_content}`。

**Recheck when:** 修改 `GuardConfig` 默认值、实现 `backend = "llm"` 或周期检查时。

**Why:** 默认 shadow 与文档的「永久 hold」曾被混为一谈；改动者容易再把 fail-open 当默认。

**How to apply:** 改安全逻辑前先跑上述测试；文档表述必须区分 shadow/enforce，
新增对外工具要在 hold 期间拒绝，并在 `docs/guide/limits.md` 登记能力边界。
