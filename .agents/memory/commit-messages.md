---
name: commit-messages
description: 提交说明里不能带 Claude Code 的共同作者行；写提交、合并或再次重写历史前读
metadata:
  type: feedback
  scope: 本仓库所有分支与 tag 的提交说明
  status: active
  last_verified: 2026-11-10
---

提交说明不带 `Co-Authored-By: Claude Code <noreply@anthropic.com>` 这一行（任何 agent 的
共同作者 / “Generated with” 署名同理）。

2026-11-10 已按用户要求重写本地历史：2026-10-30 的 14 个提交带这一行，全部删除；因为它们
在历史很靠前的位置，之后所有提交、15 个分支与 40 个 tag 里的 37 个哈希都变了（文件内容、作者、
时间、标题逐一比对过，没有变化）。

**Evidence:** 用户 2026-11-10：“对请求历史中有‘Co-Authored-By: Claude Code
<noreply@anthropic.com>’内容的全部去掉这一行，重写历史”。重写后
`git log --branches --tags --format=%B | grep -c Co-Authored-By` = 0；旧历史的完整备份是
`G:\sbot-pre-coauthor-rewrite-2026-11-10.bundle`。

**Recheck when:** 远端被强推成新历史之后（那时备份可以删，`.agents/TODO.md` 的对应项关闭）；
或用户改变署名规则。

**Why:** 用户不要这类署名出现在项目历史里。

**How to apply:** 提交时不要加共同作者行，即使客户端默认会加。远端 `origin/main` 与
alpha.33 及之前的 tag 仍是旧历史，强推需要用户授权（见 `.agents/TODO.md`）。重写历史后，
记录里引用的提交哈希要一起换（2026-11-10 换过 4 处）。
