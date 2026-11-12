# Changelog

本文件记录 ScapegoatBot 的所有重要变更。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### 破坏性变更

- `Download` 工具删除，下载是 `API` 的 `local.net.download`（`url`、`path`、可选的
  `max_bytes`）。网络策略与流式加密落盘不变。
- `Memory` 工具删除。`/USER.md`、`/MEMORY.md`、`/memory/<slug>.md` 是普通的 VFS 文件，agent 用
  `Read` / `Write` / `Edit` 读写；格式是系统提示词里写明的约定，运行时不校验。运行时自己只剩
  后台复盘会写记忆，而且只追加（一个新文件加一行索引），不改写已有的索引与文件。迁移 13 删除
  只有该工具写过的 `memory_entries` 表（schema v13）。`[memory]` 的配置键照旧生效。
- 子代理拿到的 `API` 只有 `local.*` 与 `api.*`，任何 `napcat.*`（包括查询）都被拒绝；自定义
  子代理定义里写了 `allow_send` 才是完整的 `API`。`agent.*` 对所有子代理拒绝：上下文、压缩与
  hold 属于主 agent。
- 守护进程启动时一直等到 NapCat 连上并且 QQ 已登录，然后才建 agent。以前 NapCat 当时没起来
  的话，守护进程以“账号未知”的状态跑下去，之后再也不会去连，要有人重启才恢复。等待期间控制
  通道可用（`sbot status` / `stop` / `model set` 等）；`sbot chat` 这类需要 agent 的命令返回
  `not_ready` 并说明原因，输入不落库（以前会存下一条不会被执行的输入）。

### 新增

- `sbot status` 多了 `napcat.connected`（WebSocket 此刻是否连着）与 `agent.state`
  （`waiting_for_napcat` / `starting` / `running` / `no_provider`）。

### 修复

- NapCat 断线后一直重连。SDK 自己的重连在三种情况下会停手：重试 10 次（约 7 分钟）之后、
  服务端正常关闭连接（NapCat 重启时常见）、连上后连续 3 次立刻被断开。以前停手之后没有任何
  东西再去连，守护进程就此失联到被人重启为止；现在由运行时接着连，5 秒起指数退避，最长每
  分钟一次。
- QQ 动作不再被自动重发。SDK 默认把超时与失败响应各重发最多 3 次；发送类动作超时并不代表
  没发出去，重发就是重复的 QQ 消息（实测一条被 NapCat 拒绝的发送被发了 4 次）。
- 没连上 NapCat 时调用立即失败，并明确说没有发出去。以前请求被悄悄丢掉，调用方一直等到
  超时，发送账本里留下一条“结果不明”；现在这种发送记为失败，不提示去核对聊天记录。
- 控制通道建不起来（套接字目录不安全、绑定失败、管道被占用）时守护进程报错退出。以前要到
  退出时才发现，期间进程照常运行，但既查不了状态也停不下来。一次 accept / connect 失败不再
  让控制通道的接受循环结束。
- `sbot model set` / `sbot effort set` 在 agent 启动的那一小段时间里执行时不再丢：agent 就位后
  按最新的配置再切一遍。
- 发给另一个实例的控制命令被拒绝（`wrong_instance`）。控制通道的端点名只由 `[ipc].name`
  决定，两个 `SBOT_HOME` 用了同一个名字时，`SBOT_HOME=乙 sbot stop`（以及 `chat`、
  `gate approve`）会被正在运行的甲执行；现在 CLI 在请求里带上实例目录，守护进程对不上就拒绝，
  并提示给每个实例单独的 `[ipc].name`。
- `sbot --config <路径> restart` 拉起的新进程带上同一个 `--config`。以前新进程读的是默认位置的
  配置（没有就新建一份默认的），重启悄悄变成了换配置启动。
- Unix 下 `sbot` 在 `$XDG_RUNTIME_DIR` 里连不上守护进程时，再试实例目录下的 `run/<name>.sock`。
  守护进程由服务管理器拉起时环境里常常没有 `XDG_RUNTIME_DIR`，套接字就在 `~/.sbot/run/`；登录
  shell 里有这个变量，CLI 以前只去那里找，`sbot status` 便一直报守护进程没在运行。路过的
  端点如果属于另一个实例（`wrong_instance`），同样跳过、接着找。
- 模型调用被 `sbot ai stop` 停掉时，`sbot chat` 返回 `ai_stopped`，不再回答“已接受”然后在后台
  失败。
- 后台复盘写记忆时的三个问题：`/MEMORY.md` 读不出来时被当成空索引，随后被新条目整体替换；
  索引每次被重新渲染，空行与手写的结构丢失；复盘在每次写之前伪造一次读取，agent 因而可以
  覆盖一份它在复盘改动之后没有再读过的索引。
- `sbot doctor` 的记忆对账按索引与 `/memory` 下的文件核对；列目录失败时如实报告，不再当成
  “没有文件”。
- 消息索引重建：一行解不开（损坏、篡改、换了密钥）或一个文件读不出时只记警告并跳过，
  其余消息照常补回。以前整个重建直接失败，后续文件补不上索引，下次启动再次触发、再次失败。

### 变更

- 拆分过长的函数：`app::assembly::build_agent`（208 → 120 行，`runtime_ops` /
  `build_views` / `build_runner` / `spawn_maintenance`）、`assemble_tools`（返回具名的
  `ToolSet`；`register_mcp` / `build_action_gate` / `register_api`）、`register_web_tools`
  （search / fetch 共用 `tool_backend`）、Anthropic 流解码的 `handle`（按事件类型拆成方法，
  `Stop` 只发一次的逻辑合一）、`MessageStore::rebuild_index`（文件头解析、逐行补索引、
  文件表同步分开；文件头的加密方式解析与读取路径共用 `header_encryption`）。
- OpenAI Responses 流解码的 `handle` 拆出 `item_added` 与 `responses_usage`；两个协议的
  解码器共用 `providers::str_field`。

## [0.2.0-alpha.43] - 2026-11-10

### 修复

- 开了 `allow_send` 的自定义子代理经 `API` 的 `local.media.image.*` 输出图片时，同样不能写
  `/memory`、`/config` 等受保护路径（alpha.42 的子代理写保护漏了这条路径）。

## [0.2.0-alpha.42] - 2026-11-10

配置整改：每个配置键要么真的生效，要么删除并在启动时给出迁移提示；工具描述、文档与代码
对齐；子代理边界落到代码里；结构拆分与去重。

### 破坏性变更

- 已删除的配置键在启动时报错并提示删除：`[memory].topics`、`[[hooks.events]].scope`、
  `[[hooks.events]].match.tool_name`（都从未改变过行为）。
- `[[hooks.events]]` 的事件 / 动作组合按文档的矩阵校验，不在矩阵里的组合启动报错。
- `source = { qq = false }` 报错（以前被当成订阅 QQ 事件）；关掉 monitor 用 `enabled = false`。
- `Monitor add` 的参数按 `[[monitor]]` 同一套 schema 严格解析：未知字段、`lifecycle.catch_up`、
  `qq.backlog`、`stream` 源都报错；`internal` 源写成 `{"internal": {"kind": "security"}}`。
- `Agent spawn` 不再接受（文档里从未出现的）`monitor` 参数；给子代理投递事件用
  `Monitor add` 的 `deliver.to = "<子代理 id>"`。
- `sbot ai restart` 删除（与 `ai start` 完全相同）。
- `agent.compact` 不再接受 `chat`（只有一条 transcript，参数从未起作用）；`API` 对所有
  命名空间（`agent.*`、`local.*` 也包括在内）拒绝未声明的参数。
- 子代理不能写 `/config`、`/MEMORY.md`、`/memory`、`/USER.md`、`/AGENTS.md`、`/agents`、
  `/hooks`，不能删除任何东西；`general` 子代理没有 `RM`，自定义子代理不能用 `Memory`。
- 写入、编辑、复制、建目录都不能以 `/trash` 为目标（只有 `RM` / `MV` 能把东西放进去）。
- 最低 Rust 版本是 1.89（`Cargo.toml` 的 `rust-version`；依赖 `nalgebra` / `uuid` 要求）。

### 新增

- `[memory].review_model`：后台复盘与压缩前的记忆提取用独立模型（计量计入预算）；不设时
  跟随运行中的主模型（以前热切换后记忆提取仍用启动时的模型）。
- `[[monitor]]` 定时器的 `jitter_secs`：每次触发随机推迟 `0..=n` 秒（最多 3600，`every` 时
  短于周期）。
- `internal` 源真的会收到事件：`security:hold` / `security:released`、`budget:exhausted`、
  `agent:done` / `agent:failed`、`storage:quota` / `storage:disk`。
- 配置文件里的 `stream` 源（WebSocket / SSE）会连接：断线指数退避重连，断开与恢复各一条
  系统通知。
- `[queue].mode`：`followup` 回合结束后逐条各开一个回合；`collect` 再等一个 `debounce_ms`
  合并成一个回合（以前两者与 `steer` 的后续批次相同）。
- `deliver.as = "chat"`：把 QQ 消息交给会话队列按普通会话回合处理；内置唤醒预设就是这种
  形式，`Monitor list` 显示来源（`config` / `runtime` / `preset`）。
