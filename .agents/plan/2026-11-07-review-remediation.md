# 审查整改计划（2026-11-07）

状态：进行中。取代 `2026-11-06-image-tools-and-review-handoff.md`（已删除，未完成项并入本文）。
工作目录：主 checkout `G:/ClaudeProjects/ScapegoatBot/ScapegoatBot`；构建只在这里用默认 `target/`。

## 1. 来源与可信度

2026-11-07 四个只读子代理分区审查（不跑 cargo）：权限与身份、文件/网络/进程、
agent 运行时正确性、协议与存储。另有一个文档/命名漂移审查（结果见 §5）。

**子代理的结论不是事实。** 每条在动手前必须对照代码核实；下文标注：

- `已修`：本轮改了代码并有回归测试，测试名写在条目里；
- `已核实`：主代理读过代码，问题确实存在，尚未修；
- `未核实`：只有审查报告的说法，可能是误报。

## 2. 本轮已修（2026-11-07）

| 问题 | 修法 | 测试 |
|------|------|------|
| 文件引用原样交给 NapCat：NapCat 会读宿主机任意路径（`~/.sbot/auth.json`）、替 bot 请求内网 URL。已对照 NapCatQQ `napcat-common/src/file.ts` `checkUriType` 与 `msg.ts` `handleOb11FileLikeMessage` | `tools/api/outbound.rs`：发给 NapCat 的副本一律 `base64://`；VFS 路径本进程读出，http(s) 走 NetPolicy | `outbound::tests::*`、`api::tests::{outbound_media_reaches_napcat_only_as_base64, host_file_references_never_reach_napcat}`、`every_file_param_in_the_registry_is_inlined` |
| 未声明参数透传（`send_group_notice.image` 等） | `policy.rs::declared_params` | `undeclared_params_are_rejected_before_napcat` |
| `send_scopes` 只管 Message；门禁先于范围检查，越界请求也会私聊主人 | `check_send_scope` 覆盖 `is_outbound()`，放在门禁前 | `send_scope_is_checked_before_the_approval_gate` |
| `send_msg` 同时带 user_id/group_id 时目标与 NapCat 不一致（已对照 `SendMsg.ts` `createContext`） | `chat_of` 只认恰好一个目标 | `send_msg_targets_must_be_unambiguous` |
| 私聊确认码批准永远不生效（chat 比较写成 `Private(self_id)`）；按 stand_in 修好又会让模型自我批准 | `events.rs::approval_reply_sender` 只接受另一个账号的私聊；`master_qq` 为本账号时通知不带码；确认码回复不入库 | `app::events::tests::*`、`approval_notice_omits_the_code_when_master_is_this_account` |
| 私聊 `message_sent` 回声按 `user_id`（=本账号）归档（已对照 NapCat `index.ts` 的 `target_id`） | `parse_message` 用 `target_id` | `qq::event::tests::private_self_echo_*` |
| 自发消息归档用负数占位 id，回声再存一份；戳一戳被记成消息并归到 `private:<user_id>` | 真实 `message_id`、`carries_message`、`send_poke` 带群时归群 | `only_real_messages_are_archived_with_their_napcat_id` |
| `read_bytes` 对文本节点读 blob（NotFound） | 文本节点读 `content_enc` | `outbound::tests::text_files_can_be_sent_as_files` |
| 旋转画布与解压炸弹（细图转 45° 需几十 GB） | 解码宽高 ≤ 8192；旋转先算画布；`image.text` ≤ 1000 字 | `image_ops::tests::rotation_refuses_canvases_beyond_the_side_limit`、`images_beyond_the_side_limit_are_not_decoded` |
| 系统代理绕过 `PublicOnlyResolver`（已对照 reqwest 0.12.28 `ClientBuilder::build`）；阻止列表不匹配末尾带点的域名 | `no_proxy()`；host 去尾点 | `net::tests::domain_rules` |
| provider 用 reqwest 总超时，长流被截断并整段重试计费（已对照 reqwest 文档） | 只限响应头；流体靠 30s 空闲超时 | 无单测（行为由 reqwest 语义决定） |
| 3 处 `&text[start..=end]` 在 `"} {"` 时 panic；4 份重复的 JSON 提取 | `providers::extract_json_object` | `providers::tests::inverted_braces_do_not_panic` |
| Windows 开机不满复盘间隔时 `Instant` 下溢 panic | `Option<Instant>` | 无（依赖系统开机时间） |
| monitor `expires` 过大时 `DateTime + Duration` 溢出 panic | `checked_add_signed` | `monitor::tests::absurd_expiry_is_an_error_not_a_panic` |
| `get_group_file_url` / `get_group_file_system_info` 被当成变更 | 归 Query | 现有测试 |
| 模型可见错误串里的断裂空白（policy/action_gate/web） | 修正 + 源码扫描 | `tests/source_hygiene.rs` |
| steer 进回合的消息在循环最后一次排空之后到达时“收下却不回应”，队列也不补开回合（已核实 runner/queue 代码） | 循环结束立刻关闭 steering；没回应的条数随 `TurnReport` 交还队列，按原顺序放回队首再开一轮 | `queue::tests::unanswered_steering_messages_open_a_follow_up_turn_in_order`、`runner::tests::{steering_closes_as_soon_as_the_model_loop_ends, early_exit_returns_waiting_steering_messages, skipped_turn_returns_steering_messages_to_the_queue}` |

