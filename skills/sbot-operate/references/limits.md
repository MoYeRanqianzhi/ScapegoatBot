# 当前版本的限制

这里列出**明确未实现或刻意不支持**的项。它们要么只被解析、不生效，要么是
已知的设计边界；不要把它们当作已有防护。

## 配置语义

- **`[budget]`**：每日 token / USD 在回合开始时检查，超了跳过回合（消息已入库），计数按本地
  日期存在数据库里，重启不清零。单回合 token / 墙钟是硬停。每 chat 的小时限额
  （`chat_turns_per_hour`、`chat_sends_per_hour`）只在内存里计数，重启清零；发送限额只统计
  成功的发送。`guard_usd_per_day` 是估算值（Jev 固定单次成本，LLM 按约 3k 输入 + 100 输出
  估算），耗尽后分类器降级为规则层，规则仍生效。
- **`[hooks.events]`**：每个事件只支持固定的几种动作（见
  `sbot docs sbot-deploy configuration` 的表格），其余组合启动时报错；同一个事件最多 16 条。
  `block` 只在 `message_received` 与 `before_send` 上有意义。`session_end` 在清空上下文
  （`agent.clear_context`）与进程退出时触发；退出时最多等 5 秒，`notify` 需要 NapCat 仍在线。
- **`[tools].max_schema_tokens`**：超限时把过长描述压到 160 字符；若仅 schema 就超限，
  只告警、不删工具。
- **monitor 的 `cc` / `on_missing`**：`cc` 在投给子代理时抄送主代理；`on_missing`
  支持 `drop`、`queue`（最多等 60 秒）与 `spawn`（新建 general 子代理处理）。
- **`[agent].compaction_model`**：非空时用于压缩摘要；空 = 主模型（默认）。

## 安全边界

- **`approve` 的确认码**：只在 `master_qq` 是另一个账号时发出，只接受该账号或 owner
  从**另一个账号**发来的私聊回复；`master_qq` 就是机器人账号时只能用 `sbot gate`。
  队列持久化只保存 action、摘要、参数哈希与确认码，不保存消息原文。确认码是 6 位数字，
  由时间与进程号派生，不是密码学令牌，也没有尝试次数限制：靠的是 60 秒的有效期，以及
  “只有主人能从自己的账号私聊机器人”这一边界。
- **`auto` 的发送规则**：频率、同 chat 重复、跨群同文、@全体、跨 chat 5-gram
  重叠（≥24 字、共享 5 个 5-gram）、上下文来源与媒体来源都会拦。`context_sources`
  覆盖本轮消息、pending history、`Read /messages|/inbox` 与 `API` 历史查询；
  `Grep` 结果不按来源拆分。规则是启发式，可能误报长文本的合理转述。
- **周期检查的覆盖范围**：只看最近一段上下文（约 4.8 万字符加重叠部分，最新的内容总在
  里面）；两次检查之间进出上下文的内容比这个窗口长时，较早的部分不会被看到。检查超时或
  失败按放行处理，那一段不补查。触发检查用的 token 计数是估算值。
- **WebFetch 后端模式**：初始 URL 与返回的 `final_url` 都会在本地校验，但抓取与
  重定向仍由你配置的后端执行；需要本地逐跳校验时设 `[tools.web_fetch].local = true`。
- **加密边界**：防的是拿到 `data.db` / 文件但没有 `key`；不防同机管理员、
  进程内存读取，也不防拿到备份（备份含 key）。`[storage].encryption = "none"` 期间写下的
  明文行，改回加密之后不再读取（见 `sbot docs sbot-operate troubleshooting` 的“历史读不出来”）。
- **`receive_scopes` 的边界**：`get_msg` 会通过消息索引反查 chat；查不到且在限制读取时
  保守拒绝。
- **VFS 不是沙箱**：它隔离的是模型与宿主文件系统，不隔离网络。

## 协议与接入

- 只支持 OneBot 11（通过 NapCat WebSocket）；没有官方 QQ 协议实现。
- 单实例：同一 `SBOT_HOME` 同时只能跑一个进程。
- `start` 是前台进程；systemd / Windows 服务需要你自己包一层，并且要配自动重启：程序内部
  出错（panic）时进程直接退出而不是带着坏掉的后台任务继续跑，靠服务管理器把它拉起来。中断的
  回合里已经产生的内容在启动时并回对话记录，回合本身不会重跑。systemd 用
  `Restart=on-failure`（`sbot stop` 的退出码是 0，不会被重新拉起）。
- MCP 只有 stdio 传输、一个聚合 `MCP` 工具；不支持 HTTP/SSE 传输；
  `tools/list` 分页最多取 10 页，超过会告警并停止发现剩余工具。
- 不迁移 0.1 数据；0.2 从空存储开始。

## 已知行为细节

- 只有一条 transcript：跨 chat 上下文互相可见，这是替身场景的设计。
- 子代理嵌套受 `max_depth`（默认 8）限制。子代理的写保护按路径前缀判断（见
  `sbot docs sbot-use tools-and-vfs` 的“子代理”），它仍能写其他任何普通路径。投给子代理的
  monitor（`deliver.to = "<id>"`）不会随子代理结束删除（结束的子代理仍可续聊），由
  `expires` / `max_events` 收尾。
- 压缩失败连续达到 `[compaction].max_failures` 后，自动压缩停止尝试，直到重启、一次手动
  `agent.compact` 成功，或 `agent.clear_context` 清空上下文。
- `[compaction].post_compact_recall` 注入的是 `/MEMORY.md` 索引，不是按语义检索
  的相关条目；预算 8000 字符。
- `[memory].review = every_turn` 实际是每 5 秒轮询一次并检查 transcript 是否有新增，
  不是严格“回合结束即触发”。
- `[monitor.lifecycle].catch_up` 只支持 `coalesce` 与 `skip`；补投只针对配置文件里的
  定时源（`Monitor add` 不接受 `catch_up`：运行时 monitor 重启即消失）。`coalesce` 不论错过
  多少次都只补投一次，事件里写明错过的次数（最多计到 1000）。
- `sbot doctor` 是只读的：它发现的问题需要你按提示手工修复。它不检查 MCP 服务器，也不连
  NapCat。
- MCP 服务器进程的 stderr 不进 agent 的上下文，只进 sbot 的日志：平时每行一条 `debug`
  （`MCP stderr: …`，要 `RUST_LOG=debug` 才看得到），服务器启动失败时最后 20 行按 `warn`
  写出。每行最多留 4 KiB。
- 文件头损坏的消息文件（`message file has no usable header`）不会进索引，启动时的
  文件数校验因此每次都不一致，每次启动都会重建一遍索引；把这个文件移出
  `~/.sbot/data/messages/`（或从备份恢复）即可停止。
