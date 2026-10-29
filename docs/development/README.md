# 开发文档

本目录面向协作者与 LLM。完整设计仍在 `.agents/plan/`；这里给出 0.2 代码的定位与维护规则。

## 入口

| 文档 | 内容 |
|------|------|
| [refactor-report.md](refactor-report.md) | 0.1 → 0.2 重构率、逐文件归属与计算 |
| [live-testing.md](live-testing.md) | 真实 Jev / Chat API、守护进程回合与实机结果 |
| [prompt-maintenance.md](prompt-maintenance.md) | 系统提示词与所有模型面文本的维护规则 |
| [../../.agents/plan/2026-10-28-rebuild-design.md](../../.agents/plan/2026-10-28-rebuild-design.md) | 重构总设计 |
| [../../.agents/plan/2026-10-29-prompt-catalog.md](../../.agents/plan/2026-10-29-prompt-catalog.md) | 模型面文本清单 |
| [../../.agents/plan/2026-10-30-step-guard-research.md](../../.agents/plan/2026-10-30-step-guard-research.md) | Jev 分类器研究（含真实 API 结果） |

## 模块地图

| 路径 | 职责 | 关键入口 |
|------|------|----------|
| `main.rs` | 薄入口，只调用 `cli::run` | — |
| `cli/mod.rs` | 子命令、IPC 客户端、重启 | `run()` |
| `cli/backup.rs` | 在线 SQLite 快照 + 文件归档 | `run()` / `run_with_paths()` |
| `cli/doctor.rs` | 只读诊断 | `run()` / `collect()` |
| `app.rs` | 运行时装配、IPC handler、QQ 管线 | `Runtime::start`、`build_agent` |
| `paths.rs` | `~/.sbot` 布局、逻辑根解析 | `SbotPaths` |
| `config/` | 严格 schema、凭据、模型清单 | `SbotConfig::load` |
| `storage/` | 迁移 v1–v6、主密钥、消息归档 | `Database`、`MessageStore` |
| `vfs/` | VFS 路径、SQLite 后端、配额 | `VfsStore`、`VfsPath` |
| `providers/` | 三种协议、SSE、回退 | `build_provider`、`AgentLoop::new_with_fallbacks` |
| `agent/` | 循环、消息/事件、队列、runner | `AgentLoop`、`Runner` |
| `security/` | 规则层、Jev、hold、审计 | `Gate` |
| `qq/` | 事件归一化、格式、过滤、重连 | `QqClient`、`MessageFilter` |
| `session/` | 分段、turn buffer、压缩 | `SessionStore`、`compaction` |
| `tools/` | Tool trait、文件/API/记忆/技能/下载/Web/Monitor/视图 | `ToolRegistry` |
| `monitor/` | qq/timer/internal 源、cron、投递 | `MonitorManager` |
| `ipc/` | 命名管道 / UDS、帧协议 | `Endpoint`、`Client` |

## 维护约定

- 每行代码都要有能独立读懂的中文注释；模型面文本（system prompt、工具描述、错误）保持英文。
- 改行为时同步更新 `docs/development/` 与 `README.md` 的「已知限制」。
- 新增存储字段必须追加迁移，不能改已发布的迁移。
- 任何新工具都要声明 `Permission`、参数 schema 与执行模式。
- 提交前至少运行 `cargo test --all-targets`、`cargo clippy --all-targets -- -D warnings`。