## 2b. 0.2.0-alpha.37 已修（2026-11-08，自主运行）

| 问题 | 修法 | 测试 |
|------|------|------|
| stand_in 下主人的消息全是 `is_self`，主人没有任何 QQ 渠道（已核实） | alpha.37 的“主人之声/私线”被用户否决；alpha.38 改为发送账本 `src/qq/ledger.rs`：认领不到的本账号消息是手打的，进 inbox、由 `wake-on-owner` 唤醒 | `qq::ledger::tests::*` |
| `agent.resume` 复核当初的危险原文，输出闸触发的 hold 永远解不开（已核实） | 复核当前已提交上下文尾部；不再保存危险原文；`agent.quarantine` 同时替换上下文里的原文 | `runner::ops::tests::{the_agent_can_lift_an_output_hold_by_itself, a_poisoned_context_must_be_quarantined_before_resuming}`、`gate::tests::hold_records_the_reason_but_never_the_flagged_text`、`transcript::tests::redaction_*` |
| `critical` 周期检查暂停所有非控制回合，无人值守时永久停机（已核实） | hold + 下一回合前丢弃上下文（`discard_context`）；删除 `agent_paused` | `runner::tests::critical_risk_holds_and_discards_the_context_before_the_next_turn` |
| v4 提示词多处与代码不符；技能索引从未填充；边界后模板文本死代码；`static_prefix` 忽略 mode 且 `Box::leak`（已核实） | v5 提示词；`prompt::context` 固定注入 AGENTS/USER/MEMORY/STYLE/技能索引 + agent 自管 `/hooks/context.toml`；删除 `[[hooks.context]]` | `prompt::tests::*`、`prompt::context::tests::*`、`config::tests::retired_context_hooks_fail_with_a_migration_hint` |
| `agent.status` / `agent.quarantine` 描述承诺不存在的能力；`quarantine.message_id` 未使用 | 描述与实现一致；删参数 | 现有测试 |

## 2c. 0.2.0-alpha.39 已修（2026-11-08，P0）

| 问题 | 修法 | 测试 |
|------|------|------|
| P0-2 子代理带着 spawn 时的 owner intent、不检查 ai stop / ESTOP（已核实） | `run_subagent` 检查开关；`AgentHub::message` 用调用方证据，拒绝无 owner intent 的回合引导 owner 实例 | `swarm::tests::a_turn_without_owner_intent_cannot_steer_an_owner_spawned_subagent` |
| P0-3 子代理收尾时的消息报 delivered 后丢失 / 重启丢上一轮回答（已核实） | 收尾先关通道、补跑一轮；`message` 等 `finished` 通知再续聊 | `swarm::tests::a_message_during_wrap_up_waits_for_the_run_to_finish` |
| P0-4 非法 `when`（已核实：短路求值 + 每事件一条系统通知） | 创建/配置时逐项校验；运行时出错移除并只通知一次 | `monitor::tests::invalid_when_expressions_are_rejected_when_added` |
| P0-5 turn buffer 只在提交时写（已核实） | `AgentEvent::Appended` + `runner/journal.rs` 实时落盘 | `journal::tests::*` |
| P0-6 消息行号按索引条数（已核实） | 物理行号 + 长度校验缓存 + 残行隔离 | `storage::messages::tests::{a_lost_index_commit_does_not_shift_later_line_numbers, a_torn_last_line_is_isolated_before_the_next_append}` |
| P0-7 联系人视图为空、路由不存在（已核实） | 重写 `views/contacts.rs`，按文档路由，改用 `NapcatGateway` | `views::contacts::tests::*` |
| `run_injected` 不检查 ESTOP（核实：monitor 回合检查；控制通道有意不受 ESTOP 限制，只受 `ai stop`） | 文档写明 | — |
| `agent.quarantine` 重启失效 | 恢复上下文时按 `/quarantine/` 重放 | `runner::ops::quarantine_tests::quarantine_survives_a_restart` |

