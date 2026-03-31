# 架构文档

本文档描述 ScapegoatBot 的整体架构、模块关系、数据流和关键设计模式。

## 模块关系图

```
                     ┌─────────────┐
                     │  config.rs  │  ← config.toml
                     └──────┬──────┘
                            │ AppConfig
           ┌────────────────┼───────────────────┐
           │                │                   │
           ▼                ▼                   ▼
    ┌─────────────┐  ┌─────────────┐    ┌──────────────┐
    │   main.rs   │  │  agent.rs   │    │  prompt.rs   │
    │ CLI + 事件  │  │ Agent 循环  │    │ 系统提示词   │
    └──┬──────┬───┘  └──┬────┬─────┘    └──────────────┘
       │      │         │    │
       │      │         │    ├──────────────────────┐
       │      │         │    │                      │
       ▼      ▼         ▼    ▼                      ▼
  ┌────────┐ ┌─────────────┐ ┌───────────┐  ┌───────────┐
  │storage/│ │ providers/  │ │  tools/   │  │   mcp/    │
  │ 持久化 │ │ LLM 提供商  │ │ 内置工具  │  │ MCP 客户端│
  └────────┘ └─────────────┘ └─────┬─────┘  └───────────┘
       │            │              │
       ▼            │              ▼
  ┌────────┐        │       ┌──────────────┐
  │ SQLite │        │       │  workspace/  │
  │ 索引DB │        │       │  cap-std 沙箱│
  └────────┘        │       └──────────────┘
                    │
                    ▼
             ┌─────────────┐
             │   swarm/    │
             │ 蜂群子代理  │
             │ ├ Manager   │
             │ ├ Runner    │
             │ ├ Dispatch  │
             │ └ Tools     │
             └─────────────┘
```

### 辅助模块

| 模块 | 职责 |
|------|------|
| `whitelist.rs` | 收发白名单 HashSet，O(1) 查找 |
| `group_role.rs` | 群身份状态管理（群主/管理员/普通成员），三种同步方式 |
| `compact.rs` | 上下文压缩（token 估算、前缀摘要、尾部保留） |
| `message_format.rs` | 消息段 → CQ XML 格式化 |
| `cli.rs` | clap CLI 命令定义 |
| `control.rs` / `control_server.rs` | TCP 控制通道（CLI ↔ 守护进程通信） |
| `agent_state.rs` | Agent 运行状态管理 |
| `skills/` | Skill 注册表，cap-std 沙箱隔离 |
| `swarm/` | 蜂群子代理系统（SwarmManager + SubAgentRunner + ToolDispatcher） |

## 数据流

### 主流程

```
NapCat WebSocket
    │
    ▼
事件监听 (main.rs)
    │
    ├─── 消息事件 ──→ 消息存储 (storage/) ──→ JSONL + Markdown 文件
    │                                          + SQLite 索引
    │
    ├─── 图片注册 ──→ ImageStore (images.db)
    │
    └─── mpsc 通道 ──→ Agent (agent.rs)
                          │
                          ├── 去抖（3 秒窗口，批量合并为单条 user turn）
                          │
                          ├── 上下文构建
                          │    ├── 系统提示词 (prompt.rs)
                          │    ├── 会话历史 (SessionManager)
                          │    └── 压缩检查 (compact.rs)
                          │
                          ├── LLM 请求 (providers/)
                          │    ├── OpenAI CC/Responses 端点
                          │    └── Anthropic Messages 端点
                          │    └── 全部 SSE 流式传输
                          │
                          ├── 工具分发 (agent.rs dispatch)
                          │    ├── 内置工具 (tools/)
                          │    └── MCP 工具 (mcp/)
                          │
                          └── 循环直到 Finish 或达到 max_steps
```

### 会话管理流程

```
Agent 运行
    │
    ├─── session.push(msg) ──→ 内存 Vec + JSONL 文件双写
    │
    ├─── 压缩触发 ──→ session.rotate(initial_msgs)
    │                    ├── 关闭旧 JSONL 文件（保留供 AI Read）
    │                    ├── 创建新 JSONL 文件
    │                    └── prev_session_path 注入系统提示词
    │
    └─── SQLite 索引 (sessions.db) ──→ 会话链追溯 (prev_id)
```

