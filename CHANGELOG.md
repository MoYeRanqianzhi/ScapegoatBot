# Changelog

本文件记录 ScapegoatBot 的所有重要变更。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [0.2.0-alpha.13] - 2026-11-04

审查驱动的一轮修复与文档建设。没有破坏性配置变更；数据库迁移追加到 v9。

### 修复

- **压缩**：自动压缩后切片基点改为在压缩之后取，修复越界 panic 与旧块重复提交；
  自动压缩失败按 `[compaction].max_failures` 停止重试；压缩成功后重新读取 hooks 并
  重建 system prompt；控制/monitor 注入的回合也走同一套压缩。
- **消息归档**：append 串行化，行号不再竞争；读取时校验索引行号与行内 id；
  按文件头记录的算法解密；`rebuild_index` 同时重建 `message_files` 与 `inbox_state`；
  保留清理删除孤儿未读记录。
- **会话恢复**：`turn_buffer` 增加 `scope`（迁移 v9），per_chat 只重放自己的回合，
  不再把崩溃回合写进别的 chat。
- **Provider**：SSE 字节流增量 UTF-8 解码，跨 chunk 的中文不再变成 U+FFFD；
  `

` 跨 chunk 也能正确切事件；工具参数先于 start 到达时不再丢弃。
- **安全**：`enforce` 但没有分类器时按 fail-closed 处理（每个回合 hold，启动与
  doctor 提示）；规则命中不再被概率阈值吞掉；`agent.resume` 用真实被判内容复核；
  hold 同时拒绝 Download / Web / MCP；`backend = "llm"` 在配置校验阶段明确拒绝。
- **母语/QQ**：准入过滤提前到 monitor 之前；`delete_msg` / `set_msg_emoji_like`
  恢复可用；`send_msg` 按群发送可用；owner 保护接受字符串 QQ 号、解析失败即拒绝；
  全员禁言/退群/解散不再被误当成用户目标。
- **工具**：`Copy` 受配额约束；`Grep` / `Stat` 不再标记已读；MCP stdio 单行与
  结果有大小上限；SSRF 过滤覆盖 IPv4-mapped / 6to4 / NAT64 / CGNAT。
- **子代理**：`docs` 与未开 `allow_send` 的自定义定义拿到只读 `API`，只能
  `api.list` / `api.help`；工具参数不合法时不再把 `input: null` 写进历史；
  error/refusal 路径补齐未执行的工具结果。
- **队列**：等待全局并发时不持 per-chat 状态锁；修复批次预约计数泄漏。
- **配置**：修复默认 `agent.compaction_model = ""` 无法通过校验的问题。

### 变更

- 配置 schema 拆到 `config/sections.rs`，API action 表拆到 `tools/actions.rs`，
  `app::build_agent` 的工具装配拆为 `assemble_tools`；行为不变。
- 移除未使用的 `AgentEvent::TurnEnd` / `ToolExecutionUpdate` 与
  `ModelDef::map_thinking_level`。
- 根目录旧资产 `prompt.md`、`llm.example.toml` 移到 `docs/archive/0.1/`。

### 文档

- 新增 `docs/guide/`：安装、配置、使用、人设与记忆、工具与 VFS、安全、故障排查、
  当前限制。
- `README.md` 重写并与代码对齐；`config.example.toml` 改为可运行的 0.2 样例，
  由 `tests/config_example.rs` 校验。
- 开发文档更新模块地图（迁移 v9、config/sections、tools/actions）与结构约定。

### 测试

- 新增回归测试：自动压缩、并发 append、索引错位、文件头加密、turn buffer 作用域、
  SSE UTF-8 分片、闸的 fail-closed/阈值/resume、只读 API、工具配对、队列预约、
  复制配额、Grep 不标已读、MCP 行上限、示例配置校验。
- `cargo test --all-targets`：288 项通过（2 项真实 API 测试默认忽略）；
  `cargo clippy --all-targets`：无警告。

## [0.2.0-alpha.12] - 2026-10-30

0.2 是完整重写：旧 0.1 的模块划分、数据结构与调用方式全部放弃，按
`.agents/plan/2026-10-28-rebuild-design.md` 重新实现。量化对比见
`docs/development/refactor-report.md`（复用 0 行，重写 97.5%，废弃 2.5%）。

### 新增

