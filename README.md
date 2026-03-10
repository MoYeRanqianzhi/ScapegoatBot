# ScapegoatBot

一个 QQ Agent，代替用户、像真人一样回复消息。

> **替身使者** — 让 AI 以你的语气和风格自动处理 QQ 消息，你只需在幕后观察。

## 特性

- **拟人回复** — 通过 AGENTS.md / SOUL.md 等工作区文件塑造 AI 人格，模拟真人语气
- **多模型支持** — 兼容所有 OpenAI Chat Completions API 的服务（OpenAI、DeepSeek、Moonshot、Ollama 等）
- **安全沙箱** — 文件操作通过 [cap-std](https://github.com/bytecodealliance/cap-std) 硬限制在 workspace 内，即使 AI 被劫持也无法逃逸
- **白名单控制** — 收发分离的群/私聊白名单，精确控制 AI 的可见范围和发言范围
- **原生工具调用** — 16 个内置工具（文件读写、发消息、查群友、处理请求等），通过 OpenAI function calling 协议驱动
- **上下文压缩** — 对话过长时自动将历史压缩为摘要，保持最近消息原样
- **Skill 系统** — 可扩展的技能包，通过 SKILL.md 文件注入专业知识
- **守护进程模式** — `sbot start` 后台运行，`sbot stop` / `sbot status` CLI 管理

## 快速开始

### 前置要求

- [Rust](https://www.rust-lang.org/tools/install) 工具链（edition 2024）
- [NapCatQQ](https://github.com/NapNeko/NapCatQQ) — 提供 QQ 的 OneBot 11 WebSocket 接口
- 一个 OpenAI 兼容的 LLM API（如 OpenAI、DeepSeek 等）

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
│   ├── main.rs          # CLI 入口 + 事件监听
│   ├── agent.rs         # Agent 核心循环（LLM 调用 + 工具分发）
│   ├── compact.rs       # 上下文压缩（摘要生成）
│   ├── config.rs        # TOML 配置加载与校验
│   ├── prompt.rs        # 系统提示词构建
│   ├── providers/       # LLM 提供商抽象层
│   ├── tools/           # 内置工具实现（沙箱文件、消息、查询、社交）
│   ├── skills/          # Skill 系统（cap-std 沙箱保护）
│   └── storage/         # 消息持久化存储
├── workspace/           # AI 工作区（运行时数据，不提交）
├── config.example.toml  # 配置模板
├── LICENSE              # MIT 许可证
└── Cargo.toml
```

## 安全模型

ScapegoatBot 的设计目标是 **"即使 Agent 完全被攻击者控制，也无法造成任何伤害"**：

| 防御层 | 措施 |
|--------|------|
| 文件隔离 | cap-std 沙箱 + 7 层路径校验（`..`、绝对路径、盘符、null 字节、长度、深度） |
| 写入限制 | 单文件 1 MB、磁盘剩余 <100 MB 拒绝写入、工作区总量 2 GB 上限 |
| 零执行 | 无 Bash / Shell / Exec 工具，AI 无法执行任何命令 |
| 消息管控 | 收发分离白名单，AI 只能向白名单内的群/用户发消息 |
| 循环限制 | 每轮最多 32 步工具调用，防止无限循环 |

## 许可证

[MIT](LICENSE) © 2026 墨叶染千枝
