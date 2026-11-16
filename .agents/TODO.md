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

Dependabot 的 PR：#2–#5 的内容已在 2026-11-16 手动合进 main（本地与 Debian 上完整验证过），
PR 本身没有动，Dependabot 下次运行时会自己关掉；没关的话可以直接关闭。以后的 PR 照此处理：
补丁 / 次版本的合集看 CI 绿了就合；主版本更新先看 API 变化。加密相关的依赖升级后
`storage::crypto` 里“旧版本密文必须解得开”的测试必须通过。

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
      设计稿写好了，在
      [`plan/2026-11-17-channels-and-custom-actions.md`](plan/2026-11-17-channels-and-custom-actions.md)，
      文末列了六件要用户定的事并各给了建议。**等用户确认，确认前不动代码。** 用户同意后从
      步骤 A（属性代替前缀，纯重构）做起。
- [ ] 是否授权部署 hw1。
- [ ] 临时目录要用户手动删（安全检查拦下了 `rm -rf`）：`G:\sbot-e2e-{lists,startwait,startwait-other,fresh,tmp}`
      是 11 月几轮实机验证建的；`G:\sbot-e2e-{live,perchat,phase7,qq}` 更早，用途未核实。旧历史
      的备份 `G:\sbot-pre-coauthor-rewrite-2026-11-10.bundle` 在远端强推之后已经没用。
- [ ] 上级目录的 `G:/ClaudeProjects/ScapegoatBot/CLAUDE.md`（不在本仓库里）仍说开发文档在
      `docs/development/`；那个目录 2026-11-17 已删，这一句要用户自己改（开发文档现在是
      `.agents/`，使用文档是 `sbot docs` / `skills/`）。
- [ ] 2026-11-17 核对 skill 与代码时看到、**没有改**的地方。它们不是 bug，但都是“配置或接口
      说了、程序没做”，要不要做、怎么做由用户定：
  - `[agent].name` 被校验但从不使用（删掉是破坏性的配置变更）。
  - `models.json` 的 `max_retries`、`sampling`、`compat` 被解析但不生效。
  - `compaction_checkpoints` 表只写不读（`checkpoints()` 只有测试在用）。
  - `api.help` / `api.list` 不告诉 agent 哪些动作是 destructive。
  - `sbot doctor` 只数 `.jsonl`，启动时的对账数的是 `messages/` 下全部文件：目录里有别的文件
    时两边的数对不上。
  - 自定义子代理的 `tools` 里写了不认识的名字会被悄悄跳过，不报错。
  - MCP 服务器的 stderr 被丢弃，启动失败时看不到它自己打印的原因。
  - `approve` 的 6 位确认码没有尝试次数限制（靠 60 秒有效期与“只有主人能私聊”）。
  - `SBOT__*` 环境变量覆盖不了嵌套表里的键（`[security.guard]` 这类可以，表数组不行）。

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

2026-11-16 – 17 写 skill 时把文档里的每句话对照代码核了一遍，又查出一批行为与说法不一致的
地方，都修了并补了测试（CHANGELOG 的 Unreleased“修复”段后半部分，提交 `5ee49c0` 到
`4f2200c`）：加密开关切换后旧历史读不出、加密配置下未认证的明文行会被当成历史、环境变量
覆盖只对字符串生效且被 `/config` 写入冲掉、门禁的重复 / 群发规则对字符串形式的消息不生效、
周期检查的窗口丢掉最新内容、`session_start` 注入要等第一次压缩、`message_received` 的
`block` 拦不住回合中途到达的消息、未知事件绕过 `[filter]`、自动压缩停掉后只能重启恢复、
`Grep` 搜历史没有预算。其中两条的取舍值得记住，见
[`memory/storage-invariants.md`](memory/storage-invariants.md) 与
[`memory/security-gate-semantics.md`](memory/security-gate-semantics.md)。

## 仍然超过 100 行的函数（不阻塞）

`app::dispatch` 176、`views::system::read_system` 156、
`loop::run_with_steering` 155、`app::start` 154、`loop::request_once` 133、
`backup::run_with_paths` 127、`runner::run_subagent` 124、`monitor::validate_monitor` 122、
`assembly::build_agent` 120、`events::handle_qq_event` 118、
`delivery::deliver_once` 108、`compaction::compact_scope` 115。其中是“一个 match 平铺 /
同一层的线性步骤”的不必拆。`swarm::spawn`（89）、`messages::append`（86）与 `write_config`
（59）在 2026-11-16 修 bug 时拆过了；`run_turn`（原 168，混着前置检查、入站接收、预算、提示
组装）在 2026-11-17 拆到 85，准备步骤在 `src/agent/runner/prepare.rs`。

## 外部验证（需 NapCat 登录）

- [ ] 真实 `get_image`、QQ CDN 入站图片下载（alpha.42 起 `http://` 的 CDN 地址升级为
      `https://` 再下载，未在真实 NapCat 上验证）、`get_msg` / `get_forward_msg`。
- [ ] 发出媒体：`base64://` 在真实 NapCat 上可发；私聊回声带 `target_id`。
- [ ] 发送账本：真实 `message_sent` 回声的形状与到达时序（先于/后于响应）。
- [ ] `local.media.image.text` / `.fonts`：Windows 与 CI 的 Linux / macOS runner 上测过；hw1 没装
      系统字体，那里 `.fonts` 为空、`.text` 报错（不内置字体是用户的决定）。
