---
name: docs-layout
description: 文档放哪里：开发文档就是 .agents 里的 agent 记忆，使用文档是随二进制走的 skill；新建、移动或删除任何文档前先读
metadata:
  type: feedback
  scope: README、docs/、skills/、.agents/ 与二进制内嵌文档
  status: active
  last_verified: 2026-11-10
---

用户 2026-11-10 的规则（原话见 Evidence）：

- **开发者就是 agent，开发文档就是 agent 记忆，写在 `.agents/` 里。** 不要再建
  `docs/development/` 之类的开发文档目录；模块地图、维护规则、测试方法这类内容，要么在
  代码与注释里，要么是 `.agents/memory/` 的一条记忆。
- **使用者是人或 agent。** 给 agent 用的使用文档是 **skill**（部署、使用、运维）。
- **skill 的内容内嵌在二进制里，用命令查看**，内嵌文档与 skill 是同一份内容：skill 跟着
  二进制走，不会出现文档版本与程序版本对不上。

**Evidence:** 用户 2026-11-10：“docs/development还是没有删除，我都说了开发文档即agent记忆
即写在.agents中，开发者不就是agent吗？使用者才是人类或agent，所以还需要skill作为agent
部署sbot时的文档”；随后的要求：“设计部署、使用、运维等skill，同时在二进制内内嵌文档，
通过指令可查看，内容等同skill，相当于skill跟着二进制走避免版本不兼容”。

**Recheck when:** 用户改变文档的归属，或 skill / 内嵌文档的实现方式变化。

**Why:** 两份讲同一件事的文档一定会漂移；安装在别处的 skill 也会落后于二进制。让二进制
自带文档，agent 与人看到的永远是当前版本的说法。

**How to apply:** 改行为时改 skill 源文件（它同时就是内嵌文档），不要另写一份；开发时学到
的东西写进 `.agents/memory/`。实现落地后把具体路径与命令补到这条记忆里。
