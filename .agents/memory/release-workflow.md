---
name: release-workflow
description: 版本号、tag、push 与发布的规则和实际步骤；每完成一批改动、准备发版时读
metadata:
  type: feedback
  scope: 本仓库所有提交与发布
  status: active
  last_verified: 2026-11-17
---

规则：

- 每完成一批改动：升版本号（`Cargo.toml` + `Cargo.lock` 的 `scapegoat-bot` 条目，格式
  `x.y.z-alpha.N`，N 递增），把 CHANGELOG 的 `## [Unreleased]` 改成
  `## [<版本>] - <日期>`，提交后打 annotated tag，不用问。版本号要持续更新，不让多批改动堆
  在同一个版本号下。
- **push 与 GitHub release 已授权**（用户 2026-11-13：“强推远端”“允许GitHub release”）。
  **部署 hw1 没有授权**，要单独问。
- 推 tag 就是发布：`release.yml` 由 `v*` tag 触发，自己建 release。一次只推一个 tag
  （一次推 3 个以上 GitHub 不产生事件）。

发版步骤（alpha.44 实际走过的）：

1. main 上 CI 全绿（`gh run list --branch main`）。
2. 先试跑：`gh workflow run release.yml --ref main`（workflow_dispatch 不发布，约 50 分钟）。
   改了 `release.yml` 或 `.github/scripts/` 时必须；没改也建议——它在 20 个目标上跑库测试，
   alpha.45 靠它查出两个 CI 上没出现的测试问题（见 [[release-status]]）。
3. 升版本、改 CHANGELOG 标题，本地 fmt / clippy / test，提交 `chore(release): <版本>`。
4. `git tag -a v<版本> -m v<版本>`，`git push origin main`，`git push origin v<版本>`。
5. 发布跑完后按 [[release-status]] 里的方法抽查下载的包。

verify 任务会拦下：tag 与 `Cargo.toml` 版本不一致、提交不在 main 上、CHANGELOG 没有该版本段。

不归 Dependabot 管、要手动升级的：`release.yml` 里 manylinux 镜像的 tag 与摘要；`ci.yml` 里
actionlint 的版本与校验和。

**Evidence:** 用户 2026-11-07：“你自动打tag，但是先不push因为没验证过，注意版本号需要持续
更新”；2026-11-13 的两句授权（上文）；alpha.44 的发布运行 43747825572。

**Recheck when:** 用户收回或扩大授权，或改变版本策略。

**Why:** tag 方便回溯每一批改动；发布是公开动作，授权范围要写清楚，免得下个会话要么不敢推、
要么顺手部署。

**How to apply:** 照上面的步骤；任何涉及 hw1 的动作先问用户。
