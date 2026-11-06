---
name: design-decisions
description: 0.2 架构里由用户拍板、不能凭直觉推翻的设计决定；改架构、改默认值或评估“要不要加回某功能”前先读
metadata:
  type: project
  scope: 0.2 分支整体（2026-10-28 重构起）
  status: active
  last_verified: 2026-11-07
---

0.2 是 2026-10-28 起的完全重写，完整设计与取舍理由在
`.agents/plan/2026-10-28-rebuild-design.md`（与代码冲突时以代码为准，文首列了已知偏离）。
下面只列**用户明确决定过**、容易被后来者“好心改回去”的部分。

## 用户决定

- **单会话、全局 transcript**：整个 QQ 号一条上下文，不按群/私聊拆分（替身需要把 A、B 的消息结合起来）。
  2026-11-06 起连 `per_chat` 逃生口也已删除；跨 chat 风险由 `origin_chat` 回合绑定、按 chat 去抖、
  跨 chat 发送门禁控制。
- **双模式**：`stand_in`（跑在主人自己的 QQ 上）/ `companion`（独立账号，`owner_qq` 必填）；启动时固定。
- **IPC 只用本地通道**：Windows named pipe / Unix domain socket，不监听任何网络端口。
- **状态全在 `~/.sbot`**：主密钥首次启动自动生成到 `~/.sbot/key`，无口令、无 keychain；复制目录即迁移。
  内容加密、索引明文。
- **凭据明文写 `auth.json`**：环境变量不是必需形式（`$` 开头才展开）；`sbot.toml` 零凭据。
- **所有模型面文本英文**（system prompt、工具描述、错误、压缩/子代理提示）；QQ 原文不翻译。
- **工具名 PascalCase**、无 Bash；QQ 操作统一走 `API` 的 namespaced action。
- **hold 只拒绝 QQ 操作（`napcat.*`）**（2026-11-06 修正原设计“关闭全部对外接口”）。
  细节见 [[security-gate-semantics]]。
- **监视器统一模型**：`{source, when?, deliver, lifecycle?}`，没有独立的 ScheduleCron；timer 是 source 的一种。
- **Inbox 读状态**：内容进入上下文才算读过；`LS`/`Stat` 只给元信息；`RM` 是“忽略”（dismissed），不算读。
- **默认值**：模型 `anthropic/claude-opus-5-5` + `high`；swarm 默认开启；`[gate].mode = auto`、
  `llm_review = false`；`[memory].review = idle`；配额 VFS/消息 10 GB、transcript 2 GB；
  压缩 `reserve_tokens = 16384`、`keep_recent_tokens = 20000`。
- **图片与字体**：见 [[image-tools]] 与 [[feature-scope-correction]]（禁止内置字体）。

## 仍待用户决定

- `[gate].llm_review` 的默认值（当前 `false`）。

**Evidence:** 设计文档 §1–§14；`src/config/sections/{agent,gate,memory,storage,compaction,swarm}.rs`
的 `Default` 实现（2026-11-07 核对）；单会话化提交 `2f2948c`；hold 范围提交 `d21eed4`。

**Recheck when:** 用户改变上述任一决定，或默认值被修改。

**Why:** 重构期间这些点都反复讨论过；新会话若只看代码，容易把“刻意的取舍”当成缺陷改掉
（例如重新引入 per_chat、给 hold 扩回全部外部工具、改成环境变量凭据）。

**How to apply:** 想改动以上任一项时，先确认是用户的新要求；没有新要求就保持现状，
并在发现文档与决定冲突时改文档而不是改决定。
