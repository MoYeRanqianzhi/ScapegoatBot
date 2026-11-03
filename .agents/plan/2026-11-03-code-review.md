# 0.2 代码审查（2026-11-03）

> 对象：`main` @ `4641eee`（`v0.2.0-alpha.12`，31,100 行 / 62 文件）。
> 方法：通读源码 + 四个分区并行审查 + 本机 `cargo check` / `cargo clippy` / `cargo test`。
> 本文只记录**已核对**的结论；标注「推测」的条目还需要实测或进一步反证。
>
> **修复状态（2026-11-04，`v0.2.0-alpha.14`）**：P0 与 P1 主体已修复并有回归测试；
> alpha.13 之后补齐了门禁、owner intent、llm 分类器、周期检查、压缩调优、
> monitor 补投与网络加固，结构拆分与文档同步完成。仍存在的边界见
> [`.agents/TODO.md`](../TODO.md) 与 `docs/guide/limits.md`；本文保留为修复前的快照，
> 不要据此认为对应问题仍然存在。

## 验证状态

- `cargo check --all-targets`：通过。
- `cargo clippy --all-targets`：通过（0 warning）。
- `cargo test --all-targets`：**254 passed / 1 failed**。
  - `storage::messages::tests::retention_never_removes_current_month` 失败：测试夹具把时间硬编码为 2026-10-29（`src/storage/messages.rs:936`），当前月份是 2026-11，于是「当前月」被当成旧月份删掉。是测试的时间炸弹，不是保留策略的生产 bug；但 README 把这条命令写成验收标准，红着就不能算通过。
- `Cargo.lock` 中 `scapegoat-bot` 版本仍是 `0.2.0-alpha.11`，与 `Cargo.toml` 的 `alpha.12` 不一致；任何 `cargo` 命令都会把它改脏。
- 无 `.github/`，README 里的 test/clippy 命令没有任何自动化执行。

## P0（会造成数据损坏、崩溃卡死或安全承诺失效）

1. **自动压缩后切片越界 panic，chat 永久卡死**
   - 证据：`src/agent/runner.rs:627` 先取 `let base = working.len();`；`:630-639` 压缩成功后 `working = history.lock().await.clone();`（`compact_scope` 在 `:431` 把历史替换成「摘要 + keep」，长度必然小于 base）；`:703` 仍用 `working[base..]`。
   - 触发：`estimate_messages > turn_context_budget`（默认 120,000）且 `split_for_compaction` 分出非空旧消息——即真正的自动压缩。
   - 后果：panic 发生在 `queue.rs` 的 spawn 任务里，`:212-224` 的清理不执行，`state.active` 永远是 `Some`，该 chat 之后不再起回合；ingest 的 steering 发送失败，消息静默丢失。
   - 证据类型：静态分析（未跑出 panic 的集成用例；现有测试都不触发自动压缩）。

2. **消息 JSONL 与索引非原子 + 并发 append 竞争 → 静默读到错消息**
   - 证据：`src/storage/messages.rs:143-153` 用 `1 + SUM(count)` 取行号，锁在取号后释放；`:167-184` 才写文件，`:203-218` 最后提交 `messages`/`message_files`。`read_line`（`:765-817`）只按请求行号取行，AAD 用「行内 id + 行号」，返回前不校验 id。
   - 触发：崩溃/事务失败留下未计数的行，或两个 append 并发（入站管线 `app.rs:1109` 与 agent 回合里的出站登记 `tools/api.rs:388` 可以同时发生；`[queue].max_concurrent` 默认 4）。
   - 后果：同一行号被复用，`load(id2)` 能正确解密 id1 的行并返回，静默错误且 id2 不可达。
   - 修复方向：行号分配与写入放进同一把锁/事务；读后校验行内 id == 请求 id。