- `sbot chat --scope group:<gid>|private:<qq>`：写进 `owner_context` 的 `scope` 属性；格式
  错误时拒绝（以前只回显、不生效）。
- `sbot model list` / `sbot effort list` 列出白名单模型（含 models.json 里没有的）与当前模型
  支持的级别（以前打印整份 status）。

### 修复

- `[[hooks.events]]`：`inject_context` 的 `path` 会被读取；`message_received` 的注入进入本回合；
  带 `match` 的 hook 只在有 chat 的事件上命中；`notify` 只在 `notify_channel = owner|master`
  时私聊；`session_end` 在退出时触发（最多等 5 秒）。
- `max_session_bytes` / `max_message_bytes` 在没有保留期时也会执行（默认 2 GiB 的
  transcript 上限以前从不生效）。
- `[budget].chat_sends_per_hour` 在 `trusted` 门禁下同样生效，纯图片 / 文件的发送也计数。
- 入站图片：下载的图片也计入每批 4 张的上限；格式按内容识别，认不出的数据不再当成 JPEG
  发给模型；过大或不支持的格式缩成 JPEG（与 `Read` 一致）；只从 QQ CDN 的 HTTPS 地址下载
  （`http` 升级）。
- 回合、注入回合与子代理的循环配置取自同一个 provider 快照：热切换模型时 provider 与
  模型 id 不会错配。
- 周期安全审查记为 `review`（以前混在 `input` 里）。
- `turn completed` 是 info 级（排障文档让用户找的就是这一行）。

### 删除

- 零引用的函数与类型：`QqClient::group_list` / `friend_list`、`Segment::as_text`、
  `format::render_batch` / `segment_to_napcat` / `scope_name`、`events::count_blocked`、
  `turn_buffer::load_context`、`fs::read_static`、`vfs::dynamic::DynamicProvider`、
  `tools::Permission`、`QqMessage::backfilled`（数据库列保留，恒为 0）、
  `Runner::turn_lock_for` / `session_for` / `history_for`（忽略参数）、`~/.sbot/logs/`（从无写入者）。
- `workspace/*.md`（0.1 模板，无代码读取）移到 `docs/archive/0.1/workspace/`。

### 变更

- `app/assembly.rs` 拆成 `assembly/{stores,providers,tools,qq}.rs`；`monitor/mod.rs` 拆出
  `predicate`、`spec`、`delivery`；`agent::loop::run_with_steering`、`sbot doctor` 的检查拆成
  具名步骤。
- 去重：模型清单加载（`ModelsFile::embedded` / `with_overlay`）、存储行的封装
  （`envelope::seal_as` / `open_as`）、VFS 用量查询、`edit_text`、`CompletionError` 的错误文字。
- `tests/live_napcat.rs` 改名 `tests/onebot_mock_client.rs`（它对本地模拟服务端运行）；
  README 的最小配置测试直接读 README 与入门指南里的代码块；`config.example.toml` 写出的值
  必须等于代码默认值（有测试）。
- 文档：MSRV、README 命令与目录树、备份包含 `auth.json`、`require_mention` 的语义、
  `/config` 热更新范围、`Mkdir` 的 `parents`、工具描述（`MV` / `CP` / `Stat`、子代理边界、
  Monitor 参数）。

## [0.2.0-alpha.41] - 2026-11-09

审查 P1 的其余部分：预算口径、定时器、provider 停止原因与 Anthropic 回放、动态视图查询、
资源放大。

### 修复

- 预算按每次模型请求各自的价格计量：`providers::metered` 包住运行时的每个 provider，
  子代理、压缩、记忆 flush、安全审查与 LLM 门禁都计入，缓存 token 按缓存价计费。
  `Usage` 在三种协议里口径一致：`input_tokens` 不含缓存输入（OpenAI / Chat 的计数拆开，
  DeepSeek 的 `prompt_cache_hit_tokens` 生效）；`CostDef` 新增可选的 `cache_read` /
  `cache_write` 价格，缺省按输入价。
- 每轮 token 上限统计本轮新增的 token（输出 + prompt 增长），不再把每次重发的完整 prompt
  相加；每日预算检查在自动压缩之前进行，保留下来的消息清掉压缩前的用量，预算耗尽后不再
  每次唤醒都压缩一次。
- 热切换模型同时更新上下文窗口、图片能力与价格（放进 provider slot）。
- 定时器：配置校验、配置加载、Monitor 工具与 Agent 绑定共用一个解析器
  （`monitor::timer::TimerSpec`），`at` 校验范围，`0 0 30 2 *` 这类永不触发的 cron 直接拒绝；
  `at` 每天触发而不是只触发一次；cron 的日 / 周字段同时受限时按标准的“或”语义，逐日计算
  下次运行；夏令时缺口顺延到下一个有效分钟、重叠只触发一次；上次触发时间只在真正触发时
  写入，`every` 以它为锚，补投用 `next_after` 计数；休眠每次最多 60 秒并复核时钟。
- 认不出的停止原因按失败处理并记日志，不再当成正常结束去执行可能被截断的工具调用；
  覆盖 Anthropic SDK 的全部 `StopReason`（含 `model_context_window_exceeded`）与 DeepSeek 的
  `insufficient_system_resource` / `aborted`。Responses 的 `incomplete` 若因 `content_filter`
  是拒绝，截断的响应也报告用量；408 / 504 与 5xx 一样重试。
- Anthropic：服务端工具（`server_tool_use`）的参数按 `input_json_delta` 拼好、在块结束时
  发出，不再漏进普通工具调用的参数、回放成空输入；回放时跳过空白文本块和没有签名的
  thinking 块（以及因此变空的消息），这些会被 API 以 400 拒绝；agent 循环在最后一个块上打
  缓存断点，工具循环的每一步复用上一步的前缀，一次性请求不付写缓存的钱
  （`LlmRequest.cache_history`）。
- 动态视图查询：
  - `filter` 在解析查询串时就解析成结构化条件（`vfs::filter`）：未知条件、不是数字的
    `from:`、不存在的日期、没闭合的引号直接报错，不再悄悄匹配不到任何东西；`contains:`
    支持引号、不区分大小写。
  - `mentions_self` 指 @ 本账号或 @全体成员，不再是“任何数字 @”。
  - `/inbox`、`/messages` 与日文件统一按 `filter` → `order` → `offset` → `limit` 截取，
    并说明还剩多少没给出；以前 `offset` 与 `order` 被忽略。`/messages` 带 `filter` 时按条件数
    各 chat 的未读；日文件可以按 `state:` 筛选。
  - 单文档视图带查询串、动态视图上的按行 `offset` / `limit` 都报错；`LS` 的 `filter` 只认
    `contains:` 并分页；`RM` 带查询串时只忽略这一页并报告剩余。
  - `Stat` 对动态文件与目录都能用（以前把路径本身当目录列举，几乎总是失败）；`Grep` 扫描
    整个文件，不再只搜每天的前 50 条。
  - `Read` 工具描述里的示例改成真实存在的语法。
- 资源放大：
  - `Download` 边收边加密暂存（`vfs::sqlite::BlobStage`），内存里最多约 1 MiB，暂存与提交
    在阻塞线程里做；失败或超限不留节点和暂存目录，崩溃遗留的暂存目录在启动时清理。
  - 文本类下载（`text/*`、JSON、XML、JS）在文本上限内且是合法 UTF-8 时存成文本节点；
    以前它们按二进制分块存成 kind 为 text 的节点，读不出来。`write_binary` 拒绝文本类型。
  - `CP` 逐块复制二进制节点，不再把整个文件读进内存。
  - 图片解码统一在 `src/media.rs`：64 MB 与单边 8192px 上限（以前 `Read` 看图与 MCP 截图
    不限宽高），过大的图逐级缩小到放得下视觉上限为止；`Read`、`local.media.image.*`、MCP
    截图的解码与编码都移到阻塞线程池。
  - `WebFetch` 的 HTML → 文本改为单次线性扫描（正则 `replace_all` 最坏 O(m·n²)），实体只
    解码一次（`&amp;lt;` 不再变成 `<`），支持数字实体；截断标记只在真的截断时出现；
    搜索 / 抓取后端的响应体限 16 MiB。

### 变更

- `MessageStore::inbox_for_chat` 删除（与 `unread(Some(chat))` 相同）。
- `ViewQuery.filter` 的类型从 `String` 变成 `ViewFilter`；新增 `PathError::BadFilter` /
  `QueryNotSupported`。`Views::peek` 改名 `scan`（完整、不分页、不标记已读）。

## [0.2.0-alpha.40] - 2026-11-08

安全审查 P1。

### 安全

- 只带 `message_id` 的动作（撤回、表情回应）按消息归档反查所属 chat，再按 `send_scopes`
  判断；归档里查不到的 id 拒绝。以前可以拿任意 id 撤回别处的消息。
- 踢人、禁言、改名片等群管理只在触发本回合的群里免 owner intent；在 A 群的回合里管理 B 群
  需要 owner intent。
