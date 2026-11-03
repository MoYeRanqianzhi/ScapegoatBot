---
name: release-status
description: 0.2 当前发布状态、已实现能力与真实边界；接手下一轮工作或判断某项是否可用时先读
metadata:
  type: project
  scope: 整个 0.2 分支，随版本更新
  status: active
  last_verified: 2026-11-04
---

当前版本：`0.2.0-alpha.14`（`main`）。数据库迁移到 v9；测试 310 项通过，
`cargo fmt --check`、`clippy -D warnings` 通过；CI 已接入 GitHub Actions。

alpha.13 修掉了 2026-11-03 审查的 P0/P1 主体；alpha.14 补齐了当时列为未实现的项：

- `[gate]`：trusted/auto/approve、速率、重复/跨群同文/@all、`llm_review`、
  IPC 审批（`sbot gate pending/approve/reject`）。
- owner intent：runtime 证据（控制通道 / 主人消息 / origin chat），跨 chat 与
  扩张动作强制，收缩与同 chat 回复豁免。
- 安全分类器 `backend = "llm"`；周期安全检查（high+complied → hold，
  critical → pause agent）。
- 压缩：模型窗口减 reserve 的阈值、旧工具结果裁剪、memory flush、post-compact
  索引注入、独立 compaction model。
- 记忆复盘 idle/every_turn 的真实语义；monitor catch_up；出站账本启动核对；
  Download 的公网 DNS 解析器；WebFetch final_url 复核。
- 结构：views 拆成 6 个子模块、ScopeCache、open_stores；全仓 rustfmt；CI。

仍存在的边界（细节见 `docs/guide/limits.md`）：

- `approve` 只走本地 IPC，没有 master QQ nonce；队列在内存里，重启丢失。
- `auto` 规则是启发式（没有跨 chat n-gram、媒体来源检查）。
- 周期检查使用独立窗口，不与主请求共享前缀缓存。
- 群级破坏动作（全员禁言/退群/解散）没有用户目标，不做 owner 保护。
- WebFetch 的抓取由后端完成，内容在 final_url 复核前已被取走。
- `[security.guard].max_state_tokens` / `delta_every_tokens`、
  `[style].sample_retention_days`、`models.json` 的 `default_thinking_level` 仍未使用。

**Evidence:** `CHANGELOG.md` 的 alpha.14 条目、`cargo test --all-targets` 输出、
`.github/workflows/ci.yml`、`docs/guide/limits.md`。

**Recheck when:** 发布新 alpha、实现上述边界、或改变安全默认值（shadow/enforce）。

**Why:** 审查结论与后续实现分散在两次会话里；新会话需要知道哪些能力真的可用、
哪些只是“解析了配置”。

**How to apply:** 先跑三项检查确认基线；不要把 limits 里的边界在文档/提交里描述成
已支持；新增能力时同步 `docs/guide`、`limits.md` 与 `.agents/TODO.md`。
