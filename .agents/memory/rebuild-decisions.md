---
name: rebuild-decisions
description: ScapegoatBot 全新重构（2026-10-28）已确定的设计决策与实施入口；上下文压缩或新会话后从这里恢复
metadata:
  type: project
  scope: 当前重构分支，实施完成前有效
  status: active
  last_verified: 2026-10-28
---

ScapegoatBot 正在完全重构为全新项目，完整设计见 `.agents/plan/2026-10-28-rebuild-design.md`。

## 已定

- 单 agent、全局 transcript（类 OpenClaw）；不按群/私聊拆分；不设 `/identity`、`/memory`。
- IPC：Windows named pipe / Unix domain socket，无 TCP。
- VFS：`/AGENTS.md` 在根且默认空；`/friends/<qq>/`、`/groups/<gid>/` 动态目录；`/trash` 回收站；`/config` 配置视图；二进制 blob 分块加密。
- 工具 PascalCase：`Read`、`Write`、`Edit`、`LS`、`Grep`、`Glob`、`RM`、`MV`、`CP`、`Mkdir`、`Stat`、`API`、`Download`、`Skill`、`WebSearch`、`WebFetch`、`Agent`、`MCP`。
- 配置：`sbot.toml` + `models.json` + `auth.json`；TOML/JSON 键 snake_case；`auth.json` 不挂载 VFS，`config.toml` 零凭据。
- 事件 hook：`SessionStart`、`SessionEnd`、`PreCompact`、`PostCompact`、`MessageReceived`、`BeforeSend`、`Stop`、`ConfigChange`；无 `start_turn`。
- API 门禁：`trusted` / `auto` / `approve`，覆盖所有 API 调用；`auto` 默认每条消息做规则检查。
- 压缩：`reserve_tokens = 16384`、`keep_recent_tokens = 20000`、turn 边界切割、结构化检查点摘要、增量更新、`PostCompact` 重注入。
- Swarm 保留，默认开启；默认模型 `anthropic/claude-opus-5-5` + `high`。
- 默认协议 anthropic；chat/openai 同样实现 web 工具适配。
- 消息白名单改为模型可控的 `[filter]`；过滤只影响提示，不影响存储。

## 待定

- 加密方案仍为草案（XChaCha20-Poly1305 + Argon2id/OS keychain），待用户最终确认。
- `[gate].llm_review` 默认值待定（当前 false）。

**Evidence:** 设计文档 `.agents/plan/2026-10-28-rebuild-design.md` 与 2026-10-28 的对话结论。
**Recheck when:** 用户确认加密与 `llm_review` 默认值，或 Phase 0 开始后设计发生变化。
**Why:** 上下文压缩或新会话后需要快速恢复重构决策，避免重新讨论已定项。
**How to apply:** 实施前先读设计文档；按 Phase 0 → Phase 7 推进，不恢复旧代码结构。