- 群公告与上传文件纳入 `[gate]` 发送规则（重复、群发、搬运、媒体来源、每 chat 上限）、
  BeforeSend hook 与内部标签剥离：三处共用 `actions::published_view` / `published_text_field`。
  `hooks::before_send` 多了正文字段参数，`rewrite` 改写消息文本段与公告正文，上传只受
  `block` / `log` 影响。
- owner intent 不能被别人借用：批次里的非 `self` 消息**全部**来自主人才算；回合中途经
  steering 进来别人的消息时立即撤销（不可恢复）。steering 通道改为直接传 `QqMessage`，由
  runner 渲染并更新证据；`TurnEvidence` 的 intent 变成克隆间共享、只能撤销的标志
  （`owner_intent()` / `revoke_owner_intent()`），子代理与工具调用都能看到撤销。
- hold 期间 `/config` 与 `/config.toml` 只读：上下文可能已被注入，改配置会把一次注入变成
  持久设置。
- `/config/**` 与 `/config.toml` 把 `[[mcp.servers]].env` 的值显示成 `<redacted>`；整文件写回时
  原样带回的占位符恢复成原值。`persist()` 的临时文件只给属主读写，rename 前复制原文件权限，
  不会放宽 `0600`。
- Unix 控制套接字不再回退到共享临时目录：没有 `XDG_RUNTIME_DIR` 时放在 `~/.sbot/run`，
  bind 前把所在目录收紧到 0700，收紧不了就拒绝启动。
- 日志写出前统一遮蔽 `access_token=…` 与 `Bearer …`（`src/logging.rs`）：napcat-link 在
  INFO 级别打印带 token 的 WebSocket 地址。
- `auth.json` 解析失败只报错误类别与行列，不再引用 serde 的原始信息（会带出密钥）。
- 准入过滤的 `ignore_groups` / `ignore_users` 也作用于通知与请求：被忽略的群里的进群、撤回、
  戳一戳，被忽略的人发来的好友/加群请求不入库、不进 inbox、不进 monitor 与 `/system/events`。
  主人的消息不豁免（与 0.2.0-alpha.38 的决定一致）：排除谁是 agent 自己的选择。

### 修复

- VFS 回收站有了上限并真正被清理：移入时在同一事务里执行 30 天保留期、字节 ≤ 总量 10%、
  节点 ≤ 节点上限 10%，单个条目自己就超限时如实报告为永久删除；从回收站恢复按活跃配额检查；
  每天一次的后台清理移除过期条目（`app::maintenance`，技能 curator 也搬到这里）。
- 删除节点时先提交事务、再删磁盘上的 blob 目录；以前事务回滚会留下指向不存在内容的节点。

### 移除

- `[skills].shared_dir`：技能一直在 VFS 的 `/skills`，这个宿主路径只被 `sbot backup` 读取，
  模型却能通过 `/config/skills` 改写它，让下一次备份打包任意目录。旧配置启动时报迁移提示。

### 其他

- 源码卫生扫描收紧：普通字符串里的任何裸换行都报错（SQL 除外），剩余 20 处 shell 编辑留下的
  痕迹改写为 `\n` 转义，逐个核对值不变。另修 `app/events.rs` 子代理 monitor 提示里的一处。
- `config.example.toml` 的 `curator_min_new_skills` 与代码默认值一致（3）。

## [0.2.0-alpha.39] - 2026-11-08

运行时正确性（审查 P0）。

### 修复

- 回合块实时写入 turn buffer：模型循环每并入一块就发 `AgentEvent::Appended`，runner 立刻
  落盘（`agent/runner/journal.rs`）。以前只在回合结束前一次性写入，进程中途退出会丢掉整个
  回合，包括已经执行过的工具调用。
- 消息日文件的行号改为文件里的物理位置（带长度校验的缓存，末尾残行先补换行）。以前按索引
  条数计算，文件写成功、索引事务丢失一次，之后同一天的行号全部错位，重建也修不好。
- `/friends`、`/groups` 视图重写：按文档的 `/friends/<qq>.md`、`/groups/<gid>.md`、
  `/groups/<gid>/<qq>.md` 路由；以前对已解包的结果再取 `data`，列表永远为空，读文件的路由也对不上。
  视图改用 `NapcatGateway`，有了离线单测。
- monitor 的 `when` 在创建时（含配置文件）逐项校验，写错直接报错；以前短路求值让错项潜伏，
  一旦触发就每个 QQ 事件投递一条系统通知（每条一个回合）。运行时若仍出错，移除该 monitor 并只通知一次。
  `backlog = "inbox"` 补投的 `@` 判定与实时事件一致（以前任意 `@` 都算 `mentions_self`）。
- 子代理：`sbot ai stop` 与 ESTOP 也停下子代理；`Agent message` 用调用方这一回合的证据续聊，
  不再沿用 spawn 时的 owner intent；spawn 带 owner intent 的运行中实例不接受没有 owner intent 的引导；
  运行收尾时送来的消息等收尾完成再续聊（以前立刻重启，上一轮的最终回答被丢弃），收件通道关闭前
  送进来的消息会再跑一轮回应。
- `agent.quarantine` 在重启后仍然生效：恢复上下文时按 `/quarantine/` 重放替换；空 `text` 被拒绝。

## [0.2.0-alpha.38] - 2026-11-08

主人的消息不再走特权通道。

### 变更

- 撤回 0.2.0-alpha.37 的“主人之声”与 `[agent].master = "owner"`：`src/principals.rs`、
  `event.from_owner`、主人消息免过滤、doctor 的私线警告全部删除，`master` 恢复为
  `"self"` 或 QQ 号（默认 `"self"`），只是报告与审批通知的目标。
- 新增发送账本（`src/qq/ledger.rs`）：本进程每次发消息前登记 chat 与内容指纹、返回后补记
  `message_id`，`message_sent` 回声据此认领。认领到的是 agent 自己的话（`self`）；认领不到的
  是有人在本账号上手打的——stand_in 下就是主人——它们和别人的消息一样进 inbox，发送者记为
  本账号 QQ（即 `owner_qq`），由默认的 `wake-on-owner` monitor 唤醒，带 owner intent。
  monitor 是 agent 可以改写或删除的普通 monitor；不看主人消息也是 agent 自己的选择。
  误判只偏向“是 agent 发的”：同一 chat 有发送在途时等它返回（最多 10 秒），调用失败或超时、
  无法预知回声内容的发送匹配该 chat 的下一条回声。需要 NapCat 打开 `reportSelfMessage`。
- 唤醒预设的提示词修正：原来缺少行尾 `\`，模型看到的是一段断开的缩进文本。

### 修复

- `ActionGate` 的群发拒绝信息里多了一个字面反斜杠和断开的缩进。
- `tests/source_hygiene.rs` 新增跨行扫描：普通字符串里没有 `\` 的换行后接缩进即报错
  （SQL 除外；CRLF checkout 先归一化）。原来的单行正则看不到开引号在上一行的字面量。

## [0.2.0-alpha.37] - 2026-11-08

自主运行第一批：主人只能通过 QQ 交互、hold 必须由 agent 自己解除，以及 v5 系统提示词。

### 破坏性变更

- 移除 `[[hooks.context]]`。`/AGENTS.md`（存在即注入，主人和 agent 都可以写）、`/USER.md`、
  `/MEMORY.md`、风格档案与技能索引由程序固定注入；其余文件（例如人设 `/SOUL.md`）由 agent
  自己列在 VFS 的 `/hooks/context.toml`。旧配置里的 `[[hooks.context]]` 会让启动失败并给出迁移提示。
- `[agent].master` 默认值从 `"self"` 改为 `"owner"`（= `owner_qq`），并新增 `"owner"` 取值。
  stand_in 的默认结果不变（仍是登录账号）；companion 的报告与审批通知现在默认发给主人。
- 安全周期检查的 `critical` 不再“暂停所有新回合”（只有宿主机控制通道能解开，无人值守时等于
  永久停机）：改为 hold，并在下一回合开始前丢弃当前上下文（换到新分段，旧分段留档）。
  `agent_paused` 状态随之删除。

### 新增

- 主人之声：`src/principals.rs` 统一判定“这条 QQ 消息是不是主人在对 agent 说话”——`owner_qq`
  的非 self 消息，或 `master_qq`（另一个账号）的私聊。此前 stand_in 下主人的消息全是 `self`，
  主人没有任何 QQ 渠道；现在把 `[agent].master` 设成主人的另一个账号即可。命中的消息唤醒 agent、
  带 owner intent，并且不受 `[filter]` 拦截。`sbot doctor` 对“主人没有 QQ 渠道”给出警告。
- monitor 谓词新增 `event.from_owner` 与前缀 `not`；内置唤醒预设改用它们。
- 技能索引真正进入上下文（此前模板里有占位符，但从未填充）。
- `agent.quarantine` 把原文另存 `/quarantine/` 的同时，把上下文里的同一段文字换成占位符，
  返回替换次数；`agent.status` 返回 `hold_reason`。

### 变更

- 系统提示词重写为 v5（`assets/system_prompt_v5.txt`），参考 Claude Code 的写法：讲理由、用比喻
  （白板、照片、断路器、闹钟、新同事），并直接写入发消息的 API、消息段与示例、门禁与 hold 的
  自恢复流程。去掉了与代码不符的说法（“不能写 /AGENTS.md”“/AGENTS.md 或 /SOUL.md 里的常驻
  指令算 owner intent”“下一条消息一定会唤醒你”等）。v4 模板与 `__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__`
  机制删除。
- `agent.resume` 复核**当前已提交上下文**的尾部，不再复核当初被判危险的原文（那段原文永远危险，
  输出闸触发的 hold 因此永远解不开）。被判危险的原文不再留在内存里。
- `Agent` 工具描述改为“刚走进房间、什么都不知道的新同事”的交接写法，并如实说明只有
  `allow_send: true` 的自定义定义能发送。
- `agent.quarantine` 删除从未生效的 `message_id` 参数；`agent.status` 的摘要与实际返回一致。
- 图片工具改名为 `local.media.image.*`（`info`、`transform`、`convert`、`text`、`fonts`），
  归属“本地工具 → 媒体 → 图片”；旧名 `image.*` 不再可用。`api.list` 的 `namespace`
  改为层级前缀匹配（`local`、`local.media`、`local.media.image`）。

## [0.2.0-alpha.36] - 2026-11-07

审查整改第一批：安全与数据正确性。

### 安全

- 发往 NapCat 的文件引用一律内联为 `base64://`：VFS 路径由 sbot 读出，公网 URL 按
  Download 网络策略下载；`file://`、宿主机路径、`data:` 拒绝。此前模型写的字符串
  原样交给 NapCat，而 NapCat 会读取宿主机上存在的任意路径（可把 `~/.sbot/auth.json`
  发进群里），也会自己去请求内网 URL。
