# TODO

剩余问题按优先级排列；已完成项已删除（git 有记录）。

## P1 运行时

- [ ] `execute` 的前置早退（ai stopped / ESTOP / paused / daily-budget）仍可能在 receiver 关闭时丢消息；
      这些路径没有 drain `steering_rx`。正常回合并发已由 turn scope lock 串行化。
- [ ] swarm：`run_subagent` 在返回前已 drop receiver，`run_instance` 的 completion 锁拿得太晚。
      `message` 在 Running 但 send 失败时返回 `queued`；completion 随后置 Done，没有重启。
      需要 finishing 状态或 completion 后自动重启。
- [ ] 控制/monitor 注入回合绕过 ChatQueue；`is_idle` 对它们返回 true，`idle_only` 会先投递再等 turn lock。
      控制回合另有 4 个 semaphore，总主 agent 并发可超过 `queue.max_concurrent`。
- [ ] `agent.compact` 在 per_chat 下固定压缩 owner private 链（`compact_now` 硬编码 ChatId::Private(owner_qq)），
      从群回合调用会压错链；`clear_all_context` 无 scope 锁。
- [ ] per-scope 统计仍是全局：`compaction_failures` / `unchecked_tokens` / `review_failures` 会跨 chat 互相影响。

## P1 transcript / storage

- [ ] `rotate()` 在 UPDATE ended_at 与 create 之间不持锁；没有“每 scope 仅一个 open segment”的唯一索引，
      concurrent current/rotate 仍可能产生两个 open segment。
- [ ] `append_lock`/`current_lock` 被所有 scope 共享；`read_segment` 在全局锁内读完整段，慢分段会阻塞其他 chat。
- [ ] AAD 行号恢复限制为 1..=4096；更大偏移不可读，且任意候选削弱位置完整性。需要迁移/签名策略。
- [ ] 缺 transcript 偏移恢复、并发 append/read/rotate、双 open segment、wrong-key 的测试。

## P1 图片 / 视觉

- [ ] 真实 `get_image` 契约仍未实测：只兼容 url/file/base64；`path` 字段仍未解析，
      get_image 返回纯本地路径时会被丢弃；http/重定向最终 host/大小/图片数量仍未严格限制。
- [ ] 自动图片只在触发回合下载；daily/chat budget 早退时图片不会落库，之后只能手动 Download/Read。
      转发消息、回复引用图片、control/monitor 注入回合仍不自动注入。
- [ ] MCP 图片只做 MIME/大小过滤，不压缩；错误结果已保留 images。
- [ ] `media.image_watermark` 只支持 ASCII；`image_compress` 强制 JPEG；扩展名大小写已统一为小写。
- [ ] 缺 inbound resolver、provider image body、User image estimate/prune、MCP 图片、media 各动作的测试。

## P2 / 发布

- [ ] `queue.cap_batches` 作为 deprecated 字段仍可解析，不参与调度。
- [ ] 无 alpha.35 tag；按用户要求暂缓发布/部署，服务器 `sbot.service` 保持停止。
- [ ] 历史重写后其他 clone 必须重新 clone 或
      `git fetch --all --prune --tags --force && git reset --hard origin/main`；旧 SHA 已失效。
