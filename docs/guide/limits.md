# 当前版本的限制

这里列出**明确未实现或刻意不支持**的项。它们要么只被解析、不生效，要么是
已知的设计边界；不要把它们当作已有防护。

## 仍未生效的配置

| 配置 | 现状 | 影响 |
|------|------|------|
| `[agent].compaction_model` | 仅在非空时用于摘要 | 空 = 主模型（默认） |

## 安全边界

- **`approve` 的 nonce**：通过 master 私聊发送 6 位码，只接受 owner/master 私聊回复；
  队列持久化只保存 action、摘要、参数哈希与 nonce，不保存消息原文。nonce 不是
  密码学令牌，靠“只有主人能私聊机器人”这一边界。
- **`auto` 的发送规则**：频率、同 chat 重复、跨群同文、@全体、跨 chat 5-gram
  重叠（≥24 字、共享 5 个 5-gram）与媒体来源都会拦；仍没有完整的
  `context_sources` 追踪。规则是启发式，可能误报长文本的合理转述。
- **周期检查的缓存复用**：检查请求使用独立窗口，不与主请求逐字节共享前缀；
  token 计数是估算值。
- **WebFetch 后端模式**：初始 URL 与返回的 `final_url` 都会在本地校验，但抓取与
  重定向仍由你配置的后端执行；需要本地逐跳校验时设 `[tools.web_fetch].local = true`。
- **加密边界**：防的是拿到 `data.db` / 文件但没有 `key`；不防同机管理员、
  进程内存读取，也不防拿到备份（备份含 key）。
- **`receive_scopes` 的边界**：`get_msg` 会通过消息索引反查 chat；查不到且在限制读取时
  保守拒绝。
- **VFS 不是沙箱**：它隔离的是模型与宿主文件系统，不隔离网络。

## 协议与接入

- 只支持 OneBot 11（通过 NapCat WebSocket）；没有官方 QQ 协议实现。
- 单实例：同一 `SBOT_HOME` 同时只能跑一个进程。
- `start` 是前台进程；systemd / Windows 服务需要你自己包一层。
- MCP 只有 stdio 传输、一个聚合 `MCP` 工具；不支持 HTTP/SSE 传输；
  `tools/list` 分页最多取 10 页，超过会告警并停止发现剩余工具。
- 不迁移 0.1 数据；0.2 从空存储开始。

- **`[budget]` 的语义**：每日 token/USD 在回合开始检查，超了跳过回合（消息已入库）；
  单回合 token/墙钟是硬停；每 chat 小时限额只统计成功发送；`guard_usd_per_day` 是
  估算值（Jev 固定单次成本，LLM 按约 3k 输入 + 100 输出估算），耗尽后分类器降级为
  规则层，规则仍生效。
- **`[hooks.events]` 的边界**：所有事件都会触发 `log`/`block`/`inject_context`/`notify`，
  但 `rewrite` 只在 BeforeSend 生效；`block` 只在 MessageReceived 能阻止内容进入上下文；
  `inject_context` 只被 SessionStart（system）、MessageReceived（回合内提示）、
  Pre/PostCompact（摘要输入/新分段）消费；SessionEnd/Stop/ConfigChange 的注入没有去处。
  `notify` 只支持 `owner`/`master`，且需要 NapCat 已连接。
- **`[tools].max_schema_tokens`**：超限时把过长描述压到 160 字符；若仅 schema 就超限，
  只告警、不删工具。
- **monitor 的 `cc` / `on_missing`**：`cc` 在绑定子代理时抄送主代理；`on_missing`
  支持 `drop`、`queue`（最多等 60 秒）与 `spawn`（新建 general 子代理处理）。

## 已知行为细节

- `[session].scope = shared` 时跨 chat 上下文互相可见，这是替身场景的默认设计；
  需要隔离用 `per_chat`。
- 子代理嵌套受 `max_depth`（默认 8）限制；`monitor` 绑定随子代理结束删除。
- `Agent message` 发给正在运行的子代理会排入其历史，但当前回合不会中途读取，
  要等下一次运行。
- 压缩失败连续达到 `[compaction].max_failures` 后，自动压缩停止尝试，直到重启或
  一次手动 `agent.compact` 成功。
- `[compaction].post_compact_recall` 注入的是 `/MEMORY.md` 索引，不是按语义检索
  的相关条目；预算 8000 字符。
- `[memory].review = every_turn` 实际是每 5 秒轮询一次并检查 transcript 是否有新增，
  不是严格“回合结束即触发”。
- `[monitor.lifecycle].catch_up` 只支持 `coalesce` 与 `skip`；补投只针对定时源，
  运行时创建的 monitor 不参与；长停机后的补投次数有上限。
- `sbot doctor` 是只读的：它发现的问题需要你按提示手工修复。

## 测试相关

- 真实 API / NapCat 测试默认 `#[ignore]`，需要环境变量显式启用
  （见 [live-testing.md](../development/live-testing.md)）。
- CI（GitHub Actions）运行 `cargo fmt --check`、`clippy -D warnings` 与
  `cargo test --all-targets`；真实端点测试不在 CI 里。
