# TODO

剩余问题；已完成项删除（git 有记录）。逐条细节、来源与核实状态见
[`plan/2026-11-07-review-remediation.md`](plan/2026-11-07-review-remediation.md)。
用户 2026-11-08 目标：完成本文件全部内容，并审查、测试、检查合理性（无上帝结构、无冗余、
代码/注释/文档一致）与安全性。

## 构建规则

- 子代理禁止运行任何 `cargo` 命令；构建只在主 checkout 用默认 `target/` 统一执行。
- 不使用 C: 盘放构建产物；G: 空间不足时先清 worktree target / incremental，再报告。
- 本机 cargo 访问 crates.io 的 TLS/schannel 失败；用 `target/scratch/cargo-rs.sh`（做法与
  工具位置见 `memory/cargo-network.local.md`），不要把镜像配置提交进仓库。
- Bash 工具会吞掉一层反斜杠（heredoc 里 sed 的反向引用会变成控制字符）；含 `\` 的编辑用 Edit 工具
  （见 `memory/shell-editing.local.md`）。

## 当前状态

`0.2.0-alpha.44` 已发布（2026-11-15 UTC）；测试基线、产物怎么验证的、哪些平台没实测，见
[`memory/release-status.md`](memory/release-status.md)；发版步骤与授权范围见
[`memory/release-workflow.md`](memory/release-workflow.md)。服务器 `hw1` 的 `sbot.service` 仍
停止；NapCat 未登录（2026-11-05 起 QR 登录失败）。

Dependabot 开了 4 个 PR（#1 兼容更新合集；#2 getrandom 0.4、#3 chacha20poly1305 0.11、
#4 windows-sys 0.61 是主版本更新）。**都没有处理**：#1 的一部分已经随安全升级手动做了
（anyhow、thiserror），它会自己 rebase；#2–#4 要看 API 变化与 CI 结果再合。

## 等用户决定

- [ ] **压缩策略**（用户 2026-11-13 提的问题）：压缩后保留最近几条吗？要不要“无感”——在到
      硬上限之前提前压缩、算好再替换，压缩时 bot 不停顿？六个参考实现的调研结果与 agent 的
      建议已经在 2026-11-16 的回复里给了用户，**等用户选**。结论要点（重新调研的话看
      `.ref/` 下的 grok-build、codex、hermes）：现在的实现已经保留最近约 2 万 token 的原文
      （pi / openclaw 同款）；“无感”的三家（grok-build 默认开、codex 与 hermes 默认关）都不是
      后台算完直接换，而是提前算好一部分、到阈值时仍有一步同步操作；codex 的做法（快照时记
      长度，采用时把之后追加的消息原样接回去）最适合这里，顺带能修掉下面“正确性风险”里压缩
      丢消息的那一条。建议：保留尾巴不变；空闲时在软阈值提前压缩，回合边界采用；硬阈值保留
      同步压缩兜底。
- [ ] **命名与可扩展性**（`local.*` → `native.*`、自定义 action、任意渠道、可禁用 NapCat）：
      方向记在 [`memory/tool-surface.md`](memory/tool-surface.md)。下一步是写设计稿
      （`plan/`）给用户确认，确认前不动代码。
- [ ] 是否授权部署 hw1。
- [ ] 临时目录要用户手动删（安全检查拦下了 `rm -rf`）：`G:\sbot-e2e-{lists,startwait,startwait-other,fresh,tmp}`
      是 11 月几轮实机验证建的；`G:\sbot-e2e-{live,perchat,phase7,qq}` 更早，用途未核实。旧历史
      的备份 `G:\sbot-pre-coauthor-rewrite-2026-11-10.bundle` 在远端强推之后已经没用。
- [ ] 上级目录的 `G:/ClaudeProjects/ScapegoatBot/CLAUDE.md`（不在本仓库里）仍说开发文档在
      `docs/development/`；删掉那个目录时要用户一起改。

## 用户 2026-11-10 的要求里还没做的

决定本身记在 [`memory/docs-layout.md`](memory/docs-layout.md)。

- [ ] 面向使用者（人或 agent）的 skill：部署、使用、运维；同一份内容内嵌进二进制，用命令
      查看 / 导出（`sbot docs`），保证 skill 与二进制版本一致。`docs/guide/` 与 skill 不能各写
      一份：做完后删 `docs/guide/`。计划见
      [`plan/2026-11-11-embedded-docs-skills.md`](plan/2026-11-11-embedded-docs-skills.md)。
      现状：只有一个未提交的草稿 `skills/sbot/SKILL.md`，`sbot docs` 命令不存在。做完后要在
      `.github/scripts/release-notes.sh` 里加一句“`sbot docs` 看文档”，部署 skill 里写
      “要配自动重启”（原因在 `Cargo.toml` 的 `[profile.release]` 注释与
      `docs/guide/limits.md`）与发布页怎么选文件（`docs/guide/install.md`）。
- [ ] 删除 `docs/development/`：仍然成立、且代码里看不出来的内容并入 `.agents/`，其余删除；
      README / AGENTS.md 里指向它的说法一并改掉。

## CI / 发布留下的尾巴

- [ ] PGO 没做，也不打算在没有训练负载的情况下做：仓库只有模拟 NapCat，没有模拟 LLM；用
      单元测试当训练数据会把优化引向测试路径，真实负载可能更慢。要做得先有一个能离线重放的
      代表性回合（模拟 provider + 模拟 NapCat）。已告诉用户。
- [ ] x86_64 的 v4 产物能不能在 CI 上实际运行取决于当次 runner 有没有 AVX-512（alpha.44 有
      三个只做了链接检查）。想要每次都跑：用 Intel SDE 模拟，或固定到有 AVX-512 的 runner。
- [ ] CodeQL 的两条误报已在 GitHub 上标记并写了理由（#1 HKDF 输出缓冲区被当成硬编码密钥，
      #6 `fetch_image` 的 http→https 升级）；`vendor/napcat-link/examples` 的 4 条在
      `codeql.yml` 里排除了路径，下次扫描后应自动关闭——没关的话去 Security 页看。

## 待核实的正确性风险（2026-11-10 子代理通读长函数所得，逐条未核实）

核实后：是 bug 就修并补测试，不是就从这里删掉。行号是当时的，会漂移。

- [ ] `vfs/sqlite.rs::write_binary_chunks`：旧 blob 目录的删除与暂存目录改名发生在
      `tx.commit()` 之前；提交失败时数据库回滚到旧分块行，磁盘上却已是新 blob。中途出错时
      暂存目录是否清理也要看。
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
- [ ] `local.media.image.text` / `.fonts`：Windows 与 CI 的 Linux / macOS runner 上测过；hw1 没装
      系统字体，那里 `.fonts` 为空、`.text` 报错（不内置字体是用户的决定）。
