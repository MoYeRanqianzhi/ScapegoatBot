# Changelog

本文件记录 ScapegoatBot 的所有重要变更。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

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
