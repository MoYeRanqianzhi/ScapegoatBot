# TODO

按优先级排列；完成后同步 `CHANGELOG.md` 与 `docs/guide/limits.md`。
已完成项直接删除（git 有记录）。

## 安全与设计对齐



## 配置与能力边界


## 运行与数据


## 工程


## 文档

- [ ] 真实 API / NapCat 实测记录更新到 alpha.14（当前 live-testing 停在 alpha.12）。
- [ ] 若实现上述任一项，先改 `docs/guide` 对应页，再删 `limits.md` 对应行。

## 待合并 / 待发布

- [x] 合并 `feat/i18n-logs`（`d90cec9`）：vendored `napcat-link` + 52 条英文日志 + `tools/check_log_i18n.py`。
- [x] vendored `napcat-link` 17 条中文错误文本已英文化（13b534b）。
- [x] 发布并部署 alpha.31：inbox-only + 唤醒预设 + notice/request 入库 + 图片 tool result/media.* + 回合互斥/transcript 行号。

## Agent 运行时问题

- [x] provider 流式响应加 30s 逐 chunk 空闲超时；解码失败走 Network 重试。
- [ ] 控制回合出现 `elapsed=657` 与 `elapsed=1287` 两层预算告警，确认是否重复计时/嵌套重试。
- [ ] 运行时 `owner-private-wakeup` monitor 会对同一条私聊注入重复事件（transcript 出现两次 `嗯哼？`）；确认 preset 去重与 `batch_window` 语义。
- [x] transcript append/read 串行化 + 行号信封；读取时尝试偏移行号恢复旧数据（alpha.34 待发）。

- [ ] 对照 NapCat/OneBot 事件目录做一次完整审计：notice 子类型（poke、recall、group_admin、group_upload、essence、ban、card、reaction 等）、request、meta lifecycle、未知事件；明确哪些进 inbox、哪些只审计。
- [ ] 决定 meta lifecycle / heartbeat 的落库策略；heartbeat 不能刷 inbox，但 connect/disable 等低频事件可能需要审计表。
- [ ] 未知事件没有 group_id/user_id 时目前丢弃；评估是否建 system/audit 表保留 raw。
- [ ] `/messages` 渲染目前只显示 plain_text；确认事件细节（raw/摘要）是否需要单独视图，避免上下文被 JSON 撑爆。

## pi/Claude Code 对照审计剩余项（alpha.31 之后）

- [x] monitor 唤醒携带原 QqMessage 重新走 ChatQueue；idle_only/并发/去抖恢复。
- [ ] `run_injected` 的 steering 尾部消息仍可能丢弃（当前 inbox 模式影响较小）。
- [x] 图片附件随 transcript 持久化（alpha.32）。
- [x] MCP image blocks 进入 ToolResult.images。
- [x] 重试 jitter、逐 chunk idle timeout、截断 tool args 拒绝执行；其余小竞态仍在。
- [ ] 旧版本留下的 transcript 行号损坏无法自动修复；清空数据或迁移后才能消除 `skipping unreadable transcript line`。

- [x] 上下文估算改用最近 assistant 的真实 usage；图片按 2000 tokens/张计入尾部（alpha.33）。

## 仍未完成（下一轮）

- [x] meta lifecycle / heartbeat 审计落库；heartbeat 跳过，`/system/events` 可读。
- [x] 无 chat 字段的 unknown 事件进入 event_audit。
- [x] `/system/events` 原始事件视图。
- [x] media.image_watermark / rotate / thumbnail / compress。
- [ ] runtime context 每回合持久化、prune 修改 in-flight working copy、cap_batches 不可达、swarm/monitor 小竞态。
- [ ] alpha.34 尚未发布/部署；服务器 sbot 已按要求停止。

## 仍剩余（P2）

- [ ] `prune_tool_results` 修改 in-flight working copy，与已提交 transcript 可能不一致。
- [ ] `Monitor fire` 被 min_interval 限速时仍返回成功。
- [ ] swarm 重启竞态；控制回合无显式并发上限（turn_lock 串行但未限制排队数量）。
- [ ] 图片自动下载的 QQ 域名白名单需要随 NapCat 实际 CDN 复核。
