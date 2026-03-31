# ScapegoatBot

一个 QQ Agent，代替用户、像真人一样回复消息。

> **替身使者** — 让 AI 以你的语气和风格自动处理 QQ 消息，你只需在幕后观察。

## 功能概览

- **拟人回复** — 通过 AGENTS.md / SOUL.md 等工作区文件塑造 AI 人格，模拟真人语气
- **43+ 内置工具** — 文件读写、消息收发、群管理、搜索查询、表情回应、图片识别、蜂群子代理等
- **MCP 动态扩展** — 通过 [Model Context Protocol](https://modelcontextprotocol.io/) 接入外部工具服务器，无限扩展能力
- **多 Provider 支持** — OpenAI 兼容 API（ChatCompletions / Responses 端点）+ Anthropic Messages API
- **群管理** — 18 个群组管理工具（禁言/踢人/公告/精华/头衔等），管理员状态自动同步
- **安全沙箱** — [cap-std](https://github.com/bytecodealliance/cap-std) 文件隔离 + 零执行能力 + SSRF 防护
- **网络访问** — Fetch 工具获取网页内容（HTML 自动转 Markdown），内置 SSRF 防护
- **上下文压缩** — 对话过长时自动将历史压缩为摘要，保持最近消息原样
- **蜂群子代理** — 主 Agent 可生成子代理并行执行任务，支持嵌套 spawn、跨代理通信和多模型调度
- **Skill 系统** — 可扩展的技能包，通过 SKILL.md 文件注入专业知识
- **白名单控制** — 收发分离的群/私聊白名单，精确控制 AI 的可见范围和发言范围
- **守护进程模式** — `sbot start` 后台运行，`sbot stop` / `sbot status` CLI 管理

## 快速开始

### 前置要求

- [Rust](https://www.rust-lang.org/tools/install) 工具链（edition 2024）
- [NapCatQQ](https://github.com/NapNeko/NapCatQQ) — 提供 QQ 的 OneBot 11 WebSocket 接口
- 一个 LLM API（OpenAI 兼容服务 或 Anthropic）

### 构建

```bash
cargo build --release
```

生成的可执行文件位于 `target/release/sbot`（Windows 为 `sbot.exe`）。

### 配置

复制示例配置文件并填写你的信息：

```bash
cp config.example.toml config.toml
```

必须配置的项：

```toml
[napcat]
ws_url = "ws://127.0.0.1:3001"  # NapCat WebSocket 地址

[whitelist]
recv_groups = [123456789]        # 接收消息的群号
send_groups = [123456789]        # 允许发送消息的群号

[llm]
base_url = "https://api.openai.com/v1"
api_key = "sk-your-api-key"
model = "gpt-4o"
```

完整配置项参见 [config.example.toml](config.example.toml)。

### 运行

```bash
# 前台运行（调试用）
sbot run

# 守护进程模式
sbot start
sbot status
sbot stop
```

## 项目结构

```
ScapegoatBot/
├── src/
│   ├── main.rs              # CLI 入口 + 事件监听 + 消息存储
│   ├── agent.rs             # Agent 核心循环（去抖 → LLM 调用 → 工具分发）
│   ├── agent_state.rs       # Agent 运行状态
│   ├── compact.rs           # 上下文压缩（token 估算、切分、摘要）
│   ├── config.rs            # TOML 配置加载与校验
│   ├── prompt.rs            # 系统提示词构建（10+ section）
│   ├── group_role.rs        # 群身份状态管理（管理员/群主同步）
│   ├── whitelist.rs         # 收发白名单（HashSet O(1) 查找）
│   ├── message_format.rs    # 消息格式化（CQ XML 生成）
│   ├── cli.rs               # CLI 命令定义
│   ├── control.rs           # 控制通道常量
│   ├── control_server.rs    # 控制通道服务端
│   ├── providers/           # LLM 提供商抽象层
│   │   ├── traits.rs        #   Provider trait + 通用类型
│   │   ├── openai.rs        #   OpenAI 兼容（CC + Responses 双协议）
│   │   └── anthropic.rs     #   Anthropic Messages API
│   ├── tools/               # 工具实现
│   │   ├── workspace.rs     #   文件工具（cap-std 沙箱）
│   │   ├── message.rs       #   消息工具（发送/撤回/表情/戳一戳）
│   │   ├── query.rs         #   查询工具（好友/群/成员信息）
│   │   ├── search.rs        #   搜索工具（GetMessage/SearchMessages）
│   │   ├── social.rs        #   社交工具（好友/群请求处理）
│   │   └── skill.rs         #   技能加载工具
│   ├── mcp/                 # MCP 客户端
│   │   ├── mod.rs           #   JSON-RPC 2.0 类型定义
│   │   └── transport.rs     #   Stdio 传输层（子进程通信）
│   ├── swarm/               # 蜂群子代理系统
│   │   ├── mod.rs           #   SwarmManager（子代理生命周期管理）
│   │   ├── types.rs         #   核心类型（SpawnConfig/AgentStatus/AgentHandle）
│   │   ├── runner.rs        #   SubAgentRunner（子代理 agentic 循环）
│   │   ├── dispatch.rs      #   ToolDispatcher（工具分发器）
│   │   ├── tools.rs         #   蜂群工具定义与分发
│   │   └── prompt.rs        #   子代理系统提示构建器
│   ├── storage/             # 持久化存储
│   │   ├── session.rs       #   会话管理（JSONL + SQLite 双写）
│   │   ├── images.rs        #   图片存储（SQLite 索引）
│   │   ├── message_index.rs #   消息索引（SQLite）
│   │   ├── markdown.rs      #   Markdown 格式化
│   │   ├── jsonl.rs         #   JSONL 序列化
│   │   └── cursor.rs        #   文件游标管理
│   └── skills/              # Skill 注册表（cap-std 沙箱隔离）
├── workspace/               # AI 工作区（运行时数据，不提交）
├── docs/development/        # 开发文档
├── config.example.toml      # 配置模板
├── LICENSE                  # MIT 许可证
└── Cargo.toml
```

## 安全模型

ScapegoatBot 的设计目标是 **"即使 Agent 完全被攻击者控制，也无法造成任何伤害"**：

| 防御层 | 措施 |
|--------|------|
| 文件隔离 | cap-std 沙箱 + 7 层路径校验（`..`、绝对路径、盘符、null 字节、长度、深度） |
| 写保护 | `messages/raw/`、`data/`、`sessions/` 目录对 LLM 只读 |
| 写入限制 | 单文件 1 MB、磁盘剩余 <100 MB 拒绝写入、工作区总量 2 GB / 50000 文件上限 |
| 读取限制 | 单文件最大 200 KB（防止上下文爆炸） |
| 零执行 | 无 Bash / Shell / Exec 工具，AI 无法执行任何命令 |
| 消息管控 | 收发分离白名单，AI 只能向白名单内的群/用户发消息 |
| 循环限制 | 每轮最多 128 步工具调用（可配置），防止无限循环 |
| SSRF 防护 | Fetch 工具拦截私有 IP 地址 + 域名白名单/黑名单过滤 |
| MCP 隔离 | 外部 MCP 服务器作为子进程运行，通过 stdin/stdout 通信 |

## 配置说明

| 配置段 | 用途 |
|--------|------|
| `[napcat]` | NapCat WebSocket 连接地址和认证 Token |
| `[whitelist]` | 收发分离的群号/QQ 号白名单 |
| `[storage]` | 工作区目录和文件拆分策略 |
| `[identity]` | 主人账号和额外身份文件注入 |
| `[llm]` | LLM 提供商、API 地址、模型、超时等 |
| `[compaction]` | 上下文压缩的触发阈值和保留策略 |
| `[skills]` | Skill 技能包扫描目录 |
| `[control]` | CLI 与守护进程通信端口 |
| `[mcp.<name>]` | MCP 外部工具服务器配置 |
| `[fetch]` | 网页获取工具开关和安全策略 |
| `[swarm]` | 蜂群子代理功能开关和安全限制（深度、并发） |
| `[[models]]` | 备选 LLM 模型配置（供子代理使用不同模型） |

## 技术栈

| 组件 | 技术 |
|------|------|
| 语言 | Rust 2024 edition |
| 异步运行时 | Tokio |
| QQ 接口 | NapCatLink（OneBot 11 WebSocket） |
| HTTP 客户端 | reqwest（rustls-tls） |
| 文件沙箱 | cap-std |
| 数据库 | SQLite（rusqlite bundled） |
| 序列化 | serde + serde_json + toml |
| CLI | clap（derive 模式） |
| 日志 | tracing + tracing-subscriber |

## 许可证

[MIT](LICENSE) © 2026 墨叶染千枝
