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

`0.2.0-alpha.45` 已发布（2026-11-17 UTC）；测试基线、产物怎么验证的、哪些平台没实测，见
[`memory/release-status.md`](memory/release-status.md)；发版步骤与授权范围见
[`memory/release-workflow.md`](memory/release-workflow.md)。服务器 `hw1` 的 `sbot.service` 仍
停止；NapCat 未登录（2026-11-05 起 QR 登录失败）。

Dependabot 的 PR：#2–#5 的内容在 2026-11-16、#6（`getrandom` 0.4.3）的内容在 2026-11-17
手动做进 main（本地重做同样的升级，锁文件与 PR 的逐字相同，本地与 Debian 上完整验证过）。
PR 本身没有动——合并 / 关闭 PR 没有得到用户的授权；内容进了 main 之后 Dependabot 下次运行
会自己关掉它，没关的话用户可以直接关闭。以后的 PR 照此处理：补丁 / 次版本的升级在本地重做
并验证；主版本更新先看 API 变化。加密相关的依赖升级后
`storage::crypto` 里“旧版本密文必须解得开”的测试必须通过。

## 等用户决定

- [ ] **压缩策略**（用户 2026-11-13 提的问题）：压缩后保留最近几条吗？要不要“无感”——在到
      硬上限之前提前压缩、算好再替换，压缩时 bot 不停顿？六个参考实现的调研结果与 agent 的
      建议在 2026-11-17 的收尾报告里给了用户，**等用户选**。结论要点（重新调研的话看
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
- [ ] “写了不报错、但不起作用”的三样东西，留着还是删掉 / 接上（文档已经如实写明，见
      `skills/sbot-deploy/references/configuration.md`；删掉配置键是破坏性变更，所以由用户定）：
  - `[agent].name`：必须非空，但没有任何地方使用。
  - `models.json` 的 provider `max_retries`（实际重试次数看 `[agent].retry_attempts`）、模型的
    `sampling` 与 `compat`（没有透传给 API）。
  - `compaction_checkpoints` 表只写不读（`SessionStore::checkpoints()` 只有测试在用；新分段的
    首行才是摘要的权威副本）。

同一批里已经处理掉的（2026-11-17，见 CHANGELOG 的 Unreleased）：`api.list` / `api.help` 给出
`destructive` 等门禁属性；自定义子代理 `tools` 里的未知名字改为报错；MCP 服务器的 stderr 进
日志。两条核实后不需要做：确认码不必加尝试次数限制（只有 owner / master 的私聊才会拿去比对，
见 `src/app/events.rs` 的 `approval_reply_sender`，别人没有可以穷举的入口）；`SBOT__*` 够不到
表数组是键名格式决定的，已写进配置文档。

## CI / 发布留下的尾巴

- [ ] PGO 没做，也不打算在没有训练负载的情况下做：仓库只有模拟 NapCat，没有模拟 LLM；用
      单元测试当训练数据会把优化引向测试路径，真实负载可能更慢。要做得先有一个能离线重放的
      代表性回合（模拟 provider + 模拟 NapCat）。已告诉用户。
- [ ] x86_64 的 v4 产物能不能在 CI 上实际运行取决于当次 runner 有没有 AVX-512（alpha.45 有一个、alpha.44 有
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

## 函数长度

2026-11-17 把超过 100 行的非测试函数都拆了，只留一个：`app::assembly::build_agent`（119 行）。
它是二十来个已经各自命名的装配步骤按依赖顺序排成一列，没有分支；再拆只会多出几个带六七个
参数、只被调用一次的搬运函数，读起来更费劲。以后往里加步骤时先加成独立函数再在这里调用。
新写的函数超过 100 行时，先看它是不是混着几件事（混着就拆），是一列线性步骤的可以留。

## 外部验证（需 NapCat 登录）

- [ ] 真实 `get_image`、QQ CDN 入站图片下载（alpha.42 起 `http://` 的 CDN 地址升级为
      `https://` 再下载，未在真实 NapCat 上验证）、`get_msg` / `get_forward_msg`。
- [ ] 发出媒体：`base64://` 在真实 NapCat 上可发；私聊回声带 `target_id`。
- [ ] 发送账本：真实 `message_sent` 回声的形状与到达时序（先于/后于响应）。
- [ ] `local.media.image.text` / `.fonts`：Windows 与 CI 的 Linux / macOS runner 上测过；hw1 没装
      系统字体，那里 `.fonts` 为空、`.text` 报错（不内置字体是用户的决定）。
