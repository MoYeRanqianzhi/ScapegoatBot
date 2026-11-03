# 当前版本的限制

这里列出**明确未实现或刻意不支持**的项。它们要么只被解析、不生效，要么是
已知的设计边界；不要把它们当作已有防护。

## 配置已解析但未生效

| 配置 | 现状 | 影响 |
|------|------|------|
| `[gate].mode`（trusted/auto/approve） | 未实现 | API 调用没有逐动作审批 |
| `[gate].max_per_minute` | 未实现 | 没有发送/调用频率上限 |
| `[gate].llm_review` | 未实现 | 不会对每次发送追加 LLM 审查 |
| `[security].check_every_tokens` / `check_on_compaction` / `check_overlap_tokens` / `review_model` / `check_timeout_secs` | 未实现 | 没有周期性上下文安全检查 |
| `[security.guard].max_state_tokens` / `delta_every_tokens` | 预留 | 闸使用固定窗口 |
| `[security.guard].backend = "llm"` | 未实现 | 配置校验直接拒绝 |
| `[compaction].reserve_tokens` | 预留 | 预算只用 `turn_context_budget` |
| `[compaction].prune*` | 未实现 | 旧工具结果不会被自动裁剪 |
| `[compaction].memory_flush` / `post_compact_recall` | 未实现 | 压缩前后没有记忆刷写/召回 |
| `[agent].compaction_model` | 未实现 | 摘要使用当前主模型 |
| `[memory].idle_minutes` | 未实现 | 复盘按固定间隔运行 |
| `[memory].review = every_turn` | 等价于 `idle` | 不是逐回合触发 |
| `[style].sample_retention_days` | 未实现 | 原始样本不按天数清理 |
| `[monitor.lifecycle].catch_up` | 未实现 | 停机期间的事件不做补投 |
| `models.json` 的 `default_thinking_level` | 未实现 | effort 必须显式配置 |
| `api.*` 的 `requires_owner_intent` | 仅元数据 | 不构成发送前审批 |

## 安全边界

- **WebFetch 的重定向**：初始 URL 在本地校验（协议、域名、公网地址），实际抓取与
  重定向由你配置的后端执行，本地不做逐跳复核。不要用不可信的后端。
- **DNS rebinding**：`Download` 逐跳校验解析结果，但解析与连接由 reqwest 分别进行，
  存在 TOCTOU 窗口。高风险环境请配合网络层出口策略。
- **group 级破坏动作**：全员禁言、退群、解散群没有用户目标，不做 owner 保护；
  只由 `destructive` 标记与提示词约束。
- **加密边界**：防的是拿到 `data.db` / 文件但没有 `key`；不防同机管理员、
  进程内存读取，也不防拿到备份（备份含 key）。
- **VFS 不是沙箱**：它隔离的是模型与宿主文件系统，不隔离网络。

## 协议与接入

- 只支持 OneBot 11（通过 NapCat WebSocket）；没有官方 QQ 协议实现。
- 单实例：同一 `SBOT_HOME` 同时只能跑一个进程。
- `start` 是前台进程；systemd / Windows 服务需要你自己包一层。
- MCP 只有 stdio 传输、一个聚合 `MCP` 工具；不支持 HTTP/SSE 传输。
- 不迁移 0.1 数据；0.2 从空存储开始。

## 已知行为细节

- `[session].scope = shared` 时跨 chat 上下文互相可见，这是替身场景的默认设计；
  需要隔离用 `per_chat`。
- 子代理嵌套受 `max_depth`（默认 8）限制；`monitor` 绑定随子代理结束删除。
- MCP `tools/list` 分页最多取 10 页；超过会静默截断。
- `Agent message` 发给正在运行的子代理会排入其历史，但当前回合不会中途读取，
  要等下一次运行。
- 压缩失败连续达到 `[compaction].max_failures` 后，自动压缩停止尝试，直到重启或
  一次手动 `agent.compact` 成功。
- `sbot doctor` 是只读的：它发现的问题需要你按提示手工修复。

## 测试相关

- 真实 API / NapCat 测试默认 `#[ignore]`，需要环境变量显式启用
  （见 [live-testing.md](../development/live-testing.md)）。
- 仓库没有 CI；`cargo test --all-targets` 与
  `cargo clippy --all-targets -- -D warnings` 需要本地执行。
