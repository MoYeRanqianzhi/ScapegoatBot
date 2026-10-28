---
name: legacy-sbot-cli
description: ScapegoatBot 重构前的 sbot CLI、TCP 控制通道、AI 开关与启动检查设计，重构进程管理时对照
metadata:
  type: project
  scope: ScapegoatBot 重构前的 CLI 与控制通道（计划于 2026-03-09 编写并已实施）
  status: active
  last_verified: 2026-10-28
---

`docs/plans/2026-03-09-sbot-cli.md` 是已执行的计划：把单一 `main()` 改造为 `sbot` CLI + 守护进程 + 本地 TCP 控制通道。相关代码 `src/cli.rs`、`src/control.rs`、`src/control_server.rs`、`src/agent_state.rs` 在重构前均已存在。

## CLI

- 二进制名 `sbot`，clap 4 derive；全局 `-c/--config`（默认 `config.toml`）。
- 子命令：`start [-d/--daemon]`、`stop`、`restart [-d]`、`ai start|stop|restart`、`status`、`chat <message> [--group <id>...] [--private <id>...]`。
- 无子命令等同 `start` 前台运行。

## 控制通道

- 守护进程在 `127.0.0.1:<control.port>` 监听，默认端口 19800（`ControlConfig`）。
- 帧格式：4 字节大端长度 + JSON payload；接收端限制单帧 ≤1 MiB。
- `ControlRequest`：`Shutdown`、`Status`、`Ai { action }`、`Chat { message, group_filter, private_filter }`；`ControlResponse { ok, message }`。
- CLI 侧 `send_control()` 连接端口、发请求、打印响应；连接失败提示确认 sbot 是否运行。

## 运行模型

- `AgentState` 用 `AtomicBool` 控制 AI 是否活跃；`ai stop` 仅暂停 LLM 调用，消息收集/存储/风格解析继续。
- `run_daemon()`：初始化 tracing → 加载配置/白名单/存储/Skill → 创建 AgentState 与 `broadcast` 关闭通道 → 启动控制服务端 → 连接 NapCat → `select!` 等待 Ctrl+C 或远程 shutdown。
- `spawn_daemon()`：重新 spawn 自身 `start -c <config>`；Windows 用 `CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS`，Unix 用 `setsid()`，stdio 全部 null。
- `chat` 指令设计为注入 Agent 会话的 user message，可带本轮临时白名单（必须是全局白名单子集）。
- `ai_boot` 标记（`PromptConfig`）在 start/restart 时向系统提示词注入"启动检查"段：扫描离线消息、按过期/已解答/闲聊结束等规则筛选、输出判断、积压时分批、完成后进入正常模式。

**Evidence:** 归档自 `docs/plans/2026-03-09-sbot-cli.md`；删除前内容可在提交 `f49019d` 中检索：`git show f49019d:docs/plans/2026-03-09-sbot-cli.md`。
**Recheck when:** 重构后的 CLI/进程管理方案确定后，以新实现为准；尤其确认控制端口、帧格式与 daemon 平台分支是否保留。
**Why:** 该计划记录了几项非显而易见的取舍：AI 暂停不停消息收集、启动检查的筛选规则、跨平台 detach 参数、长度前缀帧协议。
**How to apply:** 重构 CLI 时复用仍成立的产品行为（暂停语义、chat 临时白名单、启动检查），但不必保留旧的模块拆分和 IPC 实现。
