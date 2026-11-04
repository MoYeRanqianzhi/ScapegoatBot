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
- [ ] 处理 vendored `napcat-link` 剩余 17 条中文错误文本（`#[error]`、连接状态错误、配置校验），同步更新 `tests/error_test.rs` 断言。
- [x] 发布并部署 alpha.31：inbox-only + 唤醒预设 + notice/request 入库 + 图片 tool result/media.* + 回合互斥/transcript 行号。

## Agent 运行时问题

- [ ] provider 流式响应 `Network: error decoding response body` 反复出现；给流式读取加空闲超时，并让解码失败进入重试/回退，而不是拖到 wall-clock 600s。
- [ ] 控制回合出现 `elapsed=657` 与 `elapsed=1287` 两层预算告警，确认是否重复计时/嵌套重试。
- [ ] 运行时 `owner-private-wakeup` monitor 会对同一条私聊注入重复事件（transcript 出现两次 `嗯哼？`）；确认 preset 去重与 `batch_window` 语义。
- [ ] 会话压缩后旧行按新行号解密失败（AAD `session:<file_id>:<line_no>` 错位）；确认压缩是否重写/重排 transcript。

- [ ] 对照 NapCat/OneBot 事件目录做一次完整审计：notice 子类型（poke、recall、group_admin、group_upload、essence、ban、card、reaction 等）、request、meta lifecycle、未知事件；明确哪些进 inbox、哪些只审计。
- [ ] 决定 meta lifecycle / heartbeat 的落库策略；heartbeat 不能刷 inbox，但 connect/disable 等低频事件可能需要审计表。
- [ ] 未知事件没有 group_id/user_id 时目前丢弃；评估是否建 system/audit 表保留 raw。
- [ ] `/messages` 渲染目前只显示 plain_text；确认事件细节（raw/摘要）是否需要单独视图，避免上下文被 JSON 撑爆。

## pi/Claude Code 对照审计剩余项（alpha.31 之后）

- [ ] `ChatQueue` 已不在 live ingress；`idle_only`、`max_concurrent`、去抖配置实际失效。要么删除 queue 配置/文档，要么把 monitor 唤醒接回队列。
- [ ] `run_injected` 的 steering 尾部消息仍可能丢弃（当前 inbox 模式影响较小）。
- [ ] 图片附件仍不落 session：重启后文本还在、图片不在；需要持久化 VFS 引用或明确标注 turn-local。
- [ ] MCP image blocks 仍被转成 `[image content omitted]`。
- [ ] P2：重试无 jitter、无逐 chunk idle timeout、截断 tool args 以 `{}` 执行、`retention_days`/`monitor` 小竞态。
- [ ] 旧版本留下的 transcript 行号损坏无法自动修复；清空数据或迁移后才能消除 `skipping unreadable transcript line`。