- `napcat.*` 只接受注册表声明过的参数；未声明字段（例如 NapCat 会当文件读取的
  `send_group_notice.image`）直接报错。
- `send_scopes` 覆盖所有对外动作（群管理、上传、改资料），并在门禁之前检查：
  越界请求不再生成待确认项、不再私聊主人。
- `napcat.send_msg` 要求恰好一个目标；`group_id` 与 `user_id` 同时出现时拒绝
  （NapCat 在私聊对象查不到时会改发到群里，原先的作用域判断会被绕过）。
- `approve` 的私聊确认码只接受 owner/master 从**另一个账号**发来的回复；本账号发出的
  消息与 agent 自己发的无法区分，不再可能用于批准。`master_qq` 就是本账号时通知里
  不带确认码。确认码回复不再写入消息归档。
- 网络工具的 HTTP 客户端禁用系统代理：走 `HTTP(S)_PROXY` 时目标由代理解析，
  连接层的公网过滤会失效。阻止列表现在也匹配末尾带点的域名。
- 图片解码限制宽高 ≤ 8192（解压炸弹）；任意角度旋转先算画布，超限拒绝而不是分配；
  `image.text` 最多 1000 字符。
- 判定器/审查器/复盘解析模型 JSON 时，`"} {"` 一类输出不再使区间倒置 panic。

### 修复

- 回合运行中 steer 进来的消息不再“收下却不回应”：模型循环一结束就关闭 steering，
  之后到达的消息留在队列；被送进回合却没得到回应的消息（提前结束、回合被跳过、
  收尾窗口里到达）按原顺序放回队首并再开一轮。此前它们被追加在最终回复之后
  （或只写进 transcript），队列也不会补开回合，主人在 bot 回复后一两秒内发的消息
  可能永远没人回。
- 自己发出的私聊回声（`message_sent`）按 `target_id` 归到对方的会话，而不是
  `private:<本账号>`；与发送账本按真实 `message_id` 去重，不再存两份。
- 只有带 `message` 的发送才写入 `self` 归档；戳一戳不再被记成一条消息，带 `group_id`
  的戳一戳作用于那个群。
- `VfsStore::read_bytes` 支持文本节点（以前对文本文件返回 NotFound 或空）。
- provider 的 `timeout_secs` 改为只限制“拿到响应头”；此前是 reqwest 的总超时，
  超过 120 秒的长思考流会被截断并整段重试计费。流式响应体由 30 秒空闲超时保护。
- 后台记忆复盘在 Windows 开机不满一个复盘间隔时不再因 `Instant` 下溢 panic。
- monitor 的 `expires` 过大时返回错误，不再因时间相加溢出 panic（release 下会终止进程）。
- `get_group_file_url` / `get_group_file_system_info` 归为查询：受 `receive_scopes`
  约束，`approve` 模式下不再需要主人确认。

### 文档

- 安装指南：NapCat 必须配置“WebSocket 服务器”（正向），并说明
  `messagePostFormat` / `reportSelfMessage`。
- 审批、发送范围、发出的文件与媒体按新行为重写。

## [0.2.0-alpha.35] - 2026-11-06

收尾审计 P2。

### 修复

- `prune_tool_results` 只裁剪发给 provider 的副本，不再改写已提交历史。
- `Monitor fire` 在 `min_interval` 内返回错误，而不是假报成功。
- 子代理续聊加 dispatch 锁，避免并发重启覆盖历史。
- `cap_batches` 死分支移除。
- QQ 入站图片按 NapCat segment 的 `url` / `file`（URL、`base64://`、容器路径）解析；
  容器路径通过 `get_image` 换取 URL，再下载为 image prompt（仅 QQ CDN，≤3.5 MB）。
- `Read` 超过 3.5 MB 时自动缩到 2000px 并转 JPEG。
- 请求级 wall-clock 超时：请求最多使用回合剩余预算，不能越过总预算。

### 图片与协议补充

- 入站图片 prompt 现在随 transcript 持久化，不再只在到达回合可见。
- 连续 tool 结果合并为一个 user 消息，图片排在全部 tool_result 之后。
- 补 GIF/BMP 解码；MCP image-only 结果不再把 base64 复制进正文。
- `get_image` 支持返回 base64；API 响应中的 base64/image 自动转图片附件。
- `image.convert` 按输出扩展名编码为 JPEG/PNG/WebP，未知扩展名报错；WebP 为无损编码，quality 只作用于 JPEG。
- 回合 early-return 前排空 local SteeringQueue；关闭 receiver 让晚到消息回填 ChatQueue。
- API base64/image 响应重新接入图片附件；User 图片计入 token 估算与裁剪。
- 自动入站图片按模型 `input`/`vision` 门控。
- 解析 NapCat image segment 的 `path` 字段；`get_image` 可用 path 作为 fallback。
- `rotate()` 持 current 锁；`idle_only` 也检查运行中的 injected 回合。

### 重构与结构

- 单会话化：删除 `[session].scope`、`SessionScope`、`ScopeCache` 与 `SessionStore.scope`；运行时只有一条 transcript，由单个 `turn_lock` 串行。
- `app.rs` 拆为 `app/{mod,assembly,events,bootstrap}.rs`；`config/sections.rs` 拆为 19 个 section；`runner.rs` 拆出 `scope/compaction/review/injected/images/evidence/ops`；`MessageStore` 拆出 `codec/index/outbox/retention`；`ApiTool` 拆出 `policy/image_ops`；`VfsStore` 拆出 `codec/tree`。
- 模块 2-循环清零：`ExecutionMode`/`ToolExecutor` 归 `tools`，消息与证据归 `transcript`，`ProviderSlot` 归 `providers`，action 元数据归 crate 根 `actions`。
- `Runner` 字段 25→15（`RunnerDeps`）；`run_turn` 拆为 `drive_turn`/`commit_turn`。
- provider 外壳合并为 `HttpProviderCore` + 泛型 `pump`；5 份单轮补全合并为 `providers::complete_text`。
- `hold` 只拒绝 QQ 操作（`napcat.*`）；MCP、下载、Web 与本地工具不再受 hold 影响。
- 公开面收缩：7 个内部模块改 `pub(crate)`，全仓 `pub fn` 381→246。
- `actions` 注册表按 namespace 拆为 9 个 helper，`Box::leak` 清零。
- 图片工具收敛为 `image.*`：`info`、`transform`（裁剪/任意角度旋转/缩放/缩略）、`convert`、`text`、`fonts`；文字/字体来自系统字体库（fontdb + imageproc），不内置字体。

### 测试

- `cargo test --all-targets`：377 项通过（365 lib + 12 integration），2 ignored；`cargo fmt --check`、`clippy -D warnings` 通过。

## [0.2.0-alpha.34] - 2026-11-04

补齐事件审计、图片自动注入与剩余审计项。

### 新增

- `/system/events`：最近 100 条 notice/request/meta/unknown 原始事件（加密落库）。
- 入站 QQ 图片在唤醒回合内自动下载为 image prompt（仅 QQ 图片域名，≤3.5 MB）。
- `media.image_watermark`。
- `media.image_rotate` / `image_thumbnail` / `image_compress`。
- MCP image block 进入 `ToolResult.images`。

### 修复

- 唤醒预设重新走 `ChatQueue`，恢复去抖/并发/`idle_only`。
- 30 秒逐 chunk 流空闲超时。
- 旧 transcript 行号偏移自动尝试恢复。
- runtime context 不再写入 transcript。
- 截断 tool args 不再以 `{}` 执行；重试 jitter；`Retry-After` 不截断。
- session 创建改为 `create_new` 原子占位。
- monitor batch flush 竞态；移除不可达的 `cap_batches` 分支。