- **配置与凭据**：严格 `sbot.toml` schema（`version = 1`、未知字段拒绝、字段路径校验）、`SBOT__SECTION__KEY` 覆盖、`models.json` 模型清单、明文优先的 `auth.json`（掩码 Debug）。
- **存储**：SQLite 迁移 v1–v8、主密钥 `~/.sbot/key`、HKDF 域分离、XChaCha20-Poly1305、canary 校验、`SBOT_HOME`。
- **VFS**：POSIX 路径与 NFC、文本加密、1 MiB blob 分块、回收站、Read-before-Edit、配额与磁盘水位、种子布局。
- **Provider**：Anthropic Messages（自适应 thinking、effort）、OpenAI Chat Completions、OpenAI Responses、SSE 解析、工具参数按 index 组装、`Retry-After`、按模型回退链。
- **Agent 循环**：pi 风格内/外循环、事件流、steering / follow-up、配对的工具结果、`max_tokens` 带工具时不执行、输出闸、parallel/sequential 工具执行、refusal 进入 hold。
- **QQ**：NapCat 连接、事件归一化、内部标签剥离、准入门（拒收）、断线重连补消息（不触发回合）、JSONL 按群/私聊/月/日分区、索引重建、inbox 状态、发送账本。
- **安全**：规则层 + Jev `/v1/systemone` 分类器、永久 hold、`SECURITY_HOLD`、安全事件审计、主人/管理员目标保护、hold 期间拒绝全部 `napcat.*`。
- **会话**：加密 transcript 分段与 `prev_file` 链、turn buffer 崩溃恢复、CJK 感知压缩、回合边界切割、加密 `compaction_checkpoints`。
- **工具**：文件工具、动态视图（`/messages`、`/inbox`、`/sessions`、`/friends`、`/groups`、`/config`、`/system`）、`API` 命名空间、`Memory`（含 `memory_entries`）、`Skill`、`Download`（SSRF）、`WebSearch` / `WebFetch`、`Monitor`（qq/timer/internal、cron、batching、idle_only、backlog）。
- **子代理（swarm）**：`Agent` 工具的 spawn / message / check / wait / list；`general` / `explore` / `plan` / `docs` 内置类型；`/agents/*.md` 自定义定义；工具集强制为父代理子集；并发上限；独立上下文。
- **MCP**：可选、默认关闭；stdio JSON-RPC `initialize` / `tools/list` / `tools/call`；单个 `MCP` 工具；服务器启动失败只告警。
- **提示词**：v4 静态/动态边界模板、context hooks、每回合 runtime 上下文。
- **IPC**：Windows 命名管道（DACL）/ Unix UDS（0600）、长度前缀 JSON、幂等表。
- **CLI**：`start/stop/restart/status`、`ai`、`chat`、`pause/resume`、`security`、`model`、`effort`、`backup`、`doctor`。
- **备份与诊断**：`sbot backup` 在线 SQLite 快照（Busy/Locked 重试）+ 文件归档，拒绝写入被复制目录；`sbot doctor` 只读检查配置、canary、索引、孤儿 blob、记忆三方对账、权限与路径边界。
- **模型热切换**：`sbot model set` / `effort set` 通过共享 provider 槽位在下一回合生效。

### 变更

- CLI 行为与 0.1 不兼容；不提供旧数据迁移，0.2 从空存储开始。
- 文件能力改为 VFS，不再使用 `cap-std` 工作区。
- 白名单语义改为**拒收门**：被拒消息不进入任何存储。

### 已知限制

- **NapCat 协议实证**：修复 `call_api` 返回 `data` 的解包错误；新增 `tests/live_napcat.rs`（真实 WebSocket 收发）与 `examples/mock_onebot.rs`；完整管线用真实模型回复群消息并发送成功。
- **后台复盘**：空闲时从 transcript 提取记忆候选（只追加、来源 `self`）、更新 `/STYLE.md`；技能使用记录 + curator 归档长期未用技能（不删除，`pinned` 跳过）。
- **`per_chat` 会话隔离**：每个 chat 一条独立 transcript 链与历史，压缩/轮转/保留/崩溃恢复按 chat 执行；迁移 v8 增加 `scope` 列。
- **子代理嵌套**：`max_depth` 深度限制；`Agent message` 续聊复用原定义的提示、工具集与步数；`Agent spawn` 的 `monitor` 参数与 `deliver.to = <spawn_id>` / `agent:<name>` 把事件绑定到子代理实例。
- `/config` 写入支持白名单键并原子落盘；`[filter]` 与 `[queue]` 写入立即热更新（`max_concurrent` 的信号量上限需重启）。
- 真实 Chat 端点与真实 Jev 的测试默认 `#[ignore]`，需要环境变量显式启用。

## [0.1.0-alpha.2] - 2026-03-23

历史版本（0.1 系列）。以下内容已被 0.2 取代，保留用于对照。

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
