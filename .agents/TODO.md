# TODO

按优先级排列；完成后同步 `CHANGELOG.md` 与 `docs/guide/limits.md`。
已完成项直接删除（git 有记录）。

## 安全与设计对齐



## 配置与能力边界


## 运行与数据


## 工程


## 文档


## 待合并 / 待发布

- [x] 合并 `feat/i18n-logs`（`d90cec9`）：vendored `napcat-link` + 52 条英文日志 + `tools/check_log_i18n.py`。
- [x] vendored `napcat-link` 17 条中文错误文本已英文化（13b534b）。
- [x] 发布并部署 alpha.31：inbox-only + 唤醒预设 + notice/request 入库 + 图片 tool result/media.* + 回合互斥/transcript 行号。

## Agent 运行时问题

- [x] provider 流式响应加 30s 逐 chunk 空闲超时；解码失败走 Network 重试。
- [x] transcript append/read 串行化 + 行号信封；读取时尝试偏移行号恢复旧数据（alpha.34 待发）。


## pi/Claude Code 对照审计剩余项（alpha.31 之后）

- [x] monitor 唤醒携带原 QqMessage 重新走 ChatQueue；idle_only/并发/去抖恢复。
- [x] 图片附件随 transcript 持久化（alpha.32）。
- [x] MCP image blocks 进入 ToolResult.images。
- [x] 重试 jitter、逐 chunk idle timeout、截断 tool args 拒绝执行；其余小竞态仍在。

- [x] 上下文估算改用最近 assistant 的真实 usage；图片按 2000 tokens/张计入尾部（alpha.33）。

## 仍未完成（下一轮）

- [x] meta lifecycle / heartbeat 审计落库；heartbeat 跳过，`/system/events` 可读。
- [x] 无 chat 字段的 unknown 事件进入 event_audit。
- [x] `/system/events` 原始事件视图。
- [x] media.image_watermark / rotate / thumbnail / compress。

## 仍剩余（P2）

- [x] `prune_tool_results` 只裁剪请求副本，不改写已提交历史。
- [x] `Monitor fire` 限速时返回错误。

## 本轮收尾（alpha.35）

- [x] ChatQueue 唤醒接回、流式 idle timeout、旧 transcript 偏移恢复。
- [x] QQ 图片自动注入、Read 大图自动压缩、MCP 图片、media watermark/rotate/thumbnail/compress。
- [x] event_audit + `/system/events`，meta/unknown 落库，heartbeat 跳过。
- [x] i18n vendor 错误文本。
- [x] jitter/Retry-After、请求级 wall-clock、截断参数拒绝、runtime context 不落库、prune 请求副本、原子 create、monitor batch/fire、swarm 续聊、控制回合上限。
- [x] steering 尾部 drain。
- [ ] 暂缓：alpha.35 发布/部署（按要求先不部署，服务器 sbot 保持停止）。