## 2d. 0.2.0-alpha.42 已修（2026-11-10）

配置重构、§5 文档 / 命名 / 工具描述漂移、P2 结构项全部处理，细节见 CHANGELOG 的 alpha.42 段。
要点：不起作用的键全部实现或删除（`[memory].review_model`、`jitter_secs`、`internal` /
`stream` 源、hooks 矩阵、`queue.mode`、会话配额、`sbot chat --scope`）；`config.example.toml`
的值由测试锁定为代码默认值；子代理写保护 / 不能删除 / monitor 只投给自己；`/trash` 只能经
RM / MV 进入；`Monitor add` 与配置同一套 schema；`wake-on-` 前缀改为显式的
`deliver.as = "chat"` 与预设来源；`app/assembly`、`monitor`、`loop`、`doctor` 拆分；
零引用函数与误导性命名清理。

## 3. 待修（按优先级）

### P0 / 高

1. `已核实` hold 的复核偏弱：`force_hold`（周期审查、refusal）不记录被判内容，复核只看
   历史尾部；默认 shadow + 无分类器时只有规则层。**用户 2026-11-07 决定：hold 必须由 agent
   自主解除**（无人值守，见 [[design-decisions]]）。所以只加强复核输入（记录触发 hold 的内容），
   不收走解除权。
2. `未核实` 子代理带着 spawn 时的 owner intent 一直运行，且不检查 `ai_enabled` /
   ESTOP / `agent_paused`；`/agents/*.md` 模型可写，能给自定义子代理开 `allow_send`
   （swarm/mod.rs:346-363、454、560、576；runner.rs:837-859）。
3. `已修` steering 丢消息（见 §2）。子代理路径（`run_subagent` 不关闭 receiver，
   `Agent message` 可能报 delivered 后丢失）未改：提前关闭会让 `AgentHub::message`
   在旧回合收尾时重启并丢弃旧结果（generation 检查），需要先给 hub 加“等旧回合结束”。
4. `未核实` 非法 `when` 谓词让每个 QQ 事件触发一次完整回合，且阻塞事件循环
   （monitor/mod.rs:496-512）。
5. `未核实` turn buffer 只在提交前写入，崩溃/停止丢失整个进行中的回合
   （runner.rs:780-791、injected.rs:139-149；shutdown 5 秒不等回合）。
6. `未核实` 消息索引提交丢失一次后，同一天文件之后的行号全部错位，重建也修不好
   （storage/messages.rs:167-250、messages/index.rs:92-130）。
7. `未核实` `/friends`、`/groups` 视图把 `QqClient::call` 已解包的结果再取 `data`，
   列表永远为空，部分路由不存在（tools/views/contacts.rs、views.rs:169、242-247）。

### P1

- `已修 alpha.40` 收缩类群管理没有 chat 绑定：`delete_msg` 可召回任意 message_id；从 A 群回合
  踢/禁 B 群成员（B 在 send_scopes 内即可）。修法：`delete_msg` 用 `chat_for_message`
  反查并过发送范围；跨 chat 的群管理要求 owner intent。
- `已修 alpha.40` 群公告、上传文件不走 ActionGate 发送规则、BeforeSend 与发送账本
  （这些仍只按 `Category::Message` 判断）。
- `已修 alpha.40` owner intent 覆盖整个批次与 steering 注入的非主人消息
  （evidence.rs:5-25、runner.rs:646）。
- `已修 alpha.40`（部分按设计）模型写 `/config/filter` 可把 owner 加进 `ignore_users`；
  可删除 wake 预设；hold 期间也能写配置。hold 期间 `/config` 只读已修；排除主人、删除
  wake 预设按用户 alpha.38 的决定属于 agent 自己的选择，不加特权通道（曾误加主人免过滤，
  已在 f17a080 撤回）。
