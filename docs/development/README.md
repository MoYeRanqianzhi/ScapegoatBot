# 开发文档

本目录面向协作者与 LLM。完整设计仍在 `.agents/plan/`；这里给出 0.2 代码的定位与维护规则。
使用者文档在 [`../guide/`](../guide/README.md)。

## 入口

| 文档 | 内容 |
|------|------|
| [../guide/](../guide/README.md) | 用户指南：安装、配置、日常使用、安全 |
| [refactor-report.md](refactor-report.md) | 0.1 → 0.2 重构率、逐文件归属与计算（alpha.12 快照） |
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
| `app/` | 运行时、IPC handler、QQ 管线（`events.rs`）、后台维护；`assembly/` 按职责拆成 `stores`（存储与启动对账）、`providers`（模型清单与各模型的计量 provider）、`tools`（工具表与唤醒预设）、`qq`（NapCat 连接与 owner/master），`mod.rs` 的 `build_agent` 只负责按顺序接起来 | `Runtime::start`、`assembly::build_agent` |
| `paths.rs` | `~/.sbot` 布局、逻辑根解析 | `SbotPaths` |
| `config/mod.rs` | 根结构、加载、环境变量覆盖、校验分派 | `SbotConfig::load` |
| `config/sections/` | 分节 schema（agent/queue/security/monitor/hooks…），每节一个文件 | 各 `Config` 结构 |
| `storage/` | 迁移（只追加，最新版本见 `MIGRATIONS` 末项，当前 v12）、主密钥、消息归档 | `Database`、`MessageStore` |
| `vfs/` | VFS 路径、查询串与 `filter` 语法、SQLite 后端（`sqlite/stage.rs` 是分段写入）、配额 | `VfsStore`、`BlobStage`、`VfsPath`、`ViewQuery`、`ViewFilter` |
| `media.rs` | 图片解码上限、缩图、阻塞线程池（`Read` / 图片动作 / MCP 共用） | `decode_limited`、`shrink_to_fit`、`blocking` |
| `providers/` | 三种协议、SSE、回退 | `build_provider`、`AgentLoop::new_with_fallbacks` |
| `agent/` | 循环、消息/事件、队列、runner | `AgentLoop`、`Runner` |
| `security/` | 内容闸、Jev/llm 分类器、周期检查、动作门禁、hold、审计 | `Gate`、`ActionGate`、`SecurityReviewer` |
| `qq/` | 事件归一化、格式、过滤、重连 | `QqClient`、`MessageFilter` |
| `session/` | 分段、turn buffer、压缩 | `SessionStore`、`compaction` |
| `actions.rs` | 约 50 个 API action 的注册表数据（由 `tools` 重导出） | `specs()` |
| `tools/api.rs` | `API` 工具的执行、作用域、hold、发送账本 | `ApiTool` |
| `tools/` | Tool trait、文件/记忆/技能/下载/Web/Monitor；`web/html.rs` 是线性的 HTML → 文本 | `ToolRegistry` |
| `tools/views/` | 动态视图：messages/inbox/sessions/contacts/config/system；`filter.rs` 是条件求值与分页 | `Views`、`Page` |
| `monitor/` | `predicate.rs`（`when`）、`spec.rs`（配置与 `Monitor add` 参数 → `MonitorSpec` 的唯一转换）、`delivery.rs`（批处理与投递）、`timer.rs` / `stream.rs` / `internal.rs`（三种非 QQ 源） | `MonitorManager`、`MonitorSpec::from_config` |
| `ipc/` | 命名管道 / UDS、帧协议 | `Endpoint`、`Client` |

## 结构约定

- 单函数尽量 <200 行、单结构字段尽量 <20；超了就按职责拆（`tools/actions.rs`、
  `config/sections/`、`tools/views/*`、`app/assembly/`、`monitor/`、`ScopeCache` 是先例）。
- 模型面文本（system prompt、工具描述、错误）保持英文；代码注释用中文。
- 不要在文档或注释里承诺未实现的行为；未实现项集中写在
  [`../guide/limits.md`](../guide/limits.md)。

## 维护约定

- 改行为时同步更新 `docs/guide/`、`docs/development/` 与 `README.md` 的「已知限制」。
- 新增存储字段必须追加迁移，不能改已发布的迁移。
- 新增工具要声明参数 schema 与执行模式；操作 QQ 的工具要在 hold 期间拒绝。
- `config.example.toml` 由 `tests/config_example.rs` 校验：改配置 schema 时同步改样例。
- 提交前至少运行 `cargo fmt --check`、`cargo test --all-targets`、
  `cargo clippy --all-targets -- -D warnings`；CI 会跑同样三项。
- 发布：推送 `v*` 标签会触发 `.github/workflows/release.yml`，在 Linux/Windows
  原生 runner 上用 `cargo build --release --locked` 构建，上传二进制与 sha256，
  并创建 GitHub Release。`[profile.release]` 使用 fat LTO、单 codegen unit、
  `panic = "abort"` 与符号剥离，优先运行时性能、其次体积。
