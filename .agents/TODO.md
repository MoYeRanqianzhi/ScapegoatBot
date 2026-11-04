# TODO

剩余问题按优先级排列。已完成项已从本文件删除（git 有记录）。

## P1 运行时

- [ ] `ChatQueue` 的 `max_concurrent` 只限制 handler 派发；`Runner` 的全局 `turn_lock` 仍让主 agent 实际串行。
      需要决定：shared transcript 下明确“主回合串行”，还是改成 per-scope 锁并让 `max_concurrent` 真正生效。
- [ ] `idle_only` 只看 `active_turns`，不看 debounce 中尚未派发的批次；空闲判断可能过早。
- [ ] steering 尾部 drain 只覆盖“没有工具调用”的正常结束路径。
      Error / Refusal / Aborted / MaxTokens / wall-clock / token-budget 等提前返回仍可能丢消息；
      handler 返回后的收尾窗口也仍有 send 失败风险（已加回填 pending，但未覆盖所有路径）。
- [ ] swarm 完成路径在 `run_instance` 里清空 `incoming` 并写 status，不在 `dispatch_lock` 内；
      消息与完成并发时可能返回 `queued` 但不会自动启动。重启路径已加锁，完成路径仍需同样处理。

## P1 transcript / storage

- [ ] AAD 行号恢复只尝试 `1..=4096`；更大的旧偏移仍不可读。
- [ ] 尝试任意候选行号削弱了 AAD 对行位置的绑定；需要在“恢复旧数据”和“完整性”之间定策略。
- [ ] `current()` 没有 `append_lock`，`transcript_files` 也没有“每个 scope 只有一个 open 分段”的唯一约束；
      并发首次创建仍可能产生两个 active segment（turn_lock 下概率低）。
- [ ] 缺少 transcript 偏移恢复、并发 append、rotate 的回归测试。

## P1 图片 / 视觉

- [ ] `input: ["image"]` / `capabilities: ["vision"]` 仍未用于过滤附件；文本模型会收到 image_url。
      要么做 capability gate，要么把“无条件附加”写进配置文档并接受 400 风险。
- [ ] `get_image` 真实返回契约未实测：代码兼容 `url` / `file` / `base64`，但本机没有跑过真实 NapCat；
      `http` URL、重定向目标、`content-type` 为 `image/jpg`/octet-stream、`file` 只是容器本地路径等情况仍未覆盖。
- [ ] 自动图片只处理触发本回合的入站消息；后续回合/重启依赖已持久化的 image prompt。
      转发消息、回复引用图片、control/monitor 注入回合仍不会自动注入图片。
- [ ] MCP 错误结果仍丢弃 images；MCP 图片大小/MIME 只做了基础过滤，未做压缩。
- [ ] `media.image_watermark` 只支持 ASCII；中文会静默跳过并返回成功。
- [ ] `media.image_compress` 强制 JPEG 输出；`image_convert` 的未知扩展名/MIME 匹配仍需测试。
- [ ] 缺少 inbound resolver、domain/size 过滤、provider image 顺序、MCP 图片、crop/rotate/convert/thumbnail/compress 的测试。

## P2 其他

- [ ] `cap_batches` 作为 deprecated 字段仍被接受；旧配置可解析，但不再参与调度，文档已移除该行。
- [ ] 历史重写后其他 clone 必须重新 clone 或 `git fetch --all --prune --tags --force && git reset --hard origin/main`；
      旧 commit SHA 已失效。GitHub 不可达旧对象只能等服务端 GC。
- [ ] 无 alpha.35 tag；按用户要求暂缓发布/部署。服务器 `sbot.service` 保持停止。

## 验证缺口

- [ ] `event_audit` 只有存储 roundtrip 测试；`/system/events`、notice/request/meta/unknown 落库没有端到端测试。
- [ ] wake→ChatQueue、30s idle timeout、request wall-clock、控制回合上限、swarm 并发续聊没有专门测试。
- [ ] 真实 NapCat / provider 回归测试需要在部署后做；当前服务器 sbot 已停止。
