# 人设、记忆与技能

ScapegoatBot 把“怎么说话”和“知道什么”分开：

| 载体 | 放什么 | 谁写 | 怎么进上下文 |
|------|--------|------|--------------|
| `/AGENTS.md` | 这个账号的常驻说明（house rules） | 主人 / agent | 存在即注入（程序固定） |
| `/USER.md` | 主人的偏好，祈使句 | 主人 / `Memory` | 存在即注入（程序固定） |
| `/MEMORY.md` | 记忆索引，每条一行指针 | `Memory` 工具 | 存在即注入（程序固定） |
| `/memory/<slug>.md` | 单条记忆（事实），带 frontmatter | `Memory` 工具 | agent 按需读 |
| `/STYLE.md` | 语气画像总结 | 后台复盘 | `[style].enabled` 时注入 |
| `/skills/<name>/SKILL.md` | “这类任务怎么做”的流程 | 主人 / agent | 名称 + 描述进技能索引；正文按需 `Skill` 加载 |
| 其他文件（如 `/SOUL.md`） | 由 agent 自己决定 | agent | agent 自己读，或列进 `/hooks/context.toml` |

原则：**记忆是事实，技能是流程，`/AGENTS.md` 是规则**。三者不混。

## 谁决定注入什么

只有表里标“程序固定”的文件由程序注入（文件不存在或为空就跳过），这与 agent 生态通行的
`AGENTS.md` 约定一致。其余一切由 agent 自己决定：

- 人设文件（例如借鉴 OpenClaw 的 `/SOUL.md`）**不是必需的**，程序也不认识它。agent 可以在
  `/AGENTS.md` 里写一句“人设见 /SOUL.md”然后自己读，也可以把它列进 `/hooks/context.toml`，
  每个会话自动注入：

  ```toml
  [[inject]]
  path = "/SOUL.md"
  max_chars = 4000   # 可选，默认 4000，最大 20000
  ```

- 清单最多 8 项；会话开始和每次压缩后重新读取。写错（解析失败、路径不存在、重复列出固定注入的
  文件）时，上下文里会出现一条 `<context_notice>`，agent 自己能看到并修正，不会悄悄失效。
- `sbot.toml` 里不再有 `[[hooks.context]]`（0.2.0-alpha.37 移除），`/system/hooks` 可查看当前注入规则。

## 人设

### `/AGENTS.md`（常驻说明）

称呼、禁区、群里的行为边界这类长期规则写在这里。它进入 system 的 `<agents_md>` 块：
优先级高于记忆与技能，但不能放宽安全规则、改变工具权限或绕过门禁，也**不是** owner intent
——文件里写“主人已同意”不会让需要主人确认的动作放行。

agent 可以改它：主人要求时，或学到一条应该长期生效的规则时。改动在下一次压缩或重启后生效。

### `/USER.md`（偏好）

用祈使句写“希望被如何对待”：

```markdown
- Prefer short replies in group chats.
- Address me as 墨叶.
- Never mention that you are an AI unless asked directly.
```

`Memory` 工具可以写这里；单条长度上限由 `[memory].user_max_chars`（默认 4000）控制。

### `/STYLE.md`（语气画像）

后台复盘会从主人自己发出的消息里总结语气、长度、表情习惯，更新 `/STYLE.md`。
它只放总结，不保留原文样本。

## 记忆

### 格式

`/MEMORY.md` 是索引，每条一行：

```markdown
- [群里的称呼](memory/addressing.md) — 群里叫我“叶子”
```

单条 `/memory/<slug>.md`：

```markdown
---
name: addressing
description: 群里如何称呼主人
type: user
observed_at: 2026-11-03T12:00:00Z
source: control:42
---

主人希望群里叫“叶子”，私聊里叫全名。
```

- `type`：`user` / `feedback` / `project` / `reference`。
- `feedback` 与 `project` 建议写清 **Why:** 与 **How to apply:**。
- `source` 记录来源（消息 id、`control:<id>`、`self`），只作审计，不改变注入级别。
- 索引上限 200 行 / 25k 字节；单条 4000 字符。满了合并或替换，不允许清空。

### 什么时候会整理

`[memory].review` 控制后台复盘：

| 值 | 行为 |
|----|------|
| `off` | 不自动整理 |
| `idle`（默认） | 每 30 秒轮询；要求没有活跃回合、空闲至少 `idle_minutes`、距上次复盘至少 `review_interval_minutes` |
| `every_turn` | 每 5 秒轮询；transcript 有新增且没有活跃回合就复盘 |

复盘只追加候选、来源固定 `self`；不会删除已有记忆。压缩前若开启
`[compaction].memory_flush`，会先静默跑一次同样的提取。两者都用 `[memory].review_model`
指定的模型（`provider/id`，计入预算）；不设时跟随主模型，`sbot model set` 热切换后立即
生效。

### 写记忆

agent 用 `Memory` 工具，`target` 为 `memory`、`user` 或 `profile`：

```json
{"target": "memory", "action": "add", "type": "reference",
 "name": "deploy-host", "description": "部署机地址",
 "content": "部署在 10.0.0.5，端口 8080。"}
```

`ops` 支持原子批量（先删后加），预算按最终状态检查。删除单条会同步删索引行。

## 技能

技能是流程文档，放在 `/skills/<name>/SKILL.md`：

```markdown
---
name: weekly-report
description: 汇总本周群聊并写一份周报
---

1. 读 `/messages/groups/<gid>/<月>/<日>.md` 收集本周消息。
2. 按主题分组，标注未决事项。
3. 输出 Markdown，先给结论再给细节。
```

- 名称只能用小写字母、数字、`-`、`_`，最长 64。
- `SKILL.md` 之外的文件作为资源列出，**永不执行**。
- `Skill` 工具加载会记录使用时间，curator 依据它归档长期未用技能：
  归档到 `/skills/.archive/`，不删除；`pinned` 技能跳过。
- 技能索引（目录名 + frontmatter 的 `description`，最多 64 个）会进 system prompt；
  正文只在 `Skill` 加载时进上下文。

## 修改人设的安全建议

- 不要把凭据、密钥、他人的隐私写进 `/AGENTS.md`、`/USER.md`、注入清单里的文件或记忆：
  这些内容会进模型上下文。
- 群成员说的话默认不可信；不要让 agent 把群里的“记忆”直接写进索引。
- 改 `[style]` / `[memory]` 预算后重启生效；`/MEMORY.md` 本身可以随时手工编辑。