### 群角色同步流程

```
启动 ──→ 全量同步（遍历 recv_groups，获取每个群的 Bot 角色）
    │
    ├── 事件驱动 ──→ 管理员变动通知 → 即时更新 RwLock<HashMap>
    │
    └── 定时轮询 ──→ 每 12 小时全量同步一次
    │
    └── 系统提示词 ──→ 按群注入管理员权限工具说明
```

## 关键设计模式

### 动态 Provider 分发

Agent 持有 `Box<dyn Provider>`，运行时根据 `config.llm.provider` 选择具体实现：

```
config.llm.provider
    │
    ├── "openai"    → OpenAiProvider (CC 或 Responses 端点)
    └── "anthropic" → AnthropicProvider (Messages 端点)
```

`Provider` trait 定义于 `src/providers/traits.rs`，核心方法：

- `chat()` — 发送消息列表 + 工具定义，返回流式 LLM 响应
- 所有请求均使用 `stream: true` + SSE 解析，防止 504 网关超时

### 会话管理 (SessionManager)

`SessionManager` 实现双写策略：

- **内存 Vec** — 快速访问，构建 LLM 请求时直接使用
- **JSONL 文件** — 持久化，崩溃恢复和历史追溯
- **SQLite 索引** — 会话链管理，通过 `prev_id` 链接前后会话

会话轮转（compact 触发时）：
1. 旧会话文件保留在 `sessions/` 目录
2. 创建新会话文件，内存 Vec 重置为初始消息（包含摘要）
3. `prev_session_path` 注入系统提示词的"会话记录"章节，AI 可通过 Read 工具回溯

### cap-std 文件沙箱

所有文件工具（Ls/Read/Write/Edit/Grep）通过 `cap_std::fs::Dir` 句柄操作：

```
workspace/ (cap-std Dir 根)
    ├── messages/      # 消息记录（Markdown + JSONL）
    │   └── raw/       # 原始消息 [写保护]
    ├── sessions/      # 会话 JSONL [写保护]
    ├── data/          # SQLite 数据库 [写保护]
    ├── skills/        # 工作区内的 Skill 文件
    ├── AGENTS.md      # AI 人格配置
    ├── SOUL.md        # AI 灵魂配置
    └── MEMORY.md      # AI 记忆文件
```

路径验证（7 层防御）：
1. 拒绝 `..` 路径遍历
2. 拒绝绝对路径
3. 拒绝 Windows 驱动器号（如 `C:`）
4. 拒绝 null 字节
5. 拒绝超长路径
6. 拒绝过深嵌套
7. cap-std 硬件级隔离（即使前 6 层被绕过）

### MCP 连接管理

MCP 通过子进程 stdin/stdout 进行 JSON-RPC 2.0 通信：

```
ScapegoatBot 进程
    │
    ├── StdioTransport (子进程 1) ← MCP 服务器 A
    ├── StdioTransport (子进程 2) ← MCP 服务器 B
    └── ...
```

工具命名约定：`mcp.<服务器名>.<工具名>`

生命周期：
1. 启动时根据 `[mcp.*]` 配置 spawn 子进程
2. 发送 `initialize` 请求 + `initialized` 通知完成握手
3. 调用 `tools/list` 发现可用工具，转换为 ToolSpec 注入 LLM
4. Agent 调用 MCP 工具时路由到对应 StdioTransport
5. 关闭时 kill 子进程

### 上下文压缩

压缩策略（`compact.rs`）：

```
估算 token 数 = chars / 4（中英混合近似）

触发条件：总 token > trigger_tokens (180k) 且消息数 > min_messages (8)

压缩过程：
1. 从尾部保留 keep_recent_tokens (8k) 对应的最新消息
2. 将剩余前缀消息作为待压缩内容
3. 调用 LLM 生成 ≤ reserve_summary_tokens (4096) 的摘要
4. 触发 session.rotate()：摘要作为新会话的初始消息
```

### 蜂群子代理子系统 (Swarm)

蜂群子系统允许主 Agent 生成多个子代理并行执行任务。每个子代理运行在独立的 tokio 任务中，
拥有独立的 LLM 对话历史和 Provider 实例，但共享工具执行上下文（ToolContext）。

