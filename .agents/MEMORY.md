# 项目记忆索引

- [设计决定](memory/design-decisions.md) -- 用户拍板的架构取舍（单会话、本地 IPC、明文 auth.json、hold 只拦 QQ 等），改架构前先读
- [安全闸语义](memory/security-gate-semantics.md) -- shadow/enforce、fail-closed、llm 后端装入、hold 只拦 napcat.*、resume 复核
- [存储不变量](memory/storage-invariants.md) -- append 串行、(chat, message_id) 去重、索引校验、文件头加密、迁移 v12
- [图片工具 image.*](memory/image-tools.md) -- 5 个 action、系统字体约束、依赖与模块命名坑
- [功能范围纠正](memory/feature-scope-correction.md) -- 不要把代码限制升级成需求；禁止内置字体
- [发布状态](memory/release-status.md) -- 当前测试基线、tag/release 状态与未实测能力
- [审查整改计划（2026-11-07）](plan/2026-11-07-review-remediation.md) -- 四份代码审查的已修/待修项与顺序
- [重构总设计](plan/2026-10-28-rebuild-design.md) -- 0.2 设计依据与取舍理由；与代码冲突时以代码为准
- [Prompt Catalog](plan/2026-10-29-prompt-catalog.md) -- 模型面文本（工具描述、压缩、子代理、门禁提示）的英文稿
- [安全分类器调研](plan/2026-10-30-step-guard-research.md) -- Jev 单遍决策模型的能力边界与真实 API 结果
- [重构基线](plan/2026-10-30-refactor-baseline.md) -- 0.1 代码量基线，`docs/development/refactor-report.md` 的数据来源
- [0.2 代码审查快照（2026-11-03）](plan/2026-11-03-code-review.md) -- alpha.12 时的问题清单；修复前快照，不代表现状
