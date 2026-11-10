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

- `cargo test --locked --all-targets` = 504 lib（Linux 505）+ 15 集成测试通过，2 ignored；clippy / fmt 通过；
  Linux（WSL Debian，`G:/sbot-wsl/check.sh check|test`）`cargo check` 与 `cargo test` 通过。
  当前 `0.2.0-alpha.43`（本地 tag）；每批升版本、打 tag，未验证前不 push。
- 服务器 `hw1` 的 `sbot.service` 仍停止；NapCat 未登录（2026-11-05 起 QR 登录失败）。

## 进行中：用户 2026-11-10 的四项要求

决定本身记在 [`memory/tool-surface.md`](memory/tool-surface.md) 与
[`memory/docs-layout.md`](memory/docs-layout.md)。

- [ ] 删除 `Memory` 工具：`/MEMORY.md`、`/USER.md`、`/memory/**` 由文件工具直接读写
      （`src/tools/memory.rs`、fs 工具里对这些路径的拒绝、system prompt 的 Memory 段、
      `[memory]` 里只服务于该工具的键、`memory_entries` 表的去留、doctor 的三方对账）。
- [ ] 删除 `Download` 工具：下载并入 `API`（`local.*` 下的一个 action），子代理工具表与
      `[tools.download]` 的说明同步。
- [ ] 删除 `docs/development/`：仍然成立、且代码里看不出来的内容并入 `.agents/`，其余删除；
      README / AGENTS.md / 上级目录 `CLAUDE.md` 里指向它的说法一并改掉。
- [ ] 面向使用者（人或 agent）的 skill：部署、使用、运维；同一份内容内嵌进二进制，用命令
      查看 / 导出，保证 skill 与二进制版本一致。`docs/guide/` 与 skill 不能各写一份。

## 待用户确认

- [ ] **远端历史**：2026-11-10 已在本地重写历史，去掉 14 个提交里的
      `Co-Authored-By: Claude Code` 行（见 [`memory/commit-messages.md`](memory/commit-messages.md)）。
      这 14 个提交都已在 `origin/main` 上，远端要强推才会生效。两种做法：
      (a) 只替换已发布的部分：`git push --force-with-lease origin refactor/config-sections:main`
      （它就是旧 `origin/main` 重写后的对应提交）加上强推远端已有的 tag（到 alpha.33）；
      (b) 连同未发布的 alpha.34 之后一起推：`git push --force-with-lease origin main --tags --force`。
      旧历史的完整备份在 `G:\sbot-pre-coauthor-rewrite-2026-11-10.bundle`（确认远端无误后可删）。
- [ ] 是否授权 GitHub release、部署 hw1。
- [ ] `G:\sbot-e2e-lists`（alpha.42 实机验证 `model list` 用的临时 SBOT_HOME）没能删除
      （安全检查拦下了 `rm -rf`），需要用户手动删。

## 待核实的正确性风险（2026-11-10 子代理通读长函数所得，逐条未核实）

核实后：是 bug 就修并补测试，不是就从这里删掉。行号是当时的，会漂移。

- [ ] `vfs/sqlite.rs::write_binary_chunks`：旧 blob 目录的删除与暂存目录改名发生在
      `tx.commit()` 之前；提交失败时数据库回滚到旧分块行，磁盘上却已是新 blob。中途出错时
      暂存目录是否清理也要看。
- [ ] `app/mod.rs::start`：IPC 服务的 `JoinHandle` 不在 `select!` 里；`ipc::serve` 失败
      （例如绑定失败）后守护进程继续跑，但没有 IPC，也没有任何报告。
- [ ] `agent/runner/compaction.rs`：压缩先快照 history、放锁、长时间摘要、再整体替换；
      `compact_now` 不拿 `turn_lock`，期间追加的内容可能丢失。`record_checkpoint` /
      `sessions.append` 失败时分段已经轮转而 history 没变。
- [ ] `swarm/mod.rs::spawn`：`max_concurrency` 的检查与插入之间有 await（TOCTOU）；未知的
      `type` 字符串静默退回 `general`，而 `instance.kind` 记成 `custom`。
- [ ] `storage/messages.rs::append`：JSONL 行写完后索引事务失败会返回错误但不留索引行；
      同一条消息重投能通过去重检查，再写一行重复的 JSONL。
- [ ] `tools/views/config.rs::write_config`：`sbot.toml` 的读-改-写没有串行化，并发写会丢
      更新；`notify` 在句柄替换之前调用。
- [ ] `cli/backup.rs`：`output.starts_with(source)` 比较的是没有规范化的路径（相对路径、
      符号链接、Windows 大小写都能绕过“备份放进数据目录”的检查）。
- [ ] `monitor/delivery.rs::deliver_once`：先代入事件文本再代入其他占位符，事件文本里的
      `{{event.chat}}` / `{{count}}` 会被二次展开；等空闲期间 monitor 过期时直接返回，
      没有标记取消，也没有“过期”通知。
- [ ] `agent/loop.rs::request_once`：流中途出错前已经把 text / thinking 分段作为
      `MessageUpdate` 发出，之后 `request_with_retry` 重试，消费者可能收到重复的片段。
- [ ] `agent/runner/injected.rs`：`run_control` 检查 `ai_enabled` 但不检查 ESTOP（`run_event`
      与 `run_turn` 都检查）；`run_injected` 先 push history 再 `sessions.append`，与
      `run_turn` 的顺序相反。
- [ ] `agent/runner.rs::run_turn`：入站循环里的 `sessions.append(...)?` 中途失败时，前面的
      消息已进 transcript 却没进共享 history。
- [ ] `cli/doctor.rs::check_memory`：查询失败被吞掉，看起来像“没有文件”（随 Memory 工具的
      删除一起重看）。

## 仍然超过 100 行的函数（不阻塞）

`app::dispatch` 176、`runner::run_turn` 166、`views::system::read_system` 156、
`loop::run_with_steering` 155、`app::start` 154、`loop::request_once` 141、`swarm::spawn` 131、
`backup::run_with_paths` 127、`runner::run_subagent` 124、`views::config::write_config` 122
（“校验 → 落盘 → 通知 → 换句柄”的收尾重复了 3 次）、`monitor::validate_monitor` 122、
`assembly::build_agent` 120、`messages::append` 118、`events::handle_qq_event` 118、
`delivery::deliver_once` 117、`compaction::compact_scope` 115。其中是“一个 match 平铺 /
同一层的线性步骤”的不必拆；有重复块或混了几件事的（`write_config`、`run_turn`、
`spawn`、`append`）值得拆。

## 外部验证（需 NapCat 登录）

- [ ] 真实 `get_image`、QQ CDN 入站图片下载（alpha.42 起 `http://` 的 CDN 地址升级为
      `https://` 再下载，未在真实 NapCat 上验证）、`get_msg` / `get_forward_msg`。
- [ ] 发出媒体：`base64://` 在真实 NapCat 上可发；私聊回声带 `target_id`。
- [ ] 发送账本：真实 `message_sent` 回声的形状与到达时序（先于/后于响应）。
- [ ] `local.media.image.text` / `.fonts` 只在 Windows 验证过；Linux 服务器无系统字体。

## 发布

- [ ] 发布前重跑 `cargo build --release`。
- [ ] 历史重写后（2026-11-10 又重写了一次）其他 clone 必须重新 clone，或
      `git fetch --all --prune --tags --force && git reset --hard origin/main`。
