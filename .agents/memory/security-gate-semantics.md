---
name: security-gate-semantics
description: 安全闸在 shadow/enforce 下的确切行为、hold 覆盖范围与恢复条件；改安全策略或排查「为什么被 hold」时先读
metadata:
  type: project
  scope: src/security、src/tools/api/policy.rs、src/agent/runner/ops.rs 与 docs/guide/security.md
  status: active
  last_verified: 2026-11-07
---

安全闸（输入/输出两处）的当前语义：

- 默认 `[security.guard].mode = "shadow"`：规则层与分类器照跑、事件照记，但不 hold。
  文档里的「命中即 hold」只对 `enforce` 成立。
- 后端 `jev` / `llm` / `off`。`llm` 已实现：provider 就绪后由 `assembly.rs` 调
  `Gate::install_judge` 装入；装入前按“分类器不可用”处理。
- `enforce` 且配置了分类器后端却没有可用判定器（jev 缺 base_url/key、llm 尚未装入）时
  **fail-closed**：每个新回合都被 hold。只想跑规则层必须显式 `backend = "off"`。
  分类器请求失败同样判危险。guard 预算用尽时退回只跑规则层（会告警）。
- 规则命中不受概率阈值影响；阈值只收窄分类器给出的概率。
- `agent.resume` / `sbot security resume` 用「最近一次被判危险的原始增量 + 当前历史尾部
  + hold 原因」重新过规则层与分类器（`Gate::review`，忽略 shadow/enforce）；通过才删除
  `~/.sbot/SECURITY_HOLD`。被判内容只存在内存（`Gate::flagged_content`），不落盘；
  重启后内存里没有被判内容，只剩历史尾部参与复核。
- **hold 只拒绝 `napcat.*`**（`tools/api/policy.rs` 的单一检查点）。MCP、Download、
  WebSearch/WebFetch、文件与记忆工具不受 hold 影响（用户 2026-11-06 决定）。
- provider `stop_reason = refusal` 直接 `force_hold`，不重试、不换模型。

**Evidence:** `src/security/gate.rs`（`from_config` / `evaluate` / `review` / `enter_hold`）、
`src/agent/runner/ops.rs`（`RuntimeOps::resume`）、`src/tools/api/policy.rs`（hold 检查）、
`src/app/assembly.rs`（llm judge 装入）；测试 `security::gate::tests::{
enforce_without_a_classifier_holds_instead_of_allowing, explicit_off_backend_stays_rules_only,
rule_hits_are_not_suppressed_by_the_threshold, resume_review_uses_the_flagged_content,
llm_backend_can_be_installed_after_construction}`；hold 范围提交 `d21eed4`。

**Recheck when:** 修改 `GuardConfig` 默认值、hold 范围、resume 复核输入或周期检查时。

**Why:** 默认 shadow 与「命中即 hold」曾被混为一谈；改动者容易再把 fail-open 当默认，
或把已被用户收窄的 hold 范围重新扩大。

**How to apply:** 改安全逻辑前先跑上述测试；文档必须区分 shadow/enforce；
新增 QQ 侧动作必须经过 `policy.rs` 的检查链，并在 `docs/guide/limits.md` 登记能力边界。
