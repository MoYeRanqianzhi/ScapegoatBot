# Changelog

本文件记录 ScapegoatBot 的所有重要变更。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### 新增

- **蜂群子代理系统** — 主 Agent 可通过 SpawnAgent 生成子代理并行执行任务
  - SwarmManager 管理子代理完整生命周期（创建、消息传递、状态查询、等待完成）
  - SubAgentRunner 轻量级 agentic 循环（独立 tokio 任务，独立 LLM 对话）
  - 子代理可嵌套 spawn 子代理，深度受 `swarm.max_depth` 限制
  - 完成通知机制：子代理通过 SwarmNotification 唤醒主代理
- **4 个蜂群工具** — SpawnAgent / MessageAgent / CheckAgent / WaitAgent
  - SpawnAgent：生成子代理（支持指定备选模型和额外上下文）
  - MessageAgent：向运行中子代理发送消息
  - CheckAgent：查询子代理状态（支持单个或全部列出）
  - WaitAgent：等待子代理完成并获取结果（支持单个/多个/全部）
- **备选模型配置** — `[[models]]` 段定义子代理可用的 LLM 模型
  - 支持不同提供商、API 地址、上下文窗口等独立配置
  - 可选字段为空时自动继承主 `[llm]` 配置
- **蜂群配置** — `[swarm]` 段控制子代理系统安全参数
  - `enabled`: 功能总开关（默认 true）
  - `max_depth`: 最大嵌套深度（默认 32）
  - `max_concurrency`: 最大并发子代理数（默认 1024）
- **MCP 支持** — 通过 Model Context Protocol 接入外部工具服务器
  - Stdio 传输层（子进程 stdin/stdout JSON-RPC）
  - 工具自动发现，命名格式 `mcp.<server>.<tool>`
  - 配置: `[mcp.<name>]` 段
- **Fetch 工具** — Agent 网络访问能力
  - HTML 自动转换为 Markdown
  - SSRF 防护：私有 IP 地址拦截
  - 域名白名单/黑名单过滤
  - 默认禁用，需在 `[fetch]` 段显式启用
- **群管理工具** — 18 个群组管理工具（禁言/踢人/公告/精华等）
- **管理员状态同步** — 启动同步 + 事件驱动 + 定时轮询
- **开发文档** — architecture.md, tools-reference.md

### 改进

- **ToolDispatcher 提取重构** — 工具分发逻辑从 AgentRunner 提取为独立的 ToolDispatcher，主 Agent 和子 Agent 共享同一套工具调用能力
- **Edit 工具** — 改为 Claude Code 模式（Read 一次后可多次 Edit）
- **CLI 命令注入** — Codex 风格消息插队
- **消息自动标记已读** — 首次 LLM 请求成功后标记
- **消息段 Schema** — ToolSpec 详细描述每种消息段类型及其 data 字段

### 修复

- 修复 cmd_rx 通道关闭后 CPU 100% 空转的 busy-spin bug

## [0.1.0-alpha.1] - 2026-03-10

首个 Alpha 版本，核心架构就绪。

### 新增

- **Agent 核心循环** — 消息去抖 → LLM 调用 → 工具分发 → 多轮对话
- **OpenAI 兼容提供商** — 支持 OpenAI、DeepSeek、Moonshot 等所有兼容 API
- **原生工具调用** — 16 个内置工具（Read/Write/Edit、消息收发、群友查询、好友请求处理、Skill 加载）
- **cap-std 安全沙箱** — 文件操作硬限制在 workspace 内，含 7 层路径校验
- **写入安全防护** — 单文件 1 MB 上限、磁盘空间检查（100 MB 底线）、工作区容量限制（2 GB / 50000 文件）
- **上下文压缩** — 对话超过 token 阈值时自动压缩历史为摘要
- **收发分离白名单** — 精确控制 AI 可见范围和发言范围
- **Skill 系统** — 通过 SKILL.md 注入技能，每个 skill 目录独立沙箱
- **消息存储** — 按时间线统一存储（JSONL + Markdown），支持文件拆分
- **系统提示词构建** — 10+ 节结构化 prompt，支持 AGENTS.md / SOUL.md 人格注入
- **CLI 进程管理** — `sbot start/stop/status/run` 守护进程模式
- **控制通道** — TCP 端口控制 Agent 暂停/恢复/重载

### 依赖

- NapCatLink v0.1.0-alpha.2（OneBot 11 WebSocket SDK）
- cap-std 3（能力安全文件系统）
- fs2 0.4（磁盘空间查询）
- tokio, reqwest, serde, chrono, clap, tracing 等
