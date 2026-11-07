---
name: release-status
description: 0.2 当前发布状态、已验证能力与发布/验证边界；接手发布或判断某项是否可用时先读
metadata:
  type: project
  scope: 整个 0.2 分支，随版本更新
  status: active
  last_verified: 2026-11-08
---

当前版本 `0.2.0-alpha.37`（本地 tag，未 push）；之后的修复记在 `CHANGELOG.md` 的 Unreleased 段，
数据库迁移到 v12。`cargo test --locked --all-targets` = 418 passed
（405 lib + 13 integration），2 ignored；clippy/fmt 通过。CI 已接入 GitHub Actions。

发布状态：每批改动升版本并打本地 annotated tag（见 [[release-workflow]]）；尚未 push/release/deploy。
当前 HEAD 未跑过 `cargo build --release`（上次早于图片依赖）。服务器 `hw1` 的
`sbot.service` 已停止。

已经由代码与测试保证、但**未在真实 NapCat 上实测**的行为：

- 发给 NapCat 的文件引用一律是 `base64://`（VFS 路径本进程读出，URL 本进程下载）。
- 私聊 `message_sent` 回声按 `target_id` 归到对方会话（依据 NapCatQQ 源码）。
- 发送账本认领 `message_sent` 回声，stand_in 下主人手打的消息进 inbox 并由 `wake-on-owner` 唤醒（`src/qq/ledger.rs` 单测覆盖；需要 `reportSelfMessage`，未在真实 NapCat 上测）。
- `approve` 的私聊确认码只接受 owner/master 从另一个账号的回复；队列持久化在
  `gate_approvals`，重启后未过期的请求与批准仍有效。

仍存在的边界（细节见 `docs/guide/limits.md` 与
`.agents/plan/2026-11-07-review-remediation.md`）：

- NapCat 未登录（2026-11-05 05:22 起 QR 失败）；`get_image`、QQ CDN 下载、
  `get_msg`/`get_forward_msg`、发出媒体都未实测。
- 服务器无系统字体：`local.media.image.fonts` 为空、`.text` 明确报错；不内置字体。
- 审查发现的 P0（子代理权限、非法 monitor 谓词、turn buffer、
  消息索引错位、联系人视图）尚未核实与修复。
- WebFetch 默认后端模式下内容在 final_url 复核前已被取走；`local = true` 才逐跳抓取。
- hooks 的 rewrite 只在 BeforeSend；SessionEnd/Stop/ConfigChange 的 inject 无去处。
- README 写 MSRV 1.85，但代码用 let-chains，实际需要 Rust 1.88+（edition 2024；待修文档）。
  `napcat-link` 固定 0.1.0-alpha.2。

**Evidence:** 2026-11-07 全量测试输出与 `git describe`；`CHANGELOG.md` Unreleased 段；
`src/security/action_gate.rs` 的 `ApprovalQueue`（数据库加载）与测试
`approvals_persist_and_match_by_nonce`。

**Recheck when:** 打新 tag、push/deploy、安装字体、NapCat 登录后实测，或改变安全默认值。

**Why:** 新会话需要知道哪些能力真的可用、哪些只有代码保证，以及 tag/构建是否可发布。

**How to apply:** 先跑测试三项与 `git describe` 确认基线；发布前重指 tag 并重跑 release；
不要把未实测的 NapCat 行为描述成已验证。
