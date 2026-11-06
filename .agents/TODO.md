# TODO

剩余问题；已完成项删除（git 有记录）。逐条细节、来源与核实状态见
[`plan/2026-11-07-review-remediation.md`](plan/2026-11-07-review-remediation.md)。

## 构建规则

- 子代理禁止运行任何 `cargo` 命令；构建只在主 checkout 用默认 `target/` 统一执行。
- 不使用 C: 盘放构建产物；G: 空间不足时先清 worktree target / incremental，再报告。
- 本机 cargo 访问 crates.io 的 TLS/schannel 失败；需要拉依赖时用 `MEMORY.local.md`
  里的 rsproxy `--config` 命令，不要把镜像配置提交进仓库。

## 当前基线

- `cargo test --locked --all-targets` = 401 passed（388 lib + 13 integration），2 ignored；
  clippy / fmt 通过。改动记在 `CHANGELOG.md` 的 Unreleased 段，尚未打 tag。
- 本地 tag `v0.2.0-alpha.35` 未指向 HEAD；尚未 push/release/deploy。
- 服务器 `hw1` 的 `sbot.service` 仍停止；NapCat 未登录（2026-11-05 起 QR 登录失败）。

## 待用户确认

- [ ] hold 是否只允许控制通道解除（计划 §3 P0-1，改变原设计）。
- [ ] `image.*` 5 个 action（`info`/`transform`/`convert`/`text`/`fonts`）的命名与参数；
      确认前不要继续扩展图片 API。
- [ ] 是否授权 `git push origin main --tags`、GitHub release、部署 hw1。

## 修复（按计划 §3 / §6 的顺序）

- [ ] P0-2 ~ P0-7：子代理权限与急停、steering 丢消息、非法 monitor 谓词、turn buffer
      只在提交时写、消息索引错位、联系人视图为空。先核实再修。
- [ ] P1 安全项：收缩类群管理的 chat 绑定、`/config` 写保护与密钥暴露、回收站配额。
- [ ] 文档与命名漂移（计划 §5；含 11-06 的文档 P1 清单）。
- [ ] P2 结构拆分与冗余合并（计划 §3 P2）。

## 外部验证（需 NapCat 登录）

- [ ] 真实 `get_image`、QQ CDN 入站图片下载、`get_msg` / `get_forward_msg`。
- [ ] 发出媒体：`base64://` 的图片/语音/文件在真实 NapCat 上可发；私聊 `message_sent`
      回声确实带 `target_id`（代码依据 NapCatQQ 源码，未实测）。
- [ ] `image.text` / `image.fonts` 只在 Windows 验证过；Linux 服务器无系统字体。
      不内置字体（用户明确要求）。

## 发布

- [ ] 发布前重指 tag 到最终 HEAD（或开 alpha.36），重跑 `cargo build --release`；
      上次 release 构建早于图片依赖。
- [ ] 历史重写后其他 clone 必须重新 clone 或
      `git fetch --all --prune --tags --force && git reset --hard origin/main`。
