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
      长度，采用时把之后追加的消息原样接回去）最适合这里。建议：保留尾巴不变；空闲时在
      软阈值提前压缩，回合边界采用；硬阈值保留同步压缩兜底。实现时注意：现在的
      `compact_scope` 靠回合锁保证“快照到替换之间没人追加”（见它的文档注释），提前压缩
      在回合之外做摘要，这条前提就不成立了，必须改成“记长度、接回后缀”。
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

## 正确性风险

2026-11-10 通读长函数列出的十条已在 2026-11-16 逐条核实：九条是 bug，修了并各补了能在旧代码
上失败的测试（见 CHANGELOG 当时的 Unreleased 段与那天的 `fix(...)` 提交）；“`run_control`
不检查 ESTOP”不是 bug，是文档写明的设计。判断依据都写在对应代码的注释里，这里不重复。

核实时顺带看到的两处（VFS 换一种内容覆盖时留下旧内容；transcript 分段文件不在时追加会
建出没有文件头的文件）也在同一天修了。

## 仍然超过 100 行的函数（不阻塞）

`app::dispatch` 176、`runner::run_turn` 168、`views::system::read_system` 156、
`loop::run_with_steering` 155、`app::start` 154、`loop::request_once` 133、
`backup::run_with_paths` 127、`runner::run_subagent` 124、`monitor::validate_monitor` 122、
`assembly::build_agent` 120、`events::handle_qq_event` 118、
`delivery::deliver_once` 108、`compaction::compact_scope` 115。其中是“一个 match 平铺 /
同一层的线性步骤”的不必拆；混了几件事的（`run_turn`）值得拆。`swarm::spawn`（89）、
`messages::append`（86）与 `write_config`（59）在 2026-11-16 修 bug 时拆过了。

## 外部验证（需 NapCat 登录）

- [ ] 真实 `get_image`、QQ CDN 入站图片下载（alpha.42 起 `http://` 的 CDN 地址升级为
      `https://` 再下载，未在真实 NapCat 上验证）、`get_msg` / `get_forward_msg`。
- [ ] 发出媒体：`base64://` 在真实 NapCat 上可发；私聊回声带 `target_id`。
- [ ] 发送账本：真实 `message_sent` 回声的形状与到达时序（先于/后于响应）。
- [ ] `local.media.image.text` / `.fonts`：Windows 与 CI 的 Linux / macOS runner 上测过；hw1 没装
      系统字体，那里 `.fonts` 为空、`.text` 报错（不内置字体是用户的决定）。