#### 核心组件

```
SwarmManager (mod.rs)
    │
    ├── 管理子代理完整生命周期
    │   ├── spawn()        — 创建子代理（验证名称唯一性、深度、并发限制）
    │   ├── message_agent() — 向子代理发送消息
    │   ├── check_agent()   — 查询子代理状态
    │   ├── wait_agent()    — 等待子代理完成并获取结果
    │   ├── wait_all()      — 等待所有运行中子代理
    │   └── shutdown_all()  — 向所有子代理发送关闭指令
    │
    ├── 持有共享资源
    │   ├── config: Arc<AppConfig>
    │   ├── tool_ctx: Arc<ToolContext>
    │   ├── mcp_manager: Option<Arc<Mutex<McpConnectionManager>>>
    │   └── tool_specs: Arc<Vec<ToolSpec>>
    │
    └── agents: Mutex<HashMap<String, AgentHandle>>

SubAgentRunner (runner.rs)
    │
    ├── 轻量级 agentic 循环
    │   ├── 检查父代理消息（try_recv 非阻塞）
    │   ├── 构建请求 [system + conversation + retry_reminder]
    │   ├── 调用 LLM
    │   ├── 处理工具调用（Finish / 蜂群工具 / 普通工具）
    │   └── 无工具调用时设置重试提醒
    │
    ├── 与主代理 AgentRunner 的区别
    │   ├── 无去抖/批量逻辑（单一任务）
    │   ├── 无 QQ 消息通道
    │   ├── 无会话持久化（对话仅保存在内存中）
    │   └── Finish 工具的 result 字段用于向父代理传递结果
    │
    └── 子代理也可以 spawn 子代理（嵌套深度受 max_depth 限制）

ToolDispatcher (dispatch.rs)
    │
    ├── 从 AgentRunner 提取的工具分发逻辑
    ├── 主 Agent 和子 Agent 共享同一套工具调用能力
    ├── 路由：工具名 + JSON 参数 → 具体工具实现
    └── 返回 ToolResult（纯文本或多模态图片）
```

#### 通信机制

```
父代理 (AgentRunner)
    │
    ├── SpawnAgent ──→ SwarmManager::spawn()
    │                    └── tokio::spawn(SubAgentRunner::run())
    │
    ├── MessageAgent ──→ SwarmManager::message_agent()
    │                      └── mpsc::UnboundedSender → AgentMessage::UserMessage
    │                           └── 子代理在循环开始时 try_recv 接收
    │
    ├── CheckAgent ──→ SwarmManager::check_agent()
    │                    └── 读取 Arc<Mutex<AgentStatus>> 快照
    │
    ├── WaitAgent ──→ SwarmManager::wait_agent()
    │                   └── 取出 JoinHandle 并 await
    │
    └── swarm_rx (mpsc::UnboundedReceiver) ←── SwarmNotification
         └── 子代理完成时通过 notify_tx 发送
              └── 主代理 select! 中接收，注入对话历史
```

- **父→子消息**：通过 `mpsc::UnboundedSender<AgentMessage>` 传递，子代理非阻塞接收
- **子→父通知**：`SwarmNotification` 通过 `notify_tx` 发送，主代理在 `select!` 中通过 `swarm_rx` 接收
- **状态共享**：`Arc<std::sync::Mutex<AgentStatus>>`，子代理退出时更新为 Completed/Failed

#### 模型解析

子代理可以使用与主 Agent 不同的 LLM 模型：

```
SpawnAgent(model = "fast")
    │
    ├── config.find_model("fast")
    │   └── 匹配 [[models]] 中 name = "fast" 的配置
    │       ├── 独立的 provider / base_url / api_key / model
    │       └── 可选字段为 None 时继承主 [llm] 配置
    │
    └── 未指定 model → 使用主 [llm] 配置
```

#### 安全限制

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `swarm.enabled` | `true` | 功能总开关，禁用时不注册蜂群工具 |
| `swarm.max_depth` | `32` | 最大嵌套深度（主代理 spawn 的子代理 depth=1） |
| `swarm.max_concurrency` | `1024` | 所有层级的子代理总数上限 |
| `max_steps`（per model） | `64` | 单个子代理的最大工具调用步数 |

