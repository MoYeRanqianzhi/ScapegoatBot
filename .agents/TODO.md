# TODO

剩余问题；已完成项删除（git 有记录）。逐条细节、来源与核实状态见
[`plan/2026-11-07-review-remediation.md`](plan/2026-11-07-review-remediation.md)。
用户 2026-11-08 目标：完成本文件全部内容，并审查、测试、检查合理性（无上帝结构、无冗余、
代码/注释/文档一致）与安全性。

## 构建规则

- 子代理禁止运行任何 `cargo` 命令；构建只在主 checkout 用默认 `target/` 统一执行。
- 不使用 C: 盘放构建产物；G: 空间不足时先清 worktree target / incremental，再报告。
- 本机 cargo 访问 crates.io 的 TLS/schannel 失败；需要拉依赖时用 `MEMORY.local.md`
  里的 rsproxy `--config` 命令，不要把镜像配置提交进仓库。
- Bash 工具会吞掉一层反斜杠；含 `\` 的编辑用 Edit 工具（见 `memory/shell-editing.local.md`）。

## 当前基线

- `cargo test --locked --all-targets` = 499 lib（Linux 500）+ 15 集成测试通过，2 ignored；clippy / fmt 通过；
  Linux（WSL Debian，`G:/sbot-wsl/check.sh check|test`）`cargo check` 与 `cargo test` 通过。
  当前 `0.2.0-alpha.43`（本地 tag）；每批升版本、打 tag，未验证前不 push。
- 服务器 `hw1` 的 `sbot.service` 仍停止；NapCat 未登录（2026-11-05 起 QR 登录失败）。

## 待用户确认

- [ ] 是否授权 `git push origin main --tags`、GitHub release、部署 hw1。
- [ ] `G:\sbot-e2e-lists`（alpha.42 实机验证 `model list` 用的临时 SBOT_HOME）没能删除
      （安全检查拦下了 `rm -rf`），需要用户手动删。

## 已知的剩余结构项（不阻塞）

- `app::assembly::build_agent` 约 208 行、`assemble_tools` 约 140 行：剩下的主要是按顺序
  接线，再拆只会把十几个句柄来回传。
- `providers/anthropic.rs` 的 `handle`（约 200 行）按事件类型分支，协议本身就这么多分支。

## 外部验证（需 NapCat 登录）

- [ ] 真实 `get_image`、QQ CDN 入站图片下载（alpha.42 起 `http://` 的 CDN 地址升级为
      `https://` 再下载，未在真实 NapCat 上验证）、`get_msg` / `get_forward_msg`。
- [ ] 发出媒体：`base64://` 在真实 NapCat 上可发；私聊回声带 `target_id`。
- [ ] 发送账本：真实 `message_sent` 回声的形状与到达时序（先于/后于响应）。
- [ ] `local.media.image.text` / `.fonts` 只在 Windows 验证过；Linux 服务器无系统字体。

## 发布

- [ ] 发布前重跑 `cargo build --release`。
- [ ] 历史重写后其他 clone 必须重新 clone 或
      `git fetch --all --prune --tags --force && git reset --hard origin/main`。