## [0.2.0-alpha.33] - 2026-11-04

用真实 API usage 估算上下文，避免图片把上下文撑爆。

### 修复

- `estimate_messages` 优先使用最近一次 assistant 响应的
  `input/output/cache_read/cache_write` 作为前缀基准，只估算尾部消息；
  无 usage 时才退回逐条估算。
- 图片仍按 Claude Code 的保守 2000 tokens/张计入尾部；实际计费以 provider
  返回的 usage 为准。
- 避免“图片 base64 很大但估算为 0”导致自动压缩触发过晚。

## [0.2.0-alpha.32] - 2026-11-04

图片附件随 transcript 持久化，重启后仍可恢复。

### 变更

- `ToolResult` / `ToolResultMessage` 的 `images` 不再 `skip_serializing`；非空时
  写进加密 transcript 与 turn buffer，`read_segment` 会还原 base64 image。
- 图片大小仍受 `Read` 的 3.5 MB / MIME 限制，避免单条 transcript 无限膨胀。

### 测试

- `cargo test --all-targets`：345 项通过；`clippy -D warnings` 通过。

## [0.2.0-alpha.31] - 2026-11-04

- 同步 `Cargo.lock` 版本，修复 `cargo build --release --locked` 失败。
- 合并 alpha.30 的回合串行化、transcript 行号、Chat 流截断与图片链路修复。

## [0.2.0-alpha.30] - 2026-11-04

按 pi / Claude Code 对照审计修复回合与图片链路。

### 修复

- 主 agent 回合加全局互斥：shared transcript 下控制、monitor、普通回合不再并发交错；
  `Monitor fire` 改为后台投递，避免在工具栈里重入回合锁。
- transcript append/read 按分段串行化，行号写入加密信封；修复并发 append 导致的
  AAD 行号错位和“skipping unreadable transcript line”。
- Chat 协议流没有 `finish_reason` 时不再伪造 `EndTurn`，改为 Network 错误，避免
  半截响应被当成成功提交。
- `run_injected` 计入 daily budget，monitor/control 回合不再绕过预算。
- 图片：Chat 协议真正发送 `image_url`；`Read` 限制 3.5 MB 与 png/jpeg/gif/webp；
  `media.image_resize`/`crop` 限制 8192 并做 checked 边界；图片计入 token 估算，
  旧工具结果裁剪时清理图片附件。
- `API` 返回值中的 base64/image 自动转成图片附件；描述补齐 `media.*` 第四个命名空间。

### 测试

- `cargo test --all-targets`：344 项通过；`clippy -D warnings` 通过。

## [0.2.0-alpha.29] - 2026-11-04

消息不再逐条唤醒 agent，改为 inbox + 可删除唤醒预设。

### 变更

- 所有非 self、非 backfill 的 QQ 消息只写入 `/inbox` 与 transcript，不再进入
  队列逐条触发回合，避免消息洪峰把 agent 拖进无休止的回复。
- 启动时安装两个可删除的 Monitor 预设：
  - `wake-on-mention`：非主人的 @ 消息；
  - `wake-on-owner`：`owner_qq` 的消息。
  删除会写入 `monitor.removed.<name>`，重启不再恢复；手动 `Monitor add` 同名可恢复。
- Monitor 谓词新增 `event.sender != 'qq'`，用于避免主人 @ 时两个预设重复唤醒。
- `MonitorEvent` 的 `mentions_self` 现在按登录账号判断，不再把任意 @ 当成 @ 自己。
- `Read` 读取 VFS 图片时返回 image attachment；工具结果新增图片通道，Chat 协议
  转成 `image_url`，Anthropic/OpenAI 走各自的 image block。
- `API` 新增 `media.*`：`image_info`、`image_resize`、`image_crop`、`image_convert`；
  NapCat/扩展 action 响应中的 `base64`/`image` 也会自动转成图片附件。
- 主人消息触发 monitor 时，投递带 `owner_intent`，回合拥有 owner 证据，不会被
  `send_private_msg` 的 owner-intent 闸拦下；这是之前“图片消息不回复”的直接原因。
- QQ notice（拍一拍、撤回等）、好友/群请求、带 chat 字段的未知事件都转成
  inbox 伪消息；正文含摘要与压缩原始 JSON，`raw` 完整保留。
  monitor 的 `event.text` 也能看到 `[poke]` / `[recall]` 文本。

### 测试

- `cargo test --all-targets`：370 项通过；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.28] - 2026-11-04

修正 Chat 协议的 reasoning effort 映射。

### 修复

- 之前把 `max`/`xhigh` 一律向下映射成 `high`，注释还写成“Chat 只有
  low/medium/high”。实际取值由供应商决定：OpenAI 是 `low/medium/high`，
  DeepSeek 是 `low/high/max`。
- `apply_reasoning` 现在直接透传模型声明的 `low/medium/high/max`，不再猜供应商；
  `thinking.enabled = false` 时也不发送。
- 测试改为断言 DeepSeek 的 `max` 原样到达请求体。
- 新增 `[agent].retry_mode`：`stepped`（默认，base、2×base…封顶 `retry_max_ms`）
  或 `fixed`（每次固定 `retry_base_ms`）；`Retry-After` 仍优先但受上限约束。

### 测试

- `cargo test --all-targets`：347 项通过；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.27] - 2026-11-04

修复 Windows checkout 的 CRLF 导致 companion 提示词丢失。

### 修复

- `static_prefix` 在首次渲染时把模板的 `

` 归一化为 `
`；此前 Windows
  CI 上 `render_static(Companion)` 找不到 `

`，会把身份之后的全部提示词截掉。
- 新增 `.gitattributes`，源码/文本文件统一 `eol=lf`，避免 Windows checkout
  再引入 CRLF。
- alpha.26 的 Windows 产物受该问题影响；alpha.27 为修复版本。

### 测试

- `cargo test --all-targets`：347 项通过；`cargo fmt --check`、
  `clippy -D warnings` 通过；CI 的 Windows job 是本次修复的回归验证。

## [0.2.0-alpha.26] - 2026-11-04

发布构建与流水线。

### 构建

- `[profile.release]` 改为性能优先：`opt-level = 3`、fat LTO、单 codegen unit、
  `panic = "abort"`、`strip = "symbols"`。本机 Windows 上体积从 17.17 MiB
  降到 10.19 MiB。
- 新增 `.github/workflows/release.yml`：推送 `v*` 标签时在 Linux/Windows 原生
  runner 上 `cargo build --release --locked`，上传二进制与 sha256，并创建
  GitHub Release；也可手动 `workflow_dispatch`。

## [0.2.0-alpha.25] - 2026-11-04

用模拟流量补长跑覆盖，关闭剩余 TODO。

### 测试

- 新增 `soak_many_turns_stay_bounded_with_compaction`：fake provider 下连续 300 个
  回合、持续触发压缩，断言历史长度保持上限内、无 panic；覆盖压缩/轮转/提交路径。
- MSRV 记录为 Rust 1.85（edition 2024）；`napcat-link` 固定 0.1.0-alpha.2。
- `cargo test --all-targets`：347 项通过（335 lib + 4 config + 1 live stub +
  7 NapCat，2 项真实 API 测试默认忽略）；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.24] - 2026-11-04

补齐最后两个非真实 API 项：审查前缀缓存与剩余 hook 注入。

### 新增

- **周期检查复用主 system 前缀**：`SecurityReviewer` 接收 Runner 当前的 system
  块，审查请求以相同前缀开头，provider 可命中 prompt cache；消息窗口仍独立。
- **剩余 `inject_context` 落点**：
  - Stop → 作为 System 消息进入本轮提交（下一轮可见）；
  - SessionEnd → 清空后写入新 history/session；
  - ConfigChange → 写入内存 history（下一次压缩落盘）。

### 测试

- 新增：审查请求的 system[0] 与主前缀一致且 `cache = true`。
- `cargo test --all-targets`：346 项通过（334 lib + 4 config + 1 live stub +
  7 NapCat，2 项真实 API 测试默认忽略）；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.23] - 2026-11-04

补齐 `context_sources`：按内容来源而不是目标 chat 判断跨 chat 泄露。

### 新增

- `TurnEvidence` 携带本轮上下文片段及来源 chat：触发批次、pending history、
  `Read /messages|/inbox`、`API get_*_msg_history` 都会记录（最多 32 条、每条 8k 字符）。
- `auto` 门禁发送前把外发内容与来源 chat 不同的片段做 5-gram 比对；共享 5 个以上
  即拒绝，owner intent 或发回来源 chat 放行。
- 工具执行器在执行后根据 `Read` 路径 / `API` 参数自动登记来源，不需要模型自报。

### 测试

- 新增：上下文来源拦截与 owner/来源 chat 放行、Read/API 来源解析。
- `cargo test --all-targets`：345 项通过（333 lib + 4 config + 1 live stub +
  7 NapCat，2 项真实 API 测试默认忽略）；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.22] - 2026-11-04

把 NapCat 模拟测试扩到全部 `napcat.*` action。

### 测试