3. **`[session].scope = per_chat` 的崩溃恢复串链**
   - 证据：`src/agent/runner.rs:178-188` 对每个 scope 调 `recover_open(&scoped)`；`src/session/turn_buffer.rs:150-160` 的 `open_turns()` 查的是全表（表无 scope 列，`storage/mod.rs:226-240`）。
   - 后果：第一个被处理的 scope 会吞掉所有 chat 的 open 回合（跨 chat 污染），其余 scope 什么也拿不到；若某个 chat 还没有已提交分段，`scopes()` 里没有它，崩溃回合永远不恢复。README:109 声称「崩溃恢复按 chat 执行」。

4. **SSE 流按 chunk 做有损 UTF-8 解码，静默损坏中文**
   - 证据：`providers/anthropic.rs:457`、`providers/chat.rs:365`、`providers/openai.rs:392` 都是 `String::from_utf8_lossy(&bytes)` 后再喂给 SSE 解析器。
   - 触发：多字节字符恰好跨 TCP chunk（中文回复很常见）。
   - 后果：U+FFFD 写进 transcript 并可能发给用户，JSON 仍合法、无告警。

5. **安全闸默认不拦，且「分类器不可用」与「已恢复」语义都靠不住**
   - 默认 `mode = shadow`（`config/mod.rs:1654-1666`），命中只审计；README:101 却直接写「命中后写 `SECURITY_HOLD`」。
   - `backend = "llm"` 在 `gate.rs:322-328` 根本没有实现，静默降级成 rules-only 并放行；jev 缺 base_url/key 时同样 `judge=None`（`gate.rs:404-437`）。
   - `agent.resume`（`runner.rs:869-899`）用固定串 `"resume request"` 作为 delta 过闸，而 RuleJudge 只看 delta（`gate.rs:118-131`）：只要没有分类器，任何 hold（包括 refusal 强置的）都能被模型自己一次调清。
   - `api.rs:256-258` 描述 "Every napcat.* call passes the gate"，实际只检查 `hold_active()`（`:325`）；hold 也不覆盖 MCP / Download / WebFetch。
   - `gate.rs:442-447` 的「按阈值收紧」分支不可达：上一行已把 `dangerous` 收敛为 `score >= threshold`。

## P1（功能或安全缺陷，可控但应修）