- `已修 alpha.40` `/config/mcp` 与 `/config.toml` 暴露 `mcp.servers[].env` 里的 token；
  `persist()` 重建 `sbot.toml` 时用默认权限。
- `已修 alpha.40` VFS 回收站不计配额、`purge_trash` 运行时从不调用；从回收站恢复不检查配额。
- `已修 alpha.40`（删除该键） `[skills].shared_dir` 宿主机路径模型可写，只有 `sbot backup` 使用。
- `已修 alpha.41` 预算缺口：子代理、压缩摘要、记忆 flush、审查、门禁 LLM 都不计入；
  `record_usage` 丢弃 cache token；各 provider 的 `input_tokens` 口径不一
  （openai/chat 含缓存，anthropic 不含）。
- `已修 alpha.41` 每轮 token 上限把每次请求的完整 prompt 相加，75k 以上上下文只能跑两次请求。
- `已修 alpha.41` 预算耗尽后每次唤醒都重复压缩（usage 沿用压缩前的值）。
- `已修 alpha.41` 热切换模型后 `RunnerConfig` 的 context_window / 图片能力 / 价格不更新。
- `已修 alpha.41` 定时器：启动时覆盖上次触发时间；`At` 只触发一次；DST 当天 `.single()` 结束
  任务；cron 补投只扫 1440 分钟；日/周字段按 AND 而非 OR；`0 0 30 2 *` 长时间空转。
- `已修 alpha.41` Anthropic：server tool 的查询参数没被收集；空 `content` 的 assistant、
  无签名 thinking 会被原样回放导致 400；只缓存静态前缀。
- `已修 alpha.40` 准入过滤只作用于消息；被忽略群的 notice/request 仍入库、进 inbox、进 monitor。
- `已修 alpha.41` 停止原因未知时当作 EndTurn；408/504 不重试。
- `已修 alpha.41` `/messages` 的 `offset` / `order` 被忽略；`Stat` 动态文件几乎总失败；
  `Read` 工具示例里的过滤语法不存在。
- `已修 alpha.41` 资源放大：图片转换、下载整块驻留内存；无 `spawn_blocking`；`html_to_text`
  可能二次方。
- `已修 alpha.40` Unix socket 回退到 `/tmp/<name>.sock` 无 uid；`napcat-link` 在 INFO 日志里打印
  带 access_token 的 URL；`auth.json` 解析错误会回显密钥值。

### P2 / 结构与冗余（alpha.42 已处理，见 §2d；以下保留原始记录）

- `app/assembly.rs`（约 1000 行）拆分：`open_stores`、`assemble_tools`、`build_agent` 分组；
  先补装配层测试。
- `agent/runner.rs`：`run_subagent` 重复 `loop_config()`；预算跳过的图片持久化重复；
  `commit_turn` 9 个参数。`agent/loop.rs::run_with_steering` 拆终止分支。
- `monitor/mod.rs` 拆 predicate/cron/delivery/config；Monitor spec 解析三处重复
  （tools/agent.rs、tools/monitor.rs、monitor/mod.rs），`expires` 解析也重复三份。
- `vfs/sqlite.rs` 的 edit/read/usage/blob 写入重复。
- provider `finish()`、session/messages 信封、`complete_text` 错误映射、config JSON 读取的重复。
- 零引用 `pub fn`（`qq/event.rs::as_text`、`qq/format.rs` 三个、`events::count_blocked`、
  `turn_buffer::load_context`、`fs::read_static`）删除或说明。
- 命名：`tests/live_napcat.rs` 实为本地 mock 测试；`QqMessage::backfilled` 已无生产用途。
  等文档/命名审查结果（§5）一起处理。

## 4. 需要用户决定

- 是否 push / release / 部署 hw1（tag 本地自动打，见 [[release-workflow]]）。

## 5. 文档与命名漂移（2026-11-07 审查；alpha.42 已逐条核实并处理，见 §2d）

11-06 交接里的文档 P1 清单经复查**仍然存在**（记忆来源门控承诺、`sbot status` 输出、
默认值 max_steps/retry_max_ms、热更新、迁移 v12、`compaction_model` 说明、工具数、
源码注释若干）。另有：

**模型可见文本（优先，模型会照着做）**

