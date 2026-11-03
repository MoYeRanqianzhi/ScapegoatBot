# 项目记忆索引

- [重构决策（进行中）](memory/rebuild-decisions.md) -- 全新重构的已定设计、待定项与实施入口
- [System Prompt 评审（2026-10-29）](plan/2026-10-29-prompt-review.md) -- 21 项对比与改写建议，已并入 v4
- [System Prompt v3 提案（2026-10-29）](plan/2026-10-29-prompt-v3-proposal.md) -- 第二轮审查：4 处 P0 矛盾与 N1–N15，已并入 v4
- [Hermes 参考评审（2026-10-29）](plan/2026-10-29-hermes-review.md) -- 长期运行 agent 的分层 prompt、记忆/技能回路与 v4 全文
- [设计复审二（2026-10-29）](plan/2026-10-29-design-review-2.md) -- OpenClaw/Grok Build 对照：并发、长期运行、记忆可信度与 27 项发现
- [Claude Code 记忆格式（2026-10-29）](plan/2026-10-29-claude-memory-format.md) -- 索引 + 单条记忆 + 四类 + 新鲜度，及本项目适配
- [子代理审查汇总（2026-10-29）](plan/2026-10-29-subagent-review.md) -- 架构/提示词/安全三份评审、P0 修复与最终评估
- [Prompt Catalog（2026-10-29）](plan/2026-10-29-prompt-catalog.md) -- 工具/压缩/子代理/门禁/复盘/错误提示的英文稿
- [安全分类器参考（2026-10-30）](plan/2026-10-30-step-guard-research.md) -- Claude 的输入/输出分类器、Jev 单遍决策模型及其能力边界
- [0.2 代码审查（2026-11-03）](plan/2026-11-03-code-review.md) -- alpha.12 的 P0/P1 缺陷、上帝结构、注释与文档漂移及修复顺序
- [发布状态（2026-11-04）](memory/release-status.md) -- alpha.14 已实现能力、真实边界与验证基线
- [安全闸语义](memory/security-gate-semantics.md) -- shadow/enforce、fail-closed、hold 覆盖与 resume 复核条件
- [存储不变量](memory/storage-invariants.md) -- append 串行、索引校验、文件头加密、turn buffer scope
- [文档入口与示例校验](memory/docs-entrypoints.md) -- guide/development/archive 的职责与 config.example 的测试
- [旧架构总览（重构前）](memory/legacy-architecture.md) -- 模块结构、数据流、会话/沙箱/MCP/Swarm/安全设计
- [旧工具参考（重构前）](memory/legacy-tools-reference.md) -- 43 个内置工具的分组、数量与实现位置
- [旧消息状态设计（重构前）](memory/legacy-message-state.md) -- Raw/Wrapped/Compact/Linked 判定与 CQ XML 格式
- [旧 sbot CLI 与控制通道（重构前）](memory/legacy-sbot-cli.md) -- 子命令、TCP 控制协议、AI 开关与启动检查