6. **全局 states 锁跨 semaphore await**：`agent/queue.rs:160-195` 在持锁时 `acquire_owned().await`。并发打满时所有 chat 的 ingest/status 一起卡住，与「每 chat 独立计时」的模块注释矛盾；`:186-190` 的 `cap_batches` 分支注释说丢一个批次，实际只 `pop_front` 一条消息。
7. **准入过滤被 monitor 绕过**：`app.rs:1102-1109` 先 `monitors.on_qq_event` 再 `filter_and_count`；monitor 投递走 `run_injected`（`runner.rs:493`），不看 FilterVerdict。模型自建 `qq` monitor 后，被拒消息原文仍进主上下文，直接违背 README「被拒消息不写 JSONL、不进 inbox、不进 transcript」。
8. **`Copy` 绕过所有配额**：`vfs/sqlite.rs:1069-1088` 与 `:1184+` 没有 `check_growth` / `check_disk`，而 `write_text`/`write_binary` 都有（`:512-513`、`:828`）。重复复制大 blob 可突破 10 GB / 节点数 / 磁盘水位。
9. **两个 action 永远失败**：`delete_msg`、`set_msg_emoji_like` 是 `Category::Message` 但 `scope_field: None`（`api.rs:598-633`），执行前 `chat_of` 必返回 None（`:336-348`）→ 恒报 "requires a chat target"。`send_msg` 的 `scope_field` 固定为 `user_id`，按群发送同样被拒。
10. **owner 保护有缺口**：`target_qq` 只做 `Value::as_u64`（`api.rs:184-188`），字符串形式的 `user_id` 直接跳过保护；`set_group_whole_ban` / `set_group_dismiss` / `set_group_leave` 没有 `user_id` 参数（`:771-800`），恒不拦。`requires_owner_intent` 只出现在 `api.help` 输出（`:483`），从无执行点。
11. **Error / Refusal 路径留下未配对的 tool_use**：`agent/loop.rs:313-342` 直接 push assistant 并返回，若流里已经出现过 tool call，下一轮请求只有 tool_use 没有 tool_result（Anthropic 400）。
12. **截断的工具参数写回历史**：`loop.rs:661-673` 解析失败时 `unwrap_or(Value::Null)`；max_tokens 路径会把 `input: null` 的 tool_use 提交进 transcript（`:344-360`），后续请求可能永久 400。
13. **索引重建不完整**：`messages.rs:521-600` 只补 `messages`，不补 `message_files` / `inbox_state`；`usage`、保留策略、月视图、未读数全依赖后两张表。保留清理（`:653-658`）又只删前两张表，`inbox_state` 留下孤儿，未读数永久虚高。
14. **改加密模式后历史不可读**：文件头写了 `"encryption"`（`messages.rs:704-707`），读取却按当前配置 `self.encryption` 解密（`:802`）。`none ↔ xchacha` 之后旧 JSONL 全部失败被 warn 跳过。
15. **WebFetch 的 SSRF 承诺名不副实**：`tools/web.rs:176-215` 只校验初始 URL 并本地解析一次，实际抓取与重定向交给远程后端；工具 description 却写 "Enforces SSRF protection and redirect limits"。Download 有逐跳检查（`net.rs:111-145`），但仍然是「先解析、后由 reqwest 重新解析连接」的 TOCTOU（DNS rebinding，推测可利用）。`is_public_ip` 的 V6 分支不识别 IPv4-mapped（`net.rs:155-178`）；字面量 `http://[::ffff:127.0.0.1]` 因 `url::Url::host_str` 带方括号而解析失败（已核对 url 2.5.8 文档），所以那条具体路径不成立，但 DNS 返回 `::ffff:127.0.0.1` 仍会被判为公网。
16. **MCP stdio 无读取上限**：`mcp/mod.rs:117-131` 用无长度限制的 `read_line` 循环，只有 30s 总超时（`:145`）；异常 server 可以在这段时间内撑爆内存。
17. **只读子代理能发送**：`swarm/mod.rs:66-69` 给 `docs` 类型硬编码 `API`，`:390-399` 里自定义定义写 `tools: API` 也能拿到，`allow_send: false` 不会移除；`docs` 的提示词却写着 "Do not send anything"（`:109-113`）。
18. **Grep 把没匹配的消息也标成已读**：`tools/views.rs:333-367` 对每个候选文件调 `self.read()`，`read` 在 `:453` 对整窗消息 `mark_read`，之后才按正则过滤行。`Grep /inbox` 会静默清空未读，与 `fs.rs:386` 的 "files only scanned are not marked read" 相反。
19. **压缩配置与重建承诺没落地**：`config/mod.rs:1850-1872` 的 `reserve_tokens`、`max_failures`、`memory_flush`、`prune*` 全仓库没有读取点；`prompt-maintenance.md:29,61` 说「压缩成功后重建 system prompt」，实际 `runner.rs:200` 只在构造时 `load_hooks`。
20. **`run_injected` 没有预算与压缩**：`runner.rs:493-553` 与 `execute` 相比少了自动压缩；长控制/monitor 回合会无限增长直到超出模型窗口。

## 上帝结构

- `app.rs:621` `build_agent` 365 行、16 个参数，装配 provider/回退链/工具/视图/子代理/monitor/热更新；`Runtime::start`（`:151`）175 行，一条函数从配置加载走到 QQ 管线启动。
- `agent/runner.rs:86-114` `Runner` 21 个字段，同时持 provider 槽、回退链、QQ、VFS、Gate、三套存储、per_chat 缓存与压缩锁；`execute`（`:584-725`）混合入库、游标、压缩、提示、日志、buffer 提交、已读；`run_with_steering`（`loop.rs:191-416`）226 行、7 处重复的 outcome+early return；`AgentLoop` 构造在 `execute` / `run_injected` / `run_subagent` 复制三份。
- `config/mod.rs` 2,368 行的单文件，`SbotConfig` 21 字段 + 18 路 validate 分发；`EventHookConfig` 13 字段中 8 个按 action 互斥，只能事后校验。
- `tools/api.rs:558-978` `specs()` 约 420 行，40 个 action 的 schema/示例/分类/`Box::leak` 堆在一处；`tools/views.rs` 1,608 行同时做 JSONL/SQLite 读取、NapCat 实时查询、渲染、配置落盘，`read` 二十多个分支。
- `monitor/mod.rs:191-245` `add` 13 个位置参数。

