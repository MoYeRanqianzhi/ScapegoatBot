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

## 3. 待修（按优先级）

### P0 / 高

1. `已核实` **模型可以自己解除 hold**：`agent.resume` 无证据检查；`force_hold`
   （周期审查、refusal）不记录被判内容，复核只看历史尾部；默认 shadow + 无分类器时
   只有规则层。**原设计就是“agent 自己调 resume、runtime 复核”**，是否改成只允许控制
   通道需要用户决定（设计决定，见 [[design-decisions]]）。
2. `未核实` 子代理带着 spawn 时的 owner intent 一直运行，且不检查 `ai_enabled` /
   ESTOP / `agent_paused`；`/agents/*.md` 模型可写，能给自定义子代理开 `allow_send`
   （swarm/mod.rs:346-363、454、560、576；runner.rs:837-859）。
3. `未核实` 回合最后一次 drain 之后 steer 进来的消息丢失（runner.rs:503-519、754-761；
   loop.rs:429-437）；早退路径同理。
4. `未核实` 非法 `when` 谓词让每个 QQ 事件触发一次完整回合，且阻塞事件循环
   （monitor/mod.rs:496-512）。
5. `未核实` turn buffer 只在提交前写入，崩溃/停止丢失整个进行中的回合
   （runner.rs:780-791、injected.rs:139-149；shutdown 5 秒不等回合）。
6. `未核实` 消息索引提交丢失一次后，同一天文件之后的行号全部错位，重建也修不好
   （storage/messages.rs:167-250、messages/index.rs:92-130）。
7. `未核实` `/friends`、`/groups` 视图把 `QqClient::call` 已解包的结果再取 `data`，
   列表永远为空，部分路由不存在（tools/views/contacts.rs、views.rs:169、242-247）。

### P1

- `未核实` 收缩类群管理没有 chat 绑定：`delete_msg` 可召回任意 message_id；从 A 群回合
  踢/禁 B 群成员（B 在 send_scopes 内即可）。修法：`delete_msg` 用 `chat_for_message`
  反查并过发送范围；跨 chat 的群管理要求 owner intent。
- `未核实` 群公告、上传文件不走 ActionGate 发送规则、BeforeSend 与发送账本
  （这些仍只按 `Category::Message` 判断）。
- `未核实` owner intent 覆盖整个批次与 steering 注入的非主人消息
  （evidence.rs:5-25、runner.rs:646）。
- `未核实` 模型写 `/config/filter` 可把 owner 加进 `ignore_users`；可删除 wake 预设；
  hold 期间也能写配置。
- `未核实` `/config/mcp` 与 `/config.toml` 暴露 `mcp.servers[].env` 里的 token；
  `persist()` 重建 `sbot.toml` 时用默认权限。
- `未核实` VFS 回收站不计配额、`purge_trash` 运行时从不调用；从回收站恢复不检查配额。
- `未核实` `[skills].shared_dir` 宿主机路径模型可写，只有 `sbot backup` 使用。
- `未核实` 预算缺口：子代理、压缩摘要、记忆 flush、审查、门禁 LLM 都不计入；
  `record_usage` 丢弃 cache token；各 provider 的 `input_tokens` 口径不一
  （openai/chat 含缓存，anthropic 不含）。
- `未核实` 每轮 token 上限把每次请求的完整 prompt 相加，75k 以上上下文只能跑两次请求。
- `未核实` 预算耗尽后每次唤醒都重复压缩（usage 沿用压缩前的值）。
- `未核实` 热切换模型后 `RunnerConfig` 的 context_window / 图片能力 / 价格不更新。
- `未核实` 定时器：启动时覆盖上次触发时间；`At` 只触发一次；DST 当天 `.single()` 结束
  任务；cron 补投只扫 1440 分钟；日/周字段按 AND 而非 OR；`0 0 30 2 *` 长时间空转。
- `未核实` Anthropic：server tool 的查询参数没被收集；空 `content` 的 assistant、
  无签名 thinking 会被原样回放导致 400；只缓存静态前缀。
- `未核实` 准入过滤只作用于消息；被忽略群的 notice/request 仍入库、进 inbox、进 monitor。
- `未核实` 停止原因未知时当作 EndTurn；408/504 不重试。
- `未核实` `/messages` 的 `offset` / `order` 被忽略；`Stat` 动态文件几乎总失败；
  `Read` 工具示例里的过滤语法不存在。
- `未核实` 资源放大：图片转换、下载整块驻留内存；无 `spawn_blocking`；`html_to_text`
  可能二次方。
- `未核实` Unix socket 回退到 `/tmp/<name>.sock` 无 uid；`napcat-link` 在 INFO 日志里打印
  带 access_token 的 URL；`auth.json` 解析错误会回显密钥值。

### P2 / 结构与冗余（沿用 11-06 交接，未重新核实行号）

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

- §3 P0-1：hold 是否只允许控制通道解除（改变原设计）。
- `image.*` 五个 action 的命名与参数（11-06 起待确认，见 [[image-tools]]）。
- 是否 push / release / 部署 hw1；本地 tag `v0.2.0-alpha.35` 未指向 HEAD。

## 5. 文档与命名漂移

文档/命名审查子代理尚未返回时写入本文；返回后把核实过的条目补到这里。
11-06 交接里的文档 P1 清单（记忆来源门控承诺、`sbot status` 输出、默认值、热更新、
迁移版本、`compaction_model` 说明、工具数、源码注释）**尚未处理**。

## 6. 下一步

1. 核实 P0-2 ~ P0-7，逐条修复并补测试（P0-3/4/5 影响消息不丢、不重复计费）。
2. 处理文档/命名漂移（§5）。
3. P1 中的安全项（chat 绑定、`/config` 写保护与密钥暴露、配额）。
4. P2 结构拆分（先测试后拆）。
5. 每批：`cargo test --locked --all-targets`、`cargo clippy --locked --all-targets -- -D warnings`、
   `cargo fmt --all --check`，更新 CHANGELOG 的 Unreleased 段与本文。
