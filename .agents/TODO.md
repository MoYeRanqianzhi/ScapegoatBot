# TODO

剩余问题；已完成项删除（git 有记录）。

## 构建规则

- 子代理禁止运行任何 `cargo` 命令；构建只在主 checkout 用默认 `target/` 统一执行。
- 不使用 C: 盘放构建产物；G: 空间不足时先清 worktree target / incremental，再报告。

## P1

- [x] `execute` 前置早退丢 steering 消息：外层 close + 排空 + 落库，已加回归测试（c0fc680）。
- [x] 并发只作用于子代理（`[swarm].max_concurrency`）；主代理不加全局并发上限，
      已回滚共享 turn limiter（13aaebf），主代理回合仍由 session turn_lock 串行。
- [ ] 控制通道仍有写死的 4 槽位排队保护（`control_slots`）；它只是控制请求的准入保护，
      不是主代理并发上限，是否去掉待定。
- [x] per_chat 已彻底删除：`[session]` 配置、`SessionScope`、`ScopeCache`、`SessionStore.scope`/`for_scope`（2f2948c）。

## transcript

- [x] AAD 行号恢复上限 4096 已去掉，改为按文件行数扫描（279eedc）；位置完整性仍待签名/链式方案。
- [x] 已补 >4096 偏移恢复、并发 append/read/rotate、wrong-key 测试。

## 图片

- [ ] 真实 `get_image` 契约未实测：2026-11-06 直连验证时发现 NapCat 已退出登录（11-05 05:22 起持续 QR 登录失败），WS 握手被 reset；需用户扫码登录后重试。
- [ ] 转发/回复引用图片、control/monitor 注入回合仍不自动注入。
- [x] `media.image_watermark` 支持 font8x8 的 Unicode 字库（拉丁/希腊/平假名等）；CJK 仍无字形（需字体文件）。
- [x] `media.image_compress` 支持 JPEG/PNG/WebP（14f6a31）；WebP 仅无损，quality 只作用于 JPEG。
- [x] 已补 media 各动作测试；provider `image_url` 序列化已有覆盖。
- [ ] 缺 inbound resolver、User image estimate/prune、MCP 图片（已有压缩测试）的端到端测试。

## 结构

- [x] `src/app.rs` 拆为 `src/app/{mod,assembly,events,bootstrap}.rs`（45155bd）。
- [x] `src/agent/runner.rs` 已拆为 `runner/{scope,compaction,review,injected,images,evidence}.rs`（7de8177），
      `runner.rs` 从 3229 行降到 2295 行。
- [x] `src/config/sections.rs` 已拆为 `src/config/sections/` 19 个 section 文件（e0a4121）。

## P2 / 发布

- [x] `queue.cap_batches` 已删除（d807e4c）。
- [x] `agent.compact` 支持可选 `chat` 参数（2608061）。
- [x] 本地 tag `v0.2.0-alpha.35` 已创建；CHANGELOG 更新到 372 测试；本地 `cargo build --release` 通过（12 MB，`sbot --version` 正确）。
- [x] `RUSTDOCFLAGS=-D warnings cargo doc --no-deps` 通过（修复 3 处 intra-doc 链接，86ad96d）。
- [ ] push tag / GitHub release / 服务器部署需用户确认；服务器 `sbot.service` 仍停止。
- [ ] 历史重写后其他 clone 必须重新 clone 或
      `git fetch --all --prune --tags --force && git reset --hard origin/main`；旧 SHA 失效。

## 架构审查（ba81e80）

- [x] ActionGate note_send 只保留 120s，导致 1h/10min 规则失效；改为保留 3600s（已修）。
- [x] TurnBuffer `Committing` 恢复：open_turns 纳入 committing，按已写前缀去重后补写（ce5971d）。
- [x] MessageStore 读路径：按文件批量解码，load_indexed/rebuild_index 不再逐行重读（本次）。
- [x] transcript 恢复上界改用 message_count；扫描候选改为物理位置 ±64 优先、每行上限 128（本次）。
- [x] steering 早退落库只写 transcript，由 pending_history 注入一次（8f92843）。
- [x] provider `HttpProviderCore`/`pump` 外壳合并；5 份 `complete_text`/`truncate_chars` 合并（e41f5a8）。
- [x] monitor lookup/rate_limit/system_delivery 三组复制已收敛（5b4ca65）。
- [x] session/messages 加密信封已抽 `storage/envelope.rs`（6d48111）。
- [x] `build_agent` 18 参改 `AgentInputs`；`MonitorManager::add` 16 参改 `MonitorSpec`（16e97b8/44e7f6e）。
- [x] providers→agent 循环依赖已解：`Usage/StopReason/RefusalDetails` 移到 `src/llm/types.rs`（2a0f851）。
- [x] 已解：providers→agent、session→agent、security→agent、swarm→agent（消息模型/证据移到 `src/transcript.rs`）。
- [x] 已解：agent↔tools（`ExecutionMode`/`ToolExecutor` 移到 `tools`，`hooks` 移到 crate 根）。
- [x] 已解：agent↔memory（`ProviderSlot` 移到 `providers`，memory 用 `QueueActivity` 注入队列活跃度）。
- [x] 已解：security↔tools（action 元数据移到 crate 根 `actions`，`chat_of`/`qq_number` 随迁）。
- [x] 所有模块 2-循环已清零。
- [x] ApiTool `execute` 拆出 `api/policy.rs`、媒体拆出 `api/media.rs`；VfsStore 拆出 `sqlite/{codec,tree}.rs`（2608061/1f0106d）。
- [x] `actions.rs` 已按 namespace 拆成 9 个 helper；Box::leak 清零（15757a1、7026425）。
- [x] `run_turn` 已拆为 `drive_turn`/`commit_turn`（155+83 行）。
- [x] MessageStore 已拆为 `messages/{codec,index,outbox,retention}.rs`（960 行主文件）。
- [x] Runner 的 9 个存储/运行时字段收进 `RunnerDeps`，字段数 25→15（22ea723）。
- [x] 内部模块 `ipc/mcp/monitor/paths/prompt/transcript/llm` 改为 `pub(crate)`（ed88080）。
- [x] storage/session 的 `pub fn` 已降为 `pub(crate)`，全仓 `pub fn` 381→332（48ed2b2）。
- [x] agent/security 方法已降为 `pub(crate)`，只为外部测试保留 `ActionGate/ApprovalQueue/JevJudge` 构造器；全仓 `pub fn` 332→287（0eedbca）。
- [x] `tools` 方法已降为 `pub(crate)`，只为集成测试保留 `ApiTool::new/with_action_gate`、`ToolContext::new`；全仓 `pub fn` 287→246（5ce2684）。
- [x] `#[allow(dead_code)]` 清零（10→0），删除 `Secret::raw` 与 `SbotPaths` 四个未用路径 helper（6b3ed9c）。
- [x] hold 语义修正：只拒绝 QQ 操作（`napcat.*`），MCP/下载/Web 不再被 hold 拦截（d21eed4）。
