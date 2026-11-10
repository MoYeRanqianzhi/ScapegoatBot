---
name: design-decisions
description: 0.2 架构里由用户拍板、不能凭直觉推翻的设计决定；改架构、改默认值或评估“要不要加回某功能”前先读
metadata:
  type: project
  scope: 0.2 分支整体（2026-10-28 重构起）
  status: active
  last_verified: 2026-11-08
---

0.2 是 2026-10-28 起的完全重写，完整设计与取舍理由在
`.agents/plan/2026-10-28-rebuild-design.md`（与代码冲突时以代码为准，文首列了已知偏离）。
下面只列**用户明确决定过**、容易被后来者“好心改回去”的部分。

## 用户决定

- **无人值守是第一前提**（用户 2026-11-07 强调，之前多次说过但没记下）：启动后理想情况下
  不再有任何人工干预，**主人只通过 QQ 与 agent 交互**，不会去敲 CLI。凡是“必须主人在
  主机上执行命令才能恢复”的设计都等于永远卡死。由此推出：
  - **hold 必须能由 agent 自己解除**（`agent.resume`，由 runtime 用同一套闸复核当前上下文）。
    不要改成“只允许控制通道解除”；要加强的是复核本身，不是收走解除权。0.2.0-alpha.37 起
    复核对象是当前已提交上下文（不是当初的危险原文，那样永远解不开）；`critical` 不再暂停
    新回合，而是 hold + 下一回合前丢弃上下文。
  - **主人的消息不走特权通道**（用户 2026-11-08 否决了 alpha.37 的“主人之声/私线”实现：
    “不优雅……假如 agent 今天闹脾气了就是不要看主人消息呢？……不能这样实现”）：
    非 agent 发出的消息（包括同账号上主人手打的）一律进 inbox，由默认预设 monitor
    `wake-on-owner` 唤醒；agent 可以改写或删除这个 monitor。区分 agent 发的与手打的靠
    发送账本 `src/qq/ledger.rs`（误判只偏向“是 agent 发的”，防止伪造主人意图）。
    不要再加“主人消息绕过过滤/强制唤醒/私线账号”之类的专门通道。
  - CLI（`sbot gate approve`、`sbot security resume` 等）只是补充通道，不能是唯一出路。
- **`/AGENTS.md` agent 可以写**（用户 2026-11-07 纠正）。原设计与 v4 提示词里
  “`/AGENTS.md`、`/SOUL.md` 只有主人能写”是错的，不要再实现写保护。
- **上下文注入的分工**（用户 2026-11-07）：
  - `/AGENTS.md` 是业界统一的 agent 约定：**存在就由程序自动注入**，不存在也没关系。
    这是程序行为，不是可被配置删掉的普通 hook。
  - `/SOUL.md` 不是必需品，只是借鉴 OpenClaw 记录角色设定的一种做法。程序不预置、不指定注入；
    agent 可以在 `/AGENTS.md` 里写“去读 /SOUL.md”，也可以给自己加一条 hook 自动注入——
    这是 agent 自己的行为与决定。其他文件同理：agent 自己读，或自己设置 hook。
  - 因此 agent 必须能管理自己的上下文 hook（增删），程序只硬编码 AGENTS.md。
    实现（0.2.0-alpha.37）：`src/prompt/context.rs`；agent 自管清单是 VFS 的
    `/hooks/context.toml`（`[[inject]] path/max_chars`），配置里的 `[[hooks.context]]` 已删除。
  - 记忆系统自身的文件（`/MEMORY.md` 索引、`/USER.md`）与风格档案、技能索引随各自功能注入，
    属于这些功能的设计，不是“程序指定的人设文件”（主代理的理解，未与用户逐字确认）。
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
- **工具命名分层**：`API` 的 action 名要体现完整归属。图片处理是“本地工具 → 媒体 → 图片”，
  所以是 `local.media.image.*`，不能直接 `image.*` 开头（用户 2026-11-07：“漏了前缀”）。
  新增 action 时先想清楚它在哪一层。
- **图片与字体**：见 [[image-tools]] 与 [[feature-scope-correction]]（禁止内置字体）。
- **提示词风格**：见 [[prompt-style]]。
- **版本与发布**：见 [[release-workflow]]。

## 仍待用户决定

- `[gate].llm_review` 的默认值（当前 `false`）。

**Evidence:** 设计文档 §1–§14；`src/config/sections/{agent,gate,memory,storage,compaction,swarm}.rs`
的 `Default` 实现（2026-11-07 核对）；单会话化提交 `6768da8`；hold 范围提交 `b766619`；
用户 2026-11-07 原话：“这是个自主运行的agent，理想情况下启动后就不会干预，主人只会通过qq进行交互，
所以必须自主解除”“AGENTS.md可以写”“AGENTS.md是程序决定的是一定要注入的（如果存在），其他文件靠agent自己读或者自己设置hook注入”“这是本地工具类下属的媒体类的图片处理工具……不能直接image.开头”。

**Recheck when:** 用户改变上述任一决定，或默认值被修改。

**Why:** 重构期间这些点都反复讨论过；新会话若只看代码，容易把“刻意的取舍”当成缺陷改掉
（例如重新引入 per_chat、给 hold 扩回全部外部工具、改成环境变量凭据）。

**How to apply:** 想改动以上任一项时，先确认是用户的新要求；没有新要求就保持现状，
并在发现文档与决定冲突时改文档而不是改决定。