## 注释与代码不一致

- `lib.rs:7,21,24,27,31,34,43` 仍写「Phase 3/4/5 才接入」，实际 Phase 0–7 已完成。
- `tools/mod.rs:8` 说 `Permission` 是给「未来的策略表」的声明；全仓库没有任何 `.permissions()` 消费点，整套权限声明是死元数据。
- `compaction.rs:6` 说「最终以 provider usage 校准」，没有校准代码；`loop.rs:70` 说 `max_state_chars` 单位是字符，`render_state` 用字节（`:892`）。
- `agent/event.rs:37` `TurnEnd` 从未被发送；`resync.rs:9` 说写 `[gap]` 标记，实际只有 debug 日志。
- `vfs/mod.rs:104` 说 `removed_nodes` 是受影响节点数，软删子树返回 0；`path.rs:8` 声称全路径 ≤255，`join()` 不校验长度。
- `messages.rs:167` 说崩溃后 `rebuild_index` 能补回来，`message_files.count` 补不回来。

## 文档与代码不一致

- README:12 与 README:111、CHANGELOG:25 与 :44 对「子代理能否嵌套」互相矛盾；代码已支持（`runner.rs:750-760`）。
- README:75 的「最小可用配置」写了 `[napcat].account_qq`，`NapcatConfig` 只有 `url` 且 `deny_unknown_fields`（`config/mod.rs:660-662`）→ 照抄直接启动失败。
- 根目录 `config.example.toml` / `llm.example.toml` 仍是 0.1 schema（`[whitelist]`、`[llm]`、`[[models]]`、内联 api_key），refactor-report:98 说它们「仅供历史对照」，但文件本身没有标注，放在根目录仍会被当成模板。
- CHANGELOG:12「重写 78.2% / 废弃 21.8%」 vs refactor-report:12-13「97.5% / 2.5%」。
- CHANGELOG:17、docs/development/README:27「迁移 v1–v6」 vs 实际 v8（`storage/mod.rs:29`，live-testing 也记 8）。
- README:11「11 个文件工具（…Glob/Grep）」只列了 10 个，漏 `LS`。
- README:112 说 `[compaction]` 可写，但其中多个键没有读取点（见 P1-19）。

## 修复顺序建议

1. P0-1（压缩后重算 base）——一行修复，先止血。
2. P0-2（append 串行化 + 读后校验 id）与 P0-3（turn_buffer 带 scope）——数据正确性。
3. P0-4（引入增量 UTF-8 解码器）。
4. P0-5（明确「无分类器 = 不安全」或启动拒绝；resume 用真实被判内容重过闸；hold 提升为工具层统一前置检查）。
5. P1-6/7/8/9/10/11/12/13/14/18（并发、过滤、配额、action、配对、索引、加密、已读）。
6. 文档批量校正：README、CHANGELOG、docs/development、示例配置；删除或明确标注根目录旧示例。
7. 修测试时间炸弹，补自动压缩、per_chat 恢复、并发 append 的回归测试；把 `cargo test` / clippy 接到 CI。

## 误报排除（已核对）

- `sbot backup` / `doctor` 不走 IPC 是刻意设计（`cli/mod.rs:133-139` 有注释），README:18 的措辞不严谨，但不是实现缺陷。
- `http://[::ffff:127.0.0.1]/` 字面量 SSRF 不成立：`url` crate 的 `host_str()` 对 IPv6 带方括号，`parse::<IpAddr>()` 会失败并落到 DNS 查询（已核对 url 2.5.8 源码文档）。
- 默认 `guard.mode = shadow` 本身是设计选择（`config/mod.rs:1654-1666`），问题在于文档与默认值的表述不一致，而不是代码 bug。
