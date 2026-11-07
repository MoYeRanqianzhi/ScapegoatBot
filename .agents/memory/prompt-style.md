---
name: prompt-style
description: 用户对模型面文本（system prompt、工具描述、子代理提示）的写法要求；写或改任何提示词前先读
metadata:
  type: feedback
  scope: assets/system_prompt_*.template.txt、src 里所有模型可见文本
  status: active
  last_verified: 2026-11-07
---

提示词以 Claude Code 为范本，学它的**哲学感写法与比喻**，而不是罗列规则：

- 讲清“为什么”，让模型能在没写到的情况下自己推理；少用全大写和堆叠的 MUST。
- 多用比喻帮模型建立心智模型。例：子代理是“刚走进房间、什么都不知道的新同事”，
  委派时要像给这样的人写交接说明；工具调用是行动而不是表态；hold 像熔断器。
- **核心 API 要写进 system prompt**：怎么发消息（`napcat.send_group_msg` /
  `send_private_msg`、消息段数组、回复引用、@）这类每回合都会用到的内容，不能只靠
  模型去 `api.help` 查。取舍原则：高频且出错代价高的写进提示词，长尾参数留给 `api.help`。
- 提示词里的每一句“能/不能”都必须和代码一致；写之前核对实现（v4 曾写“不能写 /AGENTS.md”，
  代码并没有、用户也不要这条限制）。
- 模型面文本一律英文；参考原文在 `.ref/claude-code-system-prompts/`、`.ref/CL4R1T4S/ANTHROPIC/`。

**Evidence:** 用户 2026-11-07 原话：“提示词要求是参考claude code，模仿其中哲学感写法，还有其中的比喻……
比如将子代理比喻为什么都不知道的员工之类的，多用比喻更容易让agent理解”；
“还有一些重要的api也需要写进system prompt中，比如说怎么发消息之类的”。v4 由较弱模型写成，用户评价“写得不好”。

**Recheck when:** 用户对提示词风格给出新要求。

**Why:** 规则清单式提示词在长尾情况下失效，比喻和理由能迁移；常用 API 不写进提示词会让模型每回合多一次查询或写错参数。

**How to apply:** 改提示词前读本条与 Claude Code 参考原文；每条行为声明对照代码核实；
常用动作给出可直接照抄的最小示例。
