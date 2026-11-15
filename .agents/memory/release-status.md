---
name: release-status
description: 0.2 当前发布状态、已验证能力与发布/验证边界；接手发布或判断某项是否可用时先读
metadata:
  type: project
  scope: 整个 0.2 分支，随版本更新
  status: active
  last_verified: 2026-11-16
---

当前版本 `0.2.0-alpha.44`，已发布（2026-11-15 UTC，tag `v0.2.0-alpha.44` = 提交 `95830ea`，
GitHub release 标为 Latest）。数据库 schema v13。最低 Rust 1.89。

发布了什么、怎么验证的：

- 20 个压缩包 + `SHA256SUMS` + 构建来源证明。x86_64 的 v1–v4 × {linux-gnu（glibc 2.28）、
  linux-musl（全静态）、windows-msvc（静态 CRT）、macOS 11+}，aarch64 × 同四种。流程与每一步
  的理由写在 `.github/workflows/release.yml` 与 `.github/scripts/*.sh` 的注释里。
- CI 上每个产物：链接检查、实际运行（空数据目录上 start → status → doctor → stop）、同参数
  编译的库单元测试（Linux / macOS 529 个，Windows 527 个）。**v4 是否在 CI 上跑过取决于当次
  分到的 runner 有没有 AVX-512**：alpha.44 里 `x86_64_v4` 的 macOS、Windows、linux-musl 三个
  只做了链接检查（发布说明里列出），linux-gnu 的 v4 跑过。
- 发布后本地实测：Debian 12（WSL，glibc 2.36，CPU 是 v3）上 gnu 与 musl 的 v1–v3 六个包都跑
  通同一套冒烟脚本，v4 两个按预期以非法指令退出（退出码 132）；Windows 11 上 x86_64 的 v1–v3
  同样跑通。`sha256sum -c` 与 `gh attestation verify` 通过，证明指向 release.yml 与 tag 的提交。
- **没有实测过**：任何 aarch64 产物与 macOS 产物在 CI 之外的机器上；glibc 正好 2.28 的发行版
  （靠 manylinux_2_28 容器与符号版本检查保证）；v4 产物在真正支持 AVX-512 的非 CI 机器上。

测试基线（alpha.44）：`cargo test --locked --all-targets` = 527 lib（Linux 529）+ 18 集成测试
通过，2 ignored；clippy / fmt / rustdoc `-D warnings` / cargo-deny / zizmor / actionlint /
shellcheck / CodeQL 在 CI 上全过。

服务器 `hw1` 的 `sbot.service` 仍停止，部署未授权；NapCat 未登录（2026-11-05 起 QR 失败）。

已经由代码与测试保证、但**未在真实 NapCat 上实测**的行为：

- 发给 NapCat 的文件引用一律是 `base64://`（VFS 路径本进程读出，URL 本进程下载）。
- 私聊 `message_sent` 回声按 `target_id` 归到对方会话（依据 NapCatQQ 源码）。
- 发送账本认领 `message_sent` 回声，stand_in 下主人手打的消息进 inbox 并由 `wake-on-owner`
  唤醒（`src/qq/ledger.rs` 单测覆盖；需要 `reportSelfMessage`）。
- `approve` 的私聊确认码只接受 owner/master 从另一个账号的回复；队列持久化在
  `gate_approvals`，重启后未过期的请求与批准仍有效。
- `get_image`、QQ CDN 下载、`get_msg` / `get_forward_msg`、发出媒体。

其他边界见 `docs/guide/limits.md`（该目录计划并入 skill，见 [[docs-layout]]）。

**Evidence:** release 运行 43747825572 与试跑 43742813837（全部任务 success）；
`gh release view v0.2.0-alpha.44`（21 个资产）；2026-11-16 在 WSL Debian 12 与本机 Windows 上
用 `.github/scripts/smoke-test.sh` / `check-binary.sh` 对下载的包实跑的输出。

**Recheck when:** 打新 tag、部署、NapCat 登录后实测，或改发布矩阵。

**Why:** 新会话需要知道哪些能力真的可用、哪些只有代码或 CI 保证。

**How to apply:** 说某个平台“可用”之前看上面它属于哪一类；发新版按 [[release-workflow]]。