- 新增 `every_napcat_action_round_trips_over_the_mock`：按 action 注册表的参数
  schema 生成最小请求，逐个经 `API` 工具发到本地 OneBot 11 模拟服务，断言：
  - action 名去掉 `napcat.` 后原样出现在 WebSocket 请求里；
  - 参数 schema 是对象；
  - 所有 action 都能完成一次请求/响应（含发送、查询、管理、文件、请求处理）。
- 模拟服务只在 `get_group_info` 且 `group_id = 404` 时返回失败，正常 action 全部成功。
- `cargo test --all-targets`：343 项通过（331 lib + 4 config + 1 live stub +
  7 NapCat，2 项真实 API 测试默认忽略）；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.21] - 2026-11-04

让 `Agent message` 真正进入运行中的子代理。

### 新增

- 子代理实例持有消息通道；`AgentHub::message` 在运行中把消息送进当前回合，
  `AgentLoop` 在回合边界排空，不再只堆在实例历史里等下一次运行。
- 子代理结束后把完整历史（含注入消息）写回实例，续聊不会丢上下文。
- 运行结束后清理通道；发送失败时回退为入历史并返回 `queued`。

### 测试

- 新增：运行中的子代理在 50ms 内收到 `Agent message`，响应标记 `delivered`。
- `cargo test --all-targets`：342 项通过（331 lib + 4 config + 1 live stub +
  6 NapCat，2 项真实 API 测试默认忽略）；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.20] - 2026-11-04

新增 WebFetch 的本地抓取模式，消除“先由后端取内容、再复核 URL”的窗口。

### 新增

- **`[tools.web_fetch].local = true`**：直接 GET，逐跳校验重定向（最多
  `max_redirects`），每一跳都走 scheme/域名/公网地址检查与公共 DNS 解析器；
  读取上限 2 MiB 或 `max_content_tokens × 4` 字符。
- HTML 转文本：去掉 `script`/`style`/标签，解码常见实体并折叠空白。
- 本地模式不要求 `base_url` 或 API key；默认仍为后端模式，行为不变。

### 测试

- 新增：HTML 转文本、302 跳转 + HTML 抓取（本地 TCP 服务器）、内容类型/最终 URL。
- `cargo test --all-targets`：341 项通过（330 lib + 4 config + 1 live stub +
  6 NapCat，2 项真实 API 测试默认忽略）；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.19] - 2026-11-04

补齐 `auto` 门禁的内容级规则；数据库迁移到 v11。

### 新增

- **跨 chat 5-gram 重叠**：长文本（≥24 字）在 10 分钟内与发往其他 chat 的内容共享
  3 个以上 5-gram 时，视为搬运/转发，要求 owner intent；短句与正常短回复不受影响。
- **媒体来源**：新表 `media_origins` 记录图片/文件/语音/视频第一次出现的 chat；
  转发到其他 chat 需要 owner intent，发回来源 chat 不需要。
- 规则顺序：@all → 媒体来源 → 重复/跨群同文 → 5-gram 重叠，全部在成功发送前。

### 测试

- 新增：5-gram 拦截与 owner 放行、媒体跨 chat 拦截、来源 chat 放行。
- `cargo test --all-targets`：339 项通过（328 lib + 4 config + 1 live stub +
  6 NapCat，2 项真实 API 测试默认忽略）；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.18] - 2026-11-04

补齐 `approve` 的第二个确认通道与持久化；数据库迁移到 v10。

### 新增

- **master 私聊 nonce**：`approve` 模式登记请求后，API 通过 NapCat 私聊 `master_qq`
  发送 6 位 nonce；主人在该私聊回复 nonce 即批准。只接受 owner/master 的私聊、
  非 self、非补消息；不进入 agent 回合。
- **审批持久化**：新表 `gate_approvals` 保存 action、摘要、参数哈希、nonce、状态与
  时间戳，不保存消息原文；重启后未过期的 pending/approved 记录仍可消费一次。
  批准窗口 120 秒、pending 60 秒不变。
- **请求 id 防重启碰撞**：改为 `g<毫秒>-<纳秒>-<计数器>`，避免进程计数器归零后覆盖
  数据库中仍有效的旧请求。

### 测试

- 新增：nonce 批准与一次性消费、重启后按参数哈希恢复、API 私聊通知、以及
  NapCat 本地 WebSocket 上的完整 nonce 往返（通知 → 批准 → 成功发送）。
- `cargo test --all-targets`：337 项通过（326 lib + 4 config + 1 live stub +
  6 NapCat，2 项真实 API 测试默认忽略）；`cargo fmt --check`、
  `clippy -D warnings` 通过。

## [0.2.0-alpha.17] - 2026-11-04

补完剩余事件 hook 与配置字段，并新增本地 NapCat 协议模拟测试。

### 新增

- **全部 `[hooks.events]` 事件触发**：SessionStart/End、Pre/PostCompact、
  MessageReceived、BeforeSend、Stop、ConfigChange。`log`/`block`/`inject_context`/
  `notify` 生效；`inject_context` 落到 SessionStart（system）、MessageReceived（回合提示）、
  Pre/PostCompact（摘要输入/新分段）；`notify` 支持 owner/master 私聊。
- **`guard.max_state_tokens`**：用于截断闸看到的 state（不再固定 32k 字符）。
- **`models.json.default_thinking_level`**：`[agent].thinking_level` 为空时作为默认 effort。
- **NapCat 协议集成测试**：本地 WebSocket 模拟 OneBot 11/NapCat 的
  `{action, params, echo}` 与 `{status, retcode, data, echo}`，覆盖登录、版本、群/私聊发送、
  API 工具请求形状、失败响应与事件推送；测试按 napcat-link 源码的请求/响应结构实现。

### 变更

- 删除无效字段 `security.guard.delta_every_tokens` 与 `style.sample_retention_days`
  （严格 schema，旧配置含这两个键会报未知字段）。
- `QqClient::call` 的错误信息现在包含 NapCat 返回的 message，而不是只留方法名。

### 测试

- `cargo test --all-targets`：334 项通过（324 lib + 4 config + 1 live stub + 5 NapCat，
  2 项真实 API 测试默认忽略）；`cargo fmt --check`、`clippy -D warnings` 通过。

## [0.2.0-alpha.16] - 2026-11-04

把上一轮列出的“配置写了但不生效”补齐。数据库无新迁移。

### 新增

- **`[budget]` 真正执行**：每日 token/USD 在回合开始检查，超限跳过新回合（消息已入库）；
  `turn_tokens` / `round_trip_wall_secs` 在循环内硬停；`chat_sends_per_hour` 只统计
  成功发送；`chat_turns_per_hour` 限制每 chat 新回合；`guard_usd_per_day` 用估算成本，
  耗尽后分类器降级为规则层（规则仍生效）。
- **`[hooks.events]` 的 BeforeSend**：`log`、`block`、`rewrite` 接线到 API 发送路径，
  支持 `group_id` / `user_id` / `message_type` 匹配与 `scope = model|all`。
- **`[tools].max_schema_tokens`**：超限时把过长描述压到 160 字符；仅 schema 超限则
  告警而不删工具。
- **monitor `cc` / `on_missing`**：绑定子代理的投递可抄送主代理；目标缺失时按
  `drop` / `queue`（最多 60 秒）/ `spawn`（新建 general 子代理）处理。
- **`get_msg` 的读范围**：通过消息索引反查 chat，查不到且在限制读取时保守拒绝。

### 文档

- `limits.md` 只保留仍存在的边界：BeforeSend 之外的事件 hook、`max_state_tokens`、
  周期检查缓存、WebFetch 后端抓取、approve 的 IPC-only 确认等。

### 测试

- 新增：预算累计/USD/guard 降级、每 chat 发送上限、BeforeSend block/rewrite/匹配、
  schema 预算、monitor cc、get_msg 范围。
- `cargo test --all-targets`：327 项通过（2 项真实 API 测试默认忽略）；
  `cargo fmt --check`、`clippy -D warnings` 通过。

## [0.2.0-alpha.15] - 2026-11-04

第二轮审查：修掉 alpha.14 新代码里的边界问题，并把两个“配置写了但不生效”的
安全/资源项接上。数据库无新迁移。

### 修复

- **记忆复盘在压缩后停摆**：压缩会把 transcript 截短，`last_reviewed` 不回落时
  会永远认为“没有新增”；长度回退时重置基线。
- **发送记录早于结果**：门禁在调用 gateway 前就写入“已发送”，一次超时会让重试被
  重复规则挡住。改为只在成功后 `note_send`，失败/超时不再污染记录；短应答
  （<12 字符）不参与同 chat 重复判定。
- **主人通知被误拦**：给 `master_qq` 的私聊是通知通道，现在即使没有 owner intent
  也放行（仍受发送范围、门禁与 hold 约束）。
- **超时只包住单次读取**：安全审查/LLM 判定在事件持续到达时可以绕过总超时；
  三个循环都增加固定 deadline 检查。
- **模型未声明上下文窗口**：`context_window = 0` 不再把压缩阈值压到 1，回退到
  `turn_context_budget`。