- ~~`assets/system_prompt_v4.template.txt` 的各项~~：0.2.0-alpha.37 已由 v5 取代（见 §2b）。
- 工具描述：`fs.rs:63` 示例过滤语法 `contains("...")` 不存在；`fs.rs:973` 承诺 mount 与
  权限；`actions.rs:541/549` 承诺预算用量、“标记不可执行”；`tools/monitor.rs:46/54`
  `internal` 源与 `catch_up` 参数无效（`actions.rs` 的 agent.* 摘要已在 alpha.37 修正）；`swarm/mod.rs:98/228` 说 general 子代理不能改
  `/MEMORY.md`、不能删数据，但它有 Write/Edit/RM；`api.rs:244` 仍说三个命名空间。

**用户文档**

- MSRV：README / install 写 Rust 1.85，代码用 let-chains，需要 1.88（审查者引用
  Rust 1.88.0 发布说明；主代理确认代码大量使用 `if let … && let …`）。
- README：`sbot chat --scope` 只回显不生效；`model list` / `effort list` 打印的是整份
  status；目录树过时；`live_napcat` 实为本地 mock、跑在 CI 里。
- `config.example.toml`：多处默认值与代码不同；`[[hooks.events]] when = [...]` 示例取消
  注释即加载失败；自定义 `[[hooks.context]]` 会替换全部默认 hook 却鼓励取消注释。
- configuration/usage/install/limits/security/persona-memory/tools-and-vfs/
  troubleshooting/prompt-maintenance 各有若干条（详见审查原文要点）：@ 判定、
  `collect` 与 `followup` 相同、transcript 配额默认不生效、`trusted` 不计速率、
  hooks 的 `when`/`inject` 不读、ESTOP 不拦控制回合、`turn completed` 是 debug 级、
  备份包含 `auth.json`、`logs/` 无人写入、`/friends/<qq>.md` 等路由不存在、
  `Mkdir` 需要 `parents`、Agent 工具示例参数错误、`get_msg` 描述已过时等。
- `workspace/*.md`：0.1 遗留模板，无任何代码读取，内容与现行提示词矛盾。删除或重写。

**误导性命名（建议）**

- `tests/live_napcat.rs` → `tests/onebot_mock_client.rs`；
  `approve_mode_nonce_round_trip_over_napcat` 没有回复那一段；
  `readme_minimal_config_is_valid` 测的是内联副本。
- `QqMessage::backfilled`（功能已删，保留 DB 列）、`Permission`/`permissions()`（无消费者）、
  `DynamicProvider`（只有测试实现）、`Category::Media`（只有 image.*）、
  `ClientConfig.name: IpcConfig`。
- 忽略参数的函数：~~`prompt::static_prefix(mode)`~~（alpha.37 删除）、`turn_lock_for/session_for/history_for(chat)`、
  `compact_chat`、`turn_buffer::append(scope)`、`mentions_self`（含 @全体）、
  `gate::record_event`（固定 Input）。
- CLI：`Chat.scope`、`AiCommand::Restart`（同 Start）、`model/effort list`。
- `wake-on-` 名字前缀决定投递路径（隐藏行为）→ 显式字段。
- 不起作用的配置键：`[memory].review_model/topics`、`[skills].shared_dir`、
  `[[monitors]].jitter_secs`、`source = stream|internal`、`[[hooks.events]].when/inject`、
  ~~`[[hooks.context]].path/scope`~~（alpha.37 整段删除）、`[queue].mode = collect`、默认保留下的 `max_session_bytes`。

## 6. 下一步

1. 核实并修 P0-2、P0-4 ~ P0-7（P0-4/5 影响计费与崩溃恢复）。
2. 模型可见文本的漂移（§5 第一组剩余的工具描述项）。`/AGENTS.md` **不加**写保护（用户 2026-11-07 决定 agent 可写）。
3. 配置重构（用户 2026-11-07 授权：“不起作用的配置项可能是旧代码残留……需要实现的进行实现”）：
   逐个盘点 §5 列出的键，实现或删除，并同步 `config.example.toml`、文档与 CHANGELOG 破坏性说明。
   然后是用户文档与命名（§5 其余）。
4. P1 安全项（chat 绑定、`/config` 写保护与密钥暴露、配额）。
5. P2 结构拆分（先测试后拆）。
6. 每批：`cargo test --locked --all-targets`、`cargo clippy --locked --all-targets -- -D warnings`、
   `cargo fmt --all --check`，更新 CHANGELOG 的 Unreleased 段与本文。
