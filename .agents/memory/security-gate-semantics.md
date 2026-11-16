---
name: security-gate-semantics
description: 安全闸在 shadow/enforce 下的确切行为、hold 覆盖范围与恢复条件；改安全策略或排查「为什么被 hold」时先读
metadata:
  type: project
  scope: src/security、src/tools/api/policy.rs、src/agent/runner/ops.rs 与 skills/sbot-operate/references/security.md
  status: active
  last_verified: 2026-11-17
---

安全闸（输入/输出两处）的当前语义：

- 默认 `[security.guard].mode = "shadow"`：规则层与分类器照跑、事件照记，但不 hold。
  文档里的「命中即 hold」只对 `enforce` 成立。**shadow 不等于永远不会 hold**：provider 拒答
  与周期安全检查（默认开着）这两条路径不看 `mode`。
- 后端 `jev` / `llm` / `off`。`llm` 已实现：provider 就绪后由 `app/assembly/providers.rs` 调
  `Gate::install_judge` 装入；装入前按“分类器不可用”处理。
- `enforce` 且配置了分类器后端却没有可用判定器（jev 缺 base_url/key、llm 尚未装入）时
  **fail-closed**：每个新回合都被 hold。只想跑规则层必须显式 `backend = "off"`。
  分类器请求失败同样判危险。guard 预算用尽时退回只跑规则层（会告警）。
- 规则命中不受概率阈值影响；阈值只收窄分类器给出的概率。
- `agent.resume` / `sbot security resume` 复核的是**当前已提交上下文的尾部**（约 4.8 万字符）
  加 hold 原因，重新过规则层与分类器（`Gate::review`，忽略 shadow/enforce）；通过才删除
  `~/.sbot/SECURITY_HOLD`，上下文为空直接解除。**不复核当初被判危险的原文**，也不保存它
  （hold 文件里只有原因）：原文里可能就有凭据，而且复核它只会再次命中，hold 永远解不开——
  这个 agent 无人值守。输出闸命中的内容没进历史，上下文干净就能恢复；输入闸命中的还在历史
  里，agent 要先 `agent.quarantine` 或 `agent.clear_context`。分类器不可用时复核按不通过
  处理，原因是 `classifier unavailable: …`，隔离解决不了。
- **hold 只拒绝 `napcat.*`**（`tools/api/policy.rs` 的单一检查点）。MCP、Download、
  WebSearch/WebFetch、文件与记忆工具不受 hold 影响（用户 2026-11-06 决定）。
- provider `stop_reason = refusal` 直接 `force_hold`，不重试、不换模型。
- **动作门禁的发送规则比的是“门禁检查过的那一份内容”**（`tools/api/policy.rs`）：同一段文字
  写成字符串还是 `text` 段，规范化后算同一内容（`security/action_gate.rs` 的
  `message_content`）；记进历史用于以后比较的是 `before_send` 改写**之前**的视图，不是改写后
  发出去的参数。以前两处都不是这样：重复 / 跨群同文规则对字符串形式的消息从不生效，改写
  hook 会让“检查的”与“记下的”对不上。

**Evidence:** `src/security/gate.rs`（`from_config` / `evaluate` / `review` / `enter_hold`）、
`src/agent/runner/ops.rs`（`RuntimeOps::resume`）、`src/tools/api/policy.rs`（hold 检查）、
`src/app/assembly/providers.rs`（llm judge 装入）；测试 `security::gate::tests::{
enforce_without_a_classifier_holds_instead_of_allowing, explicit_off_backend_stays_rules_only,
rule_hits_are_not_suppressed_by_the_threshold, hold_records_the_reason_but_never_the_flagged_text,
llm_backend_can_be_installed_after_construction}`、
`agent::runner::ops::tests::the_agent_can_lift_an_output_hold_by_itself`、
`tools::api::tests::a_repeated_message_is_blocked_in_every_form`（2026-11-17 核对存在）；
hold 范围提交 `b766619`，门禁比较内容的提交 `d3739f6`。

**Recheck when:** 修改 `GuardConfig` 默认值、hold 范围、resume 复核输入或周期检查时。

**Why:** 默认 shadow 与「命中即 hold」曾被混为一谈；改动者容易再把 fail-open 当默认，
或把已被用户收窄的 hold 范围重新扩大。

**How to apply:** 改安全逻辑前先跑上述测试；文档必须区分 shadow/enforce；
新增 QQ 侧动作必须经过 `policy.rs` 的检查链，并在 `skills/sbot-operate/references/limits.md` 登记能力边界。
