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
- [x] per_chat 的 append/current 锁已按 scope 独立；shared 仍共用。

## transcript

- [x] AAD 行号恢复上限 4096 已去掉，改为按文件行数扫描（279eedc）；位置完整性仍待签名/链式方案。
- [x] 已补 >4096 偏移恢复、并发 append/read/rotate、wrong-key 测试。

## 图片

- [ ] 真实 `get_image` 契约仍未实测；http/get_image 实际返回需启动 NapCat 验证。
- [ ] 转发/回复引用图片、control/monitor 注入回合仍不自动注入。
- [ ] `media.image_watermark` 只支持 ASCII。
- [x] `media.image_compress` 支持 JPEG/PNG/WebP（14f6a31）；WebP 仅无损，quality 只作用于 JPEG。
- [x] 已补 media 各动作测试；provider `image_url` 序列化已有覆盖。
- [ ] 缺 inbound resolver、User image estimate/prune、MCP 图片（已有压缩测试）的端到端测试。

## 结构

- [x] `src/app.rs` 拆为 `src/app/{mod,assembly,events,bootstrap}.rs`（45155bd）。
- [x] `src/agent/runner.rs` 已拆为 `runner/{scope,compaction,review,injected,images,evidence}.rs`（7de8177），
      `runner.rs` 从 3229 行降到 2295 行。
- [x] `src/config/sections.rs` 已拆为 `src/config/sections/` 19 个 section 文件（e0a4121）。

## P2 / 发布

- [ ] `queue.cap_batches` 作为 deprecated 字段仍可解析，不参与调度。
- [ ] `agent.compact` 没有目标参数，只能压当前回合所在链；control/monitor 回合固定在 owner private 链
      （`run_injected`），所以控制台里无法指定压缩某个群的上下文。
- [ ] 无 alpha.35 tag；按要求暂缓发布/部署，服务器 `sbot.service` 停止。
- [ ] 历史重写后其他 clone 必须重新 clone 或
      `git fetch --all --prune --tags --force && git reset --hard origin/main`；旧 SHA 失效。

## 架构审查（ba81e80）

- [x] ActionGate note_send 只保留 120s，导致 1h/10min 规则失效；改为保留 3600s（已修）。
- [x] TurnBuffer `Committing` 恢复：open_turns 纳入 committing，按已写前缀去重后补写（ce5971d）。
- [x] MessageStore 读路径：按文件批量解码，load_indexed/rebuild_index 不再逐行重读（本次）。
- [x] transcript 恢复上界改用 message_count；扫描候选改为物理位置 ±64 优先、每行上限 128（本次）。
- [x] steering 早退落库只写 transcript，由 pending_history 注入一次（8f92843）。
- [ ] 冗余大头：provider pump 外壳 180-220 行、5 份 complete_text 90-120 行、
      monitor 三组复制 55-75 行、session/messages 信封 60-80 行。
- [ ] 上帝对象残留：Runner 25 字段/1073 行、ApiTool、Assembly build_agent 18 参、
      Monitor 23 字段/add 17 参、VfsStore 39 方法；7 对模块 2-循环（providers→agent 最应优先解）。