## 安全架构

### 文件安全

| 层次 | 机制 | 实现位置 |
|------|------|----------|
| 沙箱隔离 | cap-std Dir 句柄限制到 workspace 根 | `tools/workspace.rs` |
| 路径校验 | 7 层验证（详见上文） | `tools/workspace.rs` |
| 写保护 | `messages/raw/`、`data/`、`sessions/` 对 LLM 只读 | `tools/workspace.rs` |
| 大小限制 | Write 1MB、Read 200KB、Grep 500KB | `tools/workspace.rs` |
| 容量限制 | 工作区 2GB / 50000 文件、磁盘剩余 100MB | `tools/workspace.rs` |
| Edit 前置 | 必须先 Read 后才能 Edit（防盲写） | `tools/workspace.rs` |

### 网络安全

| 层次 | 机制 | 说明 |
|------|------|------|
| 默认禁用 | Fetch 工具需要 `[fetch] enabled = true` 显式启用 | 默认零网络访问 |
| SSRF 防护 | 拦截对私有/保留 IP 地址的访问 | 127.0.0.0/8, 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16 等 |
| 域名过滤 | whitelist/blacklist 模式控制可访问域名 | 配置 `[fetch] mode` + `domains` |
| 响应限制 | 最大响应体 512KB、最大输出 50000 字符 | 防止上下文爆炸 |

### 执行安全

| 层次 | 机制 | 说明 |
|------|------|------|
| 零执行 | 无 Bash/Shell/Exec 工具 | AI 无法执行任何系统命令 |
| MCP 隔离 | 外部工具通过子进程 + stdin/stdout 通信 | 不共享主进程内存或文件句柄 |
| 循环限制 | max_steps 配置（默认 128） | 防止无限工具调用循环 |
| 白名单 | 消息收发白名单约束 | AI 只能向白名单内的会话发消息 |

## SQLite 数据库

所有数据库位于 `workspace/data/`，使用 WAL 模式，`Mutex<Connection>` 保证线程安全。

| 数据库 | 用途 | 关键字段 |
|--------|------|----------|
| `images.db` | 消息图片注册表 | message_id + index → image URL |
| `messages.db` | 消息索引 | message_id → (file_path, line_number, timestamp, user_id, group_id, message_type) |
| `sessions.db` | 会话索引 | id → (file_path, prev_id, created_at, is_current)，通过 prev_id 链式追溯 |

## 代码定位速查

| 你想了解... | 看这个文件 |
|------------|-----------|
| Agent 核心循环 | `src/agent.rs` — `run()` 方法 |
| 工具参数定义 | `src/agent.rs` — `build_tool_specs()` 函数 |
| 工具分发路由 | `src/swarm/dispatch.rs` — `dispatch_tool()` 方法 |
| 蜂群管理器 | `src/swarm/mod.rs` — `SwarmManager` |
| 子代理运行器 | `src/swarm/runner.rs` — `SubAgentRunner::run()` |
| 蜂群工具定义 | `src/swarm/tools.rs` — `build_swarm_tool_specs()` |
| 子代理系统提示 | `src/swarm/prompt.rs` — `build_subagent_system_prompt()` |
| 系统提示词内容 | `src/prompt.rs` — `build_system_prompt()` |
| 上下文压缩逻辑 | `src/compact.rs` |
| 文件工具实现 | `src/tools/workspace.rs` |
| 消息发送实现 | `src/tools/message.rs` |
| 群管理工具实现 | `src/tools/message.rs`（群管理 API 调用） |
| Provider trait | `src/providers/traits.rs` |
| OpenAI 双协议 | `src/providers/openai.rs` |
| Anthropic 实现 | `src/providers/anthropic.rs` |
| MCP JSON-RPC | `src/mcp/mod.rs` |
| MCP 子进程通信 | `src/mcp/transport.rs` |
| 会话管理 | `src/storage/session.rs` |
| 消息索引 | `src/storage/message_index.rs` |
| 配置结构 | `src/config.rs` |
| 白名单实现 | `src/whitelist.rs` |
| 群角色管理 | `src/group_role.rs` |
| 消息状态设计 | `docs/development/message-state.md` |
