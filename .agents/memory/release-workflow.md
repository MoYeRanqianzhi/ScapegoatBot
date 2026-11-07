---
name: release-workflow
description: 版本号、tag 与 push 的规则；每完成一批改动、准备提交收尾时读
metadata:
  type: feedback
  scope: 本仓库所有提交与发布
  status: active
  last_verified: 2026-11-07
---

- 每完成一批改动：升版本号（`Cargo.toml` + `Cargo.lock` 的 `scapegoat-bot` 条目，
  格式 `x.y.z-alpha.N`，N 递增），把 CHANGELOG 的 Unreleased 段改成该版本，提交后
  **自动打 annotated tag**，不用问。
- **不要 push**（包括 tag），直到真实环境验证过；push / release / 部署 hw1 仍要用户明确授权。
- 版本号必须持续更新，不能让多批改动堆在同一个版本号下。

**Evidence:** 用户 2026-11-07：“你自动打tag，但是先不push因为没验证过，注意版本号需要持续更新”。

**Recheck when:** 用户授权 push，或改变版本策略。

**Why:** 本地 tag 方便回溯每一批改动；未经实机验证的版本不能发布。

**How to apply:** 每批结尾：bump → `cargo test --locked --all-targets` → 提交 → `git tag -a vX -m ...`；
不跑 `git push`。
