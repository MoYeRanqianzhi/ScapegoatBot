---
name: release-status
description: 0.2 当前发布状态、已验证能力与发布/验证边界；接手发布或判断某项是否可用时先读
metadata:
  type: project
  scope: 整个 0.2 分支，随版本更新
  status: active
  last_verified: 2026-11-06
---

当前版本：`0.2.0-alpha.35`（`main`，HEAD `9f52e00`，图片功能代码 `48fe69f`）。数据库迁移到 v12；
`cargo test --locked --all-targets` = 377 passed（365 lib + 12 integration），2 ignored；
clippy/fmt 通过。CI 已接入 GitHub Actions。

发布状态：本地 annotated tag `v0.2.0-alpha.35` 存在，但落后 HEAD 4 个提交
（`git describe` = `v0.2.0-alpha.35-4-g9f52e00`）；尚未 push/release/deploy。
release build 上次验证在 image 依赖与新图片工具之前，当前 HEAD 未重跑
`cargo build --release`。服务器 `hw1` 的 `sbot.service` 已停止。

本轮能力变化：

- 图片工具从 `media.*` 收敛为 `image.*` 的 5 个 action：`info`、`transform`（裁剪/任意角度/缩放/缩略）、`convert`、`text`、`fonts`；文字使用系统字体（fontdb + imageproc + ab_glyph），**不内置字体**。
- 单会话化完成：没有 per_chat/`SessionScope`；运行时只有一条 transcript。
- 消息/证据模型在 `src/transcript.rs`，action 元数据在 crate 根 `actions.rs`；所有模块 2-循环已清零。
- `hold` 只拒绝 `napcat.*`，MCP/下载/Web/文件/记忆不受影响。

仍存在的边界（细节见 `docs/guide/limits.md` 与
`.agents/plan/2026-11-06-image-tools-and-review-handoff.md`）：

- NapCat 未登录（2026-11-05 05:22 起 QR 失败）；真实 `get_image`、QQ CDN 下载、
  `get_msg`/`get_forward_msg` 未实测。
- 服务器无系统字体：`image.fonts` 为空、`image.text` 会明确报错；需安装字体后验证。
- `approve` 只走本地 IPC；队列在内存中，重启丢失（数据库只持久化参数哈希）。
- 周期安全检查复用主 system 前缀（消息窗口独立）；`auto` 门禁覆盖 `context_sources`。
- WebFetch 默认后端模式下内容在 final_url 复核前已被取走；`local = true` 才逐跳抓取。
- hooks 的 rewrite 只在 BeforeSend；SessionEnd/Stop/ConfigChange 的 inject 无去处。
- NapCat 协议用本地 WebSocket 模拟覆盖；真实 API 尚未重跑。
- MSRV Rust 1.85（edition 2024），`napcat-link` 固定 0.1.0-alpha.2。

**Evidence:** `git log`/`git describe`（2026-11-06）、全量测试输出、`CHANGELOG.md`
alpha.35 段、`src/actions.rs`、`.github/workflows/ci.yml`、`docs/guide/limits.md`。

**Recheck when:** 发布新 alpha、push/deploy、安装字体、NapCat 登录后重测，或改变安全默认值。

**Why:** 新会话需要知道哪些能力真的可用、哪些只是解析了配置，以及 tag/构建是否可发布。

**How to apply:** 先跑测试三项与 `git describe` 确认基线；发布前重指 tag 并重跑 release；
不要把未实测的 `get_image`/字体能力描述成已验证。
