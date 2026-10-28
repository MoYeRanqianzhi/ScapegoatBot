---
name: legacy-architecture
description: ScapegoatBot 重构前（2026-10-28 之前）的模块结构、数据流与关键设计，重构时用于对照旧行为
metadata:
  type: project
  scope: ScapegoatBot 重构前的代码库；重构完成后本文可能整体失效
  status: active
  last_verified: 2026-10-28
---

ScapegoatBot 在 2026-10-28 彻底重构前采用单进程架构：`main.rs` 负责 CLI 与 NapCat 事件，`agent.rs` 驱动 Agent 循环，其余子系统按模块划分。

## 模块结构

- `config.rs` — `AppConfig`，解析 `config.toml` / `llm.toml`
- `main.rs` — CLI 路由 + NapCat 事件监听 + 消息入口（`handle_group_message` / `handle_private_message`）
- `agent.rs` — Agent 核心循环 `run()`；`build_tool_specs()` 定义全部内置工具
- `prompt.rs` — `build_system_prompt()` 构建系统提示词
- `providers/` — `Box<dyn Provider>` 运行时分发：`openai`（ChatCompletions / Responses 双端点）、`anthropic`（Messages）；全部使用 `stream: true` + SSE，防 504 网关超时
- `tools/` — 内置工具，全部经 cap-std `Dir` 句柄限制在 `workspace/` 内
- `mcp/` — MCP 客户端：子进程 stdin/stdout 上的 JSON-RPC 2.0，工具名 `mcp.<server>.<tool>`
- `swarm/` — 蜂群子代理：`SwarmManager`（生命周期）、`SubAgentRunner`（轻量 agentic 循环）、`ToolDispatcher`（主/子代理共享工具路由）、`tools.rs`、`prompt.rs`
- `storage/` — 会话与消息持久化：`session.rs`（JSONL + SQLite 双写）、`message_index.rs`、`images.rs`、`markdown.rs`、`jsonl.rs`、`cursor.rs`
- 辅助模块 — `whitelist.rs`（收发白名单 HashSet）、`group_role.rs`（群主/管理员/成员状态）、`compact.rs`（上下文压缩）、`message_format.rs`（消息段 → CQ XML）、`cli.rs`、`control.rs` / `control_server.rs`（TCP 控制通道）、`agent_state.rs`、`skills/`

## 主数据流

```
NapCat WebSocket → main.rs 事件监听
  ├─ 消息事件 → storage（JSONL + Markdown 文件 + SQLite 索引）
  └─ mpsc → Agent
       ├─ 去抖：3 秒窗口批量合并为单条 user turn
       ├─ 上下文构建：系统提示词 + 会话历史 + 压缩检查
       ├─ LLM 请求（SSE 流式）
       └─ 工具分发（内置 + MCP），循环至 Finish 或 max_steps
```

会话管理采用双写：内存 `Vec`（构建请求）、JSONL 文件（持久化/恢复）、SQLite `sessions.db`（`prev_id` 链式追溯）。压缩触发 `session.rotate()`：旧 JSONL 保留供 AI 用 Read 回溯，摘要作为新会话初始消息，`prev_session_path` 注入系统提示词。

群角色同步三种方式：启动全量、管理员变动事件驱动、每 12 小时定时轮询；管理员工具说明按群注入系统提示词。

## 关键设计

- **上下文压缩**（`compact.rs`）：token ≈ chars/4；总 token > 180k 且消息数 > 8 触发；保留最近 8k token，前缀生成 ≤4096 token 摘要。
- **cap-std 沙箱**（`tools/workspace.rs`）：7 层路径校验（`..`、绝对路径、Windows 盘符、null 字节、超长、过深、cap-std 兜底）；`messages/raw/`、`sessions/`、`data/` 对 LLM 写保护；Write 1MB、Read 200KB、Grep 500KB；工作区 2GB/50000 文件、磁盘剩余 100MB 下限；Edit 前必须先 Read（Read 一次后可多次 Edit）。
- **Swarm**：父→子用 `mpsc::UnboundedSender<AgentMessage>`，子→父用 `SwarmNotification` 经 `notify_tx` 回到主代理 `select!`；状态共享 `Arc<Mutex<AgentStatus>>`；子代理可嵌套 spawn；`[[models]]` 按名称解析独立 provider/model，缺省继承主 `[llm]`；默认 `max_depth=32`、`max_concurrency=1024`、子代理 `max_steps=64`；子代理无会话持久化、无去抖、无 QQ 通道，结果经 Finish 的 `result` 字段回传。
- **安全定位**：零执行（无 Bash/Shell）、消息收发分离白名单、MCP 子进程隔离、循环步数上限；Fetch 默认禁用，启用后做 SSRF 防护（私有/保留 IP 拦截）与域名白/黑名单，响应体 512KB、输出 50000 字符上限。

## SQLite 数据库（`workspace/data/`，WAL + `Mutex<Connection>`）

| 数据库 | 用途 | 关键字段 |
|--------|------|----------|
| `images.db` | 消息图片注册表 | message_id + index → image URL |
| `messages.db` | 消息索引 | message_id → file_path, line_number, timestamp, user_id, group_id, message_type |
| `sessions.db` | 会话索引 | id → file_path, prev_id, created_at, is_current |

## 代码定位速查

| 关注点 | 文件 |
|--------|------|
| Agent 循环 | `src/agent.rs` — `run()` |
| 工具定义 | `src/agent.rs` — `build_tool_specs()` |
| 工具路由 | `src/swarm/dispatch.rs` — `dispatch_tool()` |
| 系统提示词 | `src/prompt.rs` — `build_system_prompt()` |
| 上下文压缩 | `src/compact.rs` |
| 文件工具 | `src/tools/workspace.rs` |
| 消息/群管理 | `src/tools/message.rs` |
| Provider trait | `src/providers/traits.rs` |
| 会话管理 | `src/storage/session.rs` |

**Evidence:** 归档自 `docs/development/architecture.md`；删除前内容可在提交 `f49019d` 中检索：`git show f49019d:docs/development/architecture.md`。
**Recheck when:** 重构后的 `src/` 结构与本文不一致时，以新代码为准；引用的文件/函数若已不存在，本条目按 `superseded` 处理。
**Why:** 彻底重构需要知道旧实现的边界与已解决过的问题（沙箱、压缩、Swarm 通信），避免重蹈覆辙或丢失既有约束。
**How to apply:** 设计新架构时对照本文确认哪些约束必须保留（安全、白名单、压缩阈值），哪些是实现包袱可放弃；不要直接照搬旧模块划分。
