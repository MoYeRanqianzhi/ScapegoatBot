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
- 加密：内容加密、索引明文（不加密路径）；主密钥首次初始化时自动生成到 `~/.sbot/key`，无口令、无 keychain、无解锁步骤；所有状态统一放在 `~/.sbot`，复制该目录即完成迁移；威胁边界见设计文档 6.6。
- Prompt：以 `You are ScapegoatBot` 开头；所有模型面文本英文；静态前缀与 `__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__` 动态上下文分离；QQ 回复跟随会话语言；默认纯文本，话题需要或主人人设明显使用 Markdown 时跟随；不用 Unicode emoji、用 QQ 表情；owner 通知走 master QQ 私聊；不单独设“是不是机器人”策略，跟随主人自己的人设；工具描述、错误、压缩/摘要与子代理提示英文；指令优先级为内置 > owner context > skill > QQ/网络数据。
- 历史存储：transcript 每次压缩成功后轮转新分段；摘要块含 `previous_session`，system prompt 含 `current_session`；`/sessions` 只读映射分段并提供 `index.jsonl` 链式索引；内容只存加密 JSONL，视图由 VFS 动态生成，索引仅为可重建缓存。
- 消息存储：按群/私聊、月、日写入加密 JSONL（`messages/<scope>/<peer>/<YYYY-MM>/<DD>.jsonl`）；VFS `/messages` 动态渲染，磁盘不生成派生文件；索引可重建；记录全部消息，媒体只存 URL/引用。
- 消息发送者：agent 发送标记 `self`，主人手动发送与其他用户标记 QQ 号；每条消息携带 `chat_id`；runtime 注入 `owner_qq` 与 `self_sender`。
- 增长控制：VFS / 消息 / transcript 三类独立配额（默认 10 GB / 10 GB / 2 GB，0 不限制）与可选年龄保留（默认不按年龄删）；超限清理最旧完整月份/关闭分段，绝不删除当前；墓碑与用量可见。
- Grep：大目录按目录/文件分别计量预算；超预算时只跳过或截断该目录，其他目录照常返回；结果末尾只列超预算路径并标 `skipped` / `partial`，提示可忽略；规则写入 `Grep` 工具 description。
- 工具 PascalCase：`Read`、`Write`、`Edit`、`LS`、`Grep`、`Glob`、`RM`、`MV`、`CP`、`Mkdir`、`Stat`、`API`、`Download`、`Skill`、`WebSearch`、`WebFetch`、`Agent`、`MCP`。
- 配置：`sbot.toml` + `models.json` + `auth.json`；TOML/JSON 键 snake_case；`auth.json` 不挂载 VFS，`config.toml` 零凭据。
- 事件 hook：`SessionStart`、`SessionEnd`、`PreCompact`、`PostCompact`、`MessageReceived`、`BeforeSend`、`Stop`、`ConfigChange`；无 `start_turn`。
- API 门禁：`trusted` / `auto` / `approve`，覆盖所有 API 调用；`auto` 默认每条消息做规则检查。
- 压缩：`reserve_tokens = 16384`、`keep_recent_tokens = 20000`、turn 边界切割、结构化检查点摘要、增量更新、`PostCompact` 重注入。
- Swarm 保留，默认开启；默认模型 `anthropic/claude-opus-5-5` + `high`。
- 默认协议 anthropic；chat/openai 同样实现 web 工具适配。
- 消息白名单改为模型可控的 `[filter]`；过滤只影响提示，不影响存储。

## 待定

- `[gate].llm_review` 默认值待定（当前 false）。
- Prompt 评审（2026-10-29）：21 项见 `.agents/plan/2026-10-29-prompt-review.md`；P0-4 sender/chat 已落地；owner 通知=master QQ 私聊；P1-15 不单独设策略；按全部 P0/P1/P2 落地中。

**Evidence:** 设计文档 `.agents/plan/2026-10-28-rebuild-design.md` 与 2026-10-28 的对话结论。
**Recheck when:** 用户确认 `llm_review` 默认值，或 Phase 0 开始后设计发生变化。
**Why:** 上下文压缩或新会话后需要快速恢复重构决策，避免重新讨论已定项。
**How to apply:** 实施前先读设计文档；按 Phase 0 → Phase 7 推进，不恢复旧代码结构。