- **保留天数未执行**：`message_retention_days` / `session_retention_days` 之前只有
  `/system` 展示。消息在 append 时、会话在 rotate/启动时真正清理过期分段。
- **工具结果无上限**：`[tools].max_result_bytes` 现在截断单次工具结果（按字符边界），
  避免一次 Read/Grep 撑爆上下文。
- **`receive_scopes` 只在 prompt 里**：消息/收件箱/联系人视图与带 `group_id`/
  `user_id` 的 API 查询现在按范围过滤；越界读取返回 NotFound。

### 文档

- `limits.md` 增加仍未执行的 `[budget]`、`[hooks.events]`、`max_schema_tokens`、
  monitor `cc`/`on_missing`，以及 `get_msg` 无法按 chat 过滤的边界。

### 测试

- 新增：压缩后复盘基线、成功/失败发送与短应答、master 通知、保留天数、
  工具结果截断、视图与 API 的 receive scope。
- `cargo test --all-targets`：316 项通过（2 项真实 API 测试默认忽略）；
  `cargo fmt --check`、`clippy -D warnings` 通过。

## [0.2.0-alpha.14] - 2026-11-04

补齐 alpha.13 文档里列出的未实现项，并完成结构收尾。数据库无新迁移。

### 新增

- **动作门禁 `[gate]`**：`trusted` / `auto` / `approve`；每分钟速率上限、
  同 chat 重复发送、跨群同文、无 owner intent 的 @全体；可选 LLM 审查（失败 fail-closed）。
  `approve` 返回待确认 id，`sbot gate pending/approve/reject` 经 IPC 裁决，
  批准后 120 秒内可重试一次，60 秒未确认失效。
- **owner intent 证据**：runtime 生成 `TurnEvidence`（控制通道 / 主人消息 / origin chat），
  经工具执行器传给 `API` 并在子代理间继承。跨 chat 发送、加好友、改资料、上传文件、
  改群名片/群名/授予权限/发公告需要证据；收缩动作与同 chat 回复不需要。
- **`backend = "llm"` 安全分类器**：复用主 provider 做输入/输出判定；`Gate` 支持在
  provider 就绪后后置注入判定器，装入前 enforce 保持 fail-closed。
- **周期安全检查**：`check_every_tokens` 在回合边界用 `review_model`（默认主模型）
  检查注入/社工与是否已照做；`high + complied` 进入 hold，`critical` 暂停新回合
  （重启从 hold 文件恢复）；超时放行，连续失败告警。
- **压缩调优**：按模型窗口减 `reserve_tokens` 计算阈值；旧工具结果按
  `prune_keep_turns` / `prune_soft_limit` 裁剪；压缩前 `memory_flush`；
  压缩后 `post_compact_recall` 注入记忆索引；`compaction_model` 可用独立模型。
- **记忆复盘语义**：`idle` 使用 `idle_minutes`，`every_turn` 按 transcript 增量触发；
  与压缩前 flusher 共用同一套提取/落盘逻辑。
- **monitor catch_up**：定时源把上次触发时间写进 `meta`，重启后 `coalesce` 补投一次
  （并说明错过次数），`skip` 放弃；运行时创建的 monitor 不参与。
- **出站账本核对**：启动时把久悬 `pending` 标为 `unknown`；发送超时/断线也标
  `unknown` 并提示先查历史，避免重复发送；`status` 暴露未确认数量。
- **网络加固**：`Download` 使用只返回公网地址的自定义 DNS 解析器（关闭 rebinding
  TOCTOU）；`WebFetch` 复核后端返回的 `final_url`。
- **群级破坏动作**：全员禁言、退群、解散群会改变主人所在群的可达性，改为要求
  owner intent（没有用户目标可做硬保护）。
- **MCP 分页**：`tools/list` 达到 10 页上限时告警，不再静默截断。

### 变更

- `tools/views.rs` 拆为 `views/{messages,inbox,sessions,contacts,config,system}.rs`。
- `Runner` 的 per-chat 缓存收进 `ScopeCache`；`Runtime::start` 的存储初始化与启动对账
  抽到 `open_stores`。
- 全仓 `cargo fmt`；新增 GitHub Actions CI（Ubuntu 上 fmt/clippy/tests，Windows 上
  clippy/tests）。
- 文档同步：`docs/guide` 的配置/安全/限制/使用/记忆页与 README 更新；
  `limits.md` 只保留仍存在的边界。

### 修复

- 发送失败不再被标记为 `failed`（那会诱导重试），改为 `unknown`。
- `sbot status` 增加 `gate` 与 `unconfirmed_sends`。

### 测试

- 新增：动作门禁（速率/重复/跨群/@all/审批/LLM 审查）、owner intent 放行与拒绝、
  LLM judge 安装、周期检查的 hold/pause、工具结果裁剪、monitor 补投计数、
  账本核对、DNS 解析过滤、views 拆分后的既有用例。
- `cargo test --all-targets`：311 项通过（2 项真实 API 测试默认忽略）；
  `cargo clippy --all-targets -- -D warnings` 与 `cargo fmt --check` 通过。

## [0.2.0-alpha.13] - 2026-11-04

审查驱动的一轮修复与文档建设。没有破坏性配置变更；数据库迁移追加到 v9。

### 修复

- **压缩**：自动压缩后切片基点改为在压缩之后取，修复越界 panic 与旧块重复提交；
  自动压缩失败按 `[compaction].max_failures` 停止重试；压缩成功后重新读取 hooks 并
  重建 system prompt；控制/monitor 注入的回合也走同一套压缩。
- **消息归档**：append 串行化，行号不再竞争；读取时校验索引行号与行内 id；
  按文件头记录的算法解密；`rebuild_index` 同时重建 `message_files` 与 `inbox_state`；
  保留清理删除孤儿未读记录。
- **会话恢复**：`turn_buffer` 增加 `scope`（迁移 v9），per_chat 只重放自己的回合，
  不再把崩溃回合写进别的 chat。
- **Provider**：SSE 字节流增量 UTF-8 解码，跨 chunk 的中文不再变成 U+FFFD；
  `

` 跨 chunk 也能正确切事件；工具参数先于 start 到达时不再丢弃。
- **安全**：`enforce` 但没有分类器时按 fail-closed 处理（每个回合 hold，启动与
  doctor 提示）；规则命中不再被概率阈值吞掉；`agent.resume` 用真实被判内容复核；
  hold 同时拒绝 Download / Web / MCP；`backend = "llm"` 在配置校验阶段明确拒绝。
- **母语/QQ**：准入过滤提前到 monitor 之前；`delete_msg` / `set_msg_emoji_like`
  恢复可用；`send_msg` 按群发送可用；owner 保护接受字符串 QQ 号、解析失败即拒绝；
  全员禁言/退群/解散不再被误当成用户目标。
- **工具**：`Copy` 受配额约束；`Grep` / `Stat` 不再标记已读；MCP stdio 单行与
  结果有大小上限；SSRF 过滤覆盖 IPv4-mapped / 6to4 / NAT64 / CGNAT。
- **子代理**：`docs` 与未开 `allow_send` 的自定义定义拿到只读 `API`，只能
  `api.list` / `api.help`；工具参数不合法时不再把 `input: null` 写进历史；
  error/refusal 路径补齐未执行的工具结果。
- **队列**：等待全局并发时不持 per-chat 状态锁；修复批次预约计数泄漏。
- **配置**：`agent.compaction_model` 默认改为空（表示用主模型），修复默认值与
  最小配置无法通过校验、导致 `sbot start` 直接失败的问题。

### 变更

- 配置 schema 拆到 `config/sections.rs`，API action 表拆到 `tools/actions.rs`，
  `app::build_agent` 的工具装配拆为 `assemble_tools`；行为不变。
- 移除未使用的 `AgentEvent::TurnEnd` / `ToolExecutionUpdate` 与
  `ModelDef::map_thinking_level`。
- 根目录旧资产 `prompt.md`、`llm.example.toml` 移到 `docs/archive/0.1/`。

### 文档

- 新增 `docs/guide/`：安装、配置、使用、人设与记忆、工具与 VFS、安全、故障排查、
  当前限制。
- `README.md` 重写并与代码对齐；`config.example.toml` 改为可运行的 0.2 样例，
  由 `tests/config_example.rs` 校验。
- 开发文档更新模块地图（迁移 v9、config/sections、tools/actions）与结构约定。

### 测试

- 新增回归测试：自动压缩、并发 append、索引错位、文件头加密、turn buffer 作用域、
  SSE UTF-8 分片、闸的 fail-closed/阈值/resume、只读 API、工具配对、队列预约、
  复制配额、Grep 不标已读、MCP 行上限、示例配置校验。
- `cargo test --all-targets`：289 项通过（2 项真实 API 测试默认忽略）；
  `cargo clippy --all-targets`：无警告。

## [0.2.0-alpha.12] - 2026-10-30

0.2 是完整重写：旧 0.1 的模块划分、数据结构与调用方式全部放弃，按
`.agents/plan/2026-10-28-rebuild-design.md` 重新实现。量化对比见
`docs/development/refactor-report.md`（复用 0 行，重写 97.5%，废弃 2.5%）。

### 新增

