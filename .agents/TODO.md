# TODO

剩余问题；已完成项删除（git 有记录）。

## P1

- [ ] `execute` 的前置早退（ai stopped / ESTOP / paused / daily-budget）仍可能丢 `steering_rx` 中已发送的消息；
      正常回合已 drain local SteeringQueue 并提前关闭 receiver。
- [x] swarm 重启代数：旧 run 完成不会覆盖新状态；send 失败会走重启。
- [ ] 控制/monitor 回合现在计入 `Runner::is_busy`，但控制 semaphore 与 `queue.max_concurrent` 是两套上限，
      总主 agent 并发仍可超过配置值；`agent.compact` 在 per_chat 固定压 owner private 链。
- [ ] per-scope 统计仍是全局：`compaction_failures` / `unchecked_tokens` / `review_failures` 跨 chat 互相影响。
- [x] per_chat 的 append/current 锁已按 scope 独立；shared 仍共用。

## transcript

- [ ] AAD 行号恢复限制 1..=4096；更大偏移不可读，任意候选削弱位置完整性。需要迁移/签名策略。
- [ ] 缺 transcript 偏移恢复、并发 append/read/rotate、wrong-key 的测试。

## 图片

- [ ] 真实 `get_image` 契约仍未实测；重定向 host 与流式大小已校验，但 http/get_image 实际返回仍需实测。
- [x] daily/chat budget 早退时图片会持久化；转发/回复引用图片、control/monitor 注入回合仍不自动注入。
- [ ] MCP 图片只做 MIME/大小过滤，不压缩。
- [ ] `media.image_watermark` 只支持 ASCII；`image_compress` 强制 JPEG。
- [ ] 缺 inbound resolver、provider image body、User image estimate/prune、MCP 图片、media 各动作测试。

## P2 / 发布

- [ ] `queue.cap_batches` 作为 deprecated 字段仍可解析，不参与调度。
- [ ] 无 alpha.35 tag；按要求暂缓发布/部署，服务器 `sbot.service` 停止。
- [ ] 历史重写后其他 clone 必须重新 clone 或
      `git fetch --all --prune --tags --force && git reset --hard origin/main`；旧 SHA 失效。
