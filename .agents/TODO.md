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

- `cargo test --locked --all-targets` = 452 passed，2 ignored；clippy / fmt 通过；
  Linux `cargo check -D warnings` 通过（WSL Debian，脚本 `G:/sbot-wsl/check.sh`，
  产物与 cargo 缓存都在 G:；WSL 工具链没有 clippy）。
  当前 `0.2.0-alpha.40`（本地 tag）；每批升版本、打 tag，未验证前不 push。
- 服务器 `hw1` 的 `sbot.service` 仍停止；NapCat 未登录（2026-11-05 起 QR 登录失败）。

## 待用户确认

- [ ] 是否授权 `git push origin main --tags`、GitHub release、部署 hw1。

## 预算、定时与 provider（P1）

- [ ] 预算缺口：子代理、压缩、记忆 flush、审查、门禁 LLM 不计入；cache token 丢弃；口径不一。
- [ ] 每轮 token 上限把每次请求的完整 prompt 相加；预算耗尽后每次唤醒都重复压缩。
- [ ] 热切换模型后 context_window / 图片能力 / 价格不更新。
- [ ] 定时器：启动覆盖上次触发时间；`At` 只触发一次；DST；cron 补投窗口；日/周字段 AND。
- [ ] Anthropic：server tool 参数未收集；空 content / 无签名 thinking 回放 400；只缓存静态前缀。
- [ ] 未知停止原因当 EndTurn；408/504 不重试。
- [ ] `/messages` 的 `offset` / `order` 被忽略；`Stat` 动态文件失败；`Read` 示例过滤语法不存在。
- [ ] 资源放大：图片/下载整块驻留内存；无 `spawn_blocking`；`html_to_text` 可能二次方。

## 配置重构

- [ ] 实现或删除不起作用的键：`[memory].review_model/topics`、
      `[[monitor]].jitter_secs`、`source = stream|internal`、`[[hooks.events]].when/inject`、
      `[queue].mode = collect`、默认保留下的 `max_session_bytes`、`sbot chat --scope`。
- [ ] hooks 事件/动作矩阵与文档不一致（文档说所有事件支持 log/block/inject/notify）。
- [ ] `config.example.toml` 默认值与代码不一致处。

## 文档、命名与结构

- [ ] 文档漂移（计划 §5）：MSRV 1.88、README 命令/目录树、各 guide 的若干条、
      `workspace/*.md` 0.1 遗留模板。
- [ ] 工具描述漂移：`fs.rs` 过滤语法与 mount 承诺、monitor `internal`/`catch_up`、
      swarm general 子代理“不能改 /MEMORY.md”。
- [ ] 误导性命名：`tests/live_napcat.rs`、`QqMessage::backfilled`、`Permission`、
      `DynamicProvider`、忽略参数的函数、`wake-on-` 前缀决定投递路径。
- [ ] P2 结构：`app/assembly.rs` 拆分、runner 重复、monitor 拆分、Monitor spec 三处解析、
      vfs/sqlite 重复、零引用 `pub fn`。

## 外部验证（需 NapCat 登录）

- [ ] 真实 `get_image`、QQ CDN 入站图片下载、`get_msg` / `get_forward_msg`。
- [ ] 发出媒体：`base64://` 在真实 NapCat 上可发；私聊回声带 `target_id`。
- [ ] 发送账本：真实 `message_sent` 回声的形状与到达时序（先于/后于响应）。
- [ ] `local.media.image.text` / `.fonts` 只在 Windows 验证过；Linux 服务器无系统字体。

## 发布

- [ ] 发布前重跑 `cargo build --release`。
- [ ] 历史重写后其他 clone 必须重新 clone 或
      `git fetch --all --prune --tags --force && git reset --hard origin/main`。