- **配置与凭据**：严格 `sbot.toml` schema（`version = 1`、未知字段拒绝、字段路径校验）、`SBOT__SECTION__KEY` 覆盖、`models.json` 模型清单、明文优先的 `auth.json`（掩码 Debug）。
- **存储**：SQLite 迁移 v1–v8、主密钥 `~/.sbot/key`、HKDF 域分离、XChaCha20-Poly1305、canary 校验、`SBOT_HOME`。
- **VFS**：POSIX 路径与 NFC、文本加密、1 MiB blob 分块、回收站、Read-before-Edit、配额与磁盘水位、种子布局。
- **Provider**：Anthropic Messages（自适应 thinking、effort）、OpenAI Chat Completions、OpenAI Responses、SSE 解析、工具参数按 index 组装、`Retry-After`、按模型回退链。
- **Agent 循环**：pi 风格内/外循环、事件流、steering / follow-up、配对的工具结果、`max_tokens` 带工具时不执行、输出闸、parallel/sequential 工具执行、refusal 进入 hold。
- **QQ**：NapCat 连接、事件归一化、内部标签剥离、准入门（拒收）、断线重连补消息（不触发回合）、JSONL 按群/私聊/月/日分区、索引重建、inbox 状态、发送账本。
- **安全**：规则层 + Jev `/v1/systemone` 分类器、永久 hold、`SECURITY_HOLD`、安全事件审计、主人/管理员目标保护、hold 期间拒绝全部 `napcat.*`。
- **会话**：加密 transcript 分段与 `prev_file` 链、turn buffer 崩溃恢复、CJK 感知压缩、回合边界切割、加密 `compaction_checkpoints`。
- **工具**：文件工具、动态视图（`/messages`、`/inbox`、`/sessions`、`/friends`、`/groups`、`/config`、`/system`）、`API` 命名空间、`Memory`（含 `memory_entries`）、`Skill`、`Download`（SSRF）、`WebSearch` / `WebFetch`、`Monitor`（qq/timer/internal、cron、batching、idle_only、backlog）。
- **子代理（swarm）**：`Agent` 工具的 spawn / message / check / wait / list；`general` / `explore` / `plan` / `docs` 内置类型；`/agents/*.md` 自定义定义；工具集强制为父代理子集；并发上限；独立上下文。
- **MCP**：可选、默认关闭；stdio JSON-RPC `initialize` / `tools/list` / `tools/call`；单个 `MCP` 工具；服务器启动失败只告警。
- **提示词**：v4 静态/动态边界模板、context hooks、每回合 runtime 上下文。
- **IPC**：Windows 命名管道（DACL）/ Unix UDS（0600）、长度前缀 JSON、幂等表。
- **CLI**：`start/stop/restart/status`、`ai`、`chat`、`pause/resume`、`security`、`model`、`effort`、`backup`、`doctor`。
- **备份与诊断**：`sbot backup` 在线 SQLite 快照（Busy/Locked 重试）+ 文件归档，拒绝写入被复制目录；`sbot doctor` 只读检查配置、canary、索引、孤儿 blob、记忆三方对账、权限与路径边界。
- **模型热切换**：`sbot model set` / `effort set` 通过共享 provider 槽位在下一回合生效。

### 变更

- CLI 行为与 0.1 不兼容；不提供旧数据迁移，0.2 从空存储开始。
- 文件能力改为 VFS，不再使用 `cap-std` 工作区。
- 白名单语义改为**拒收门**：被拒消息不进入任何存储。

### 已知限制

- **NapCat 协议实证**：修复 `call_api` 返回 `data` 的解包错误；新增 `tests/live_napcat.rs`（真实 WebSocket 收发）与 `examples/mock_onebot.rs`；完整管线用真实模型回复群消息并发送成功。
- **后台复盘**：空闲时从 transcript 提取记忆候选（只追加、来源 `self`）、更新 `/STYLE.md`；技能使用记录 + curator 归档长期未用技能（不删除，`pinned` 跳过）。
- **`per_chat` 会话隔离**：每个 chat 一条独立 transcript 链与历史，压缩/轮转/保留/崩溃恢复按 chat 执行；迁移 v8 增加 `scope` 列。
- **子代理嵌套**：`max_depth` 深度限制；`Agent message` 续聊复用原定义的提示、工具集与步数；`Agent spawn` 的 `monitor` 参数与 `deliver.to = <spawn_id>` / `agent:<name>` 把事件绑定到子代理实例。
- `/config` 写入支持白名单键并原子落盘；`[filter]` 与 `[queue]` 写入立即热更新（`max_concurrent` 的信号量上限需重启）。
- 真实 Chat 端点与真实 Jev 的测试默认 `#[ignore]`，需要环境变量显式启用。

## [0.1.0-alpha.2] - 2026-03-23

历史版本（0.1 系列）。以下内容已被 0.2 取代，保留用于对照。

### 新增

- **蜂群子代理系统** — 主 Agent 可通过 SpawnAgent 生成子代理并行执行任务
  - SwarmManager 管理子代理完整生命周期（创建、消息传递、状态查询、等待完成）
  - SubAgentRunner 轻量级 agentic 循环（独立 tokio 任务，独立 LLM 对话）
  - 子代理可嵌套 spawn 子代理，深度受 `swarm.max_depth` 限制
  - 完成通知机制：子代理通过 SwarmNotification 唤醒主代理
- **4 个蜂群工具** — SpawnAgent / MessageAgent / CheckAgent / WaitAgent
  - SpawnAgent：生成子代理（支持指定备选模型和额外上下文）
  - MessageAgent：向运行中子代理发送消息
  - CheckAgent：查询子代理状态（支持单个或全部列出）
  - WaitAgent：等待子代理完成并获取结果（支持单个/多个/全部）
- **备选模型配置** — `[[models]]` 段定义子代理可用的 LLM 模型
  - 支持不同提供商、API 地址、上下文窗口等独立配置
  - 可选字段为空时自动继承主 `[llm]` 配置
- **蜂群配置** — `[swarm]` 段控制子代理系统安全参数
  - `enabled`: 功能总开关（默认 true）
  - `max_depth`: 最大嵌套深度（默认 32）
  - `max_concurrency`: 最大并发子代理数（默认 1024）
- **MCP 支持** — 通过 Model Context Protocol 接入外部工具服务器
  - Stdio 传输层（子进程 stdin/stdout JSON-RPC）
  - 工具自动发现，命名格式 `mcp.<server>.<tool>`
  - 配置: `[mcp.<name>]` 段
- **Fetch 工具** — Agent 网络访问能力
  - HTML 自动转换为 Markdown
  - SSRF 防护：私有 IP 地址拦截
  - 域名白名单/黑名单过滤
  - 默认禁用，需在 `[fetch]` 段显式启用
- **群管理工具** — 18 个群组管理工具（禁言/踢人/公告/精华等）
- **管理员状态同步** — 启动同步 + 事件驱动 + 定时轮询
- **开发文档** — architecture.md, tools-reference.md

### 改进

- **ToolDispatcher 提取重构** — 工具分发逻辑从 AgentRunner 提取为独立的 ToolDispatcher，主 Agent 和子 Agent 共享同一套工具调用能力
- **Edit 工具** — 改为 Claude Code 模式（Read 一次后可多次 Edit）
- **CLI 命令注入** — Codex 风格消息插队
- **消息自动标记已读** — 首次 LLM 请求成功后标记
- **消息段 Schema** — ToolSpec 详细描述每种消息段类型及其 data 字段

### 修复

- 修复 cmd_rx 通道关闭后 CPU 100% 空转的 busy-spin bug

## [0.1.0-alpha.1] - 2026-03-10

首个 Alpha 版本，核心架构就绪。

### 新增

- **Agent 核心循环** — 消息去抖 → LLM 调用 → 工具分发 → 多轮对话
- **OpenAI 兼容提供商** — 支持 OpenAI、DeepSeek、Moonshot 等所有兼容 API
- **原生工具调用** — 16 个内置工具（Read/Write/Edit、消息收发、群友查询、好友请求处理、Skill 加载）
- **cap-std 安全沙箱** — 文件操作硬限制在 workspace 内，含 7 层路径校验
- **写入安全防护** — 单文件 1 MB 上限、磁盘空间检查（100 MB 底线）、工作区容量限制（2 GB / 50000 文件）
- **上下文压缩** — 对话超过 token 阈值时自动压缩历史为摘要
- **收发分离白名单** — 精确控制 AI 可见范围和发言范围
- **Skill 系统** — 通过 SKILL.md 注入技能，每个 skill 目录独立沙箱
- **消息存储** — 按时间线统一存储（JSONL + Markdown），支持文件拆分
- **系统提示词构建** — 10+ 节结构化 prompt，支持 AGENTS.md / SOUL.md 人格注入
- **CLI 进程管理** — `sbot start/stop/status/run` 守护进程模式
- **控制通道** — TCP 端口控制 Agent 暂停/恢复/重载

### 依赖

- NapCatLink v0.1.0-alpha.2（OneBot 11 WebSocket SDK）
- cap-std 3（能力安全文件系统）
- fs2 0.4（磁盘空间查询）
- tokio, reqwest, serde, chrono, clap, tracing 等
