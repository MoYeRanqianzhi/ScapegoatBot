# 工具与虚拟文件系统

agent 不直接接触宿主文件系统。它看到一个 POSIX 风格的虚拟文件系统（VFS），
所有路径以 `/` 开头，深度 ≤16、长度 ≤255，`.` / `..` / 反斜杠 / 盘符一律拒绝。

## 种子布局

```text
/AGENTS.md        常驻说明（存在即注入；主人和 agent 都可以写）
/USER.md          偏好（祈使句）
/MEMORY.md        记忆索引
/memory/topics/   记忆分类（可选）
/memory/notes/    其他笔记（可选）
/agents/          自定义子代理定义（<name>.md）
/skills/          技能（<name>/SKILL.md）
/trash/           回收站：RM 默认移到这里
/tmp/             临时文件
```

`/hooks/context.toml` 不是种子文件：agent 想让别的文件（例如 `/SOUL.md`）每个会话自动注入时
自己创建，格式见 [人设、记忆与技能](persona-memory.md)。

`/quarantine/` 由 agent 在可疑内容上创建。

## 动态视图

这些路径不落盘，读取时由运行时生成：

| 路径 | 内容 |
|------|------|
| `/messages` | 未读摘要 |
| `/messages/groups/<gid>/<YYYY-MM>/<DD>.md` | 某天群消息 |
| `/messages/private/<qq>/<YYYY-MM>/<DD>.md` | 某天私聊消息 |
| `/inbox` | 未读来源列表 |
| `/inbox/<chat>` | 某 chat 的未读 |
| `/inbox/<chat>/<message_id>.md` | 单条消息 |
| `/sessions` | transcript 分段列表 |
| `/sessions/<file>` | 某段内容 |
| `/sessions/index.jsonl` | 分段链索引 |
| `/friends/<qq>.md` | 好友资料（NapCat 实时） |
| `/groups/<gid>.md` | 群资料 |
| `/groups/<gid>/<qq>.md` | 群成员资料 |
| `/config` / `/config.toml` | 配置视图 |
| `/system` / `/system/<section>` | 运行状态、hold、用量 |

查询参数只允许出现在这些路径上，静态文件带查询直接报错：

```text
/messages/groups/123/2026-11/05.md?filter=contains:deploy%20from:10001&order=desc&limit=20
/inbox?filter=mentions_self&limit=10
```

视图先按 `filter` 筛选，再按 `order` 排序（`asc` 旧的在前，默认；`desc` 新的在前），
然后跳过 `offset` 条、最多取 `limit` 条（默认 50、上限 500）。结果没给全时末尾附一行
说明：可翻页的视图写明下一页的 `offset`，`/inbox` 写明还剩多少未读。

- `Read` 只在消息集合上接受查询串：`/inbox`、`/inbox/<chat>`（及其 `index.md`）、
  `/messages`（未读摘要，`filter` 按条件数每个 chat 的未读）、`/messages/.../<DD>.md`。
  其余单文档视图（`/config.toml`、`/system/*`、单条消息等）带查询串报错。
  `Read` 的按行 `offset` / `limit` 参数不用于动态视图，传了会报错，翻页写在查询串里。
- `LS` 在任何动态目录上接受查询串，`filter` 只认 `contains:`（比较条目路径），
  其他条件报错。
- `RM` 带查询串时与 `Read` 同样筛选、分页，只忽略这一页，并报告还剩多少满足条件。

`filter` 是空白分隔的若干条件，全部满足才算匹配（实现：`src/vfs/filter.rs`）：

| 条件 | 含义 |
|------|------|
| `unread` | 等同 `state:unread` |
| `state:<unread\|read\|dismissed>` | 收件箱状态；不在收件箱里的消息（本账号发出的）不满足任何 `state:` |
| `from:<qq>` | 发送者 QQ 号 |
| `mentions_self` | @ 了本账号或 @全体成员 |
| `contains:<text>` | 正文包含子串，不区分大小写；含空白时加双引号 `contains:"a b"`，引号内 `\"`、`\\` 转义 |
| `after:<YYYY-MM-DD>` / `before:<YYYY-MM-DD>` | 本地日期不早于 / 不晚于这一天（都包含当天） |

条件写错（未知条件、`from:` 不是数字、不存在的日期、引号没闭合）在解析时报错，
不会被当成“不匹配”返回空结果。路径里的空白写成 `%20`，引号写成 `%22`。

### 读状态语义

- `Read` 进来的内容算“读过”，按条标记已读。
- `Grep` **不**标记：只扫描、不读入上下文。
- `LS` / `Stat` 只给元信息，不标记。`Stat` 对动态文件给出类型与大小，对 `/inbox/<chat>`
  这类目录给出未读数。
- monitor 投递同样算读入。

## 内置工具

文件工具（11 个）：

| 工具 | 作用 | 关键参数 |
|------|------|----------|
| `Read` | 读文本（`offset` 从 0 开始的行号） | `path`、`offset`、`limit` |
| `Write` | 写文本（覆盖需先完整读过） | `path`、`content` |
| `Edit` | 精确替换（需先读过且版本未变） | `path`、`old_text`、`new_text`、`replace_all` |
| `LS` | 列目录 | `path` |
| `Grep` | 正则搜索内容，大目录按子节点计量预算 | `pattern`、`path`、`file_ext`、`case_insensitive` |
| `Glob` | 按 glob 找文件 | `pattern`、`path` |
| `RM` | 移入 `/trash`（或 `permanent: true` 彻底删；目录需 `recursive`） | `path`、`recursive`、`permanent` |
| `MV` / `CP` | 移动 / 复制（复制受配额约束） | `from`、`to` |
| `Mkdir` | 递归建目录 | `path` |
| `Stat` | 类型、大小、版本、hash、子节点数 | `path` |

其余工具：

| 工具 | 说明 |
|------|------|
| `API` | 唯一的 QQ 动作入口；`api.list` / `api.help` 查表 |
| `Memory` | `/USER.md`、`/memory/*`、`/MEMORY.md` 的唯一写入口 |
| `Skill` | 加载 `/skills/<name>/SKILL.md` 与资源清单 |
| `Download` | HTTP(S) 下载到 VFS，带 SSRF/域名/大小限制；边收边加密暂存，不把整个文件留在内存里。文本类型（`text/*`、JSON、XML、JS）且不超过文本上限、是合法 UTF-8 时存成文本节点，否则存成二进制 |
| `WebSearch` / `WebFetch` | 搜索 / 抓取；由服务端工具或你配置的后端执行 |
| `Monitor` | 增删查事件订阅，手动触发一次 |
| `Agent` | 生成/续聊/查询/等待子代理 |
| `MCP` | 调用已连接的 MCP 服务器工具（可选） |

`API` 的 action 名是分层的，第一段说明效果落在哪里：

- `napcat.*`：QQ 操作，唯一会被别人看到的动作。hold 期间全部拒绝。
- `local.*`：只读写 VFS 的本地工具。目前是 `local.media.image.*`（本地工具 → 媒体 → 图片）：
  `info`、`transform`（裁剪/任意角度旋转/缩放/缩略）、`convert`（PNG/JPEG/WebP，quality
  只作用于 JPEG）、`text`（字体、位置、颜色、透明度）、`fonts`（列出系统字体）。不内置字体，
  `text` 使用系统安装的字体；没有字体时 `fonts` 返回空列表，`text` 返回明确错误。
- `agent.*`：`status`、`resume`、`quarantine`、`compact`、`clear_context`。
- `api.*`：`list`（`namespace` 按层级前缀过滤，如 `local.media`；另有分类/关键词/分页）、
  `help`（单个 action 的 schema）。

`Read` 读取 VFS 图片时会把图片作为 tool result 的 image attachment 交给 provider；
图片处理工具的输出同样适用；图片附件会随 transcript 持久化，重启后仍可恢复
（受 3.5 MB / MIME 限制）。超过 3.5 MB 的图片先缩成 JPEG（最长边从 2000px 起逐级
缩小，直到放得下）；超过 64 MB 或单边超过 8192px 的图片不解码。解码与编码在阻塞
线程池里进行，`Read`、`local.media.image.*` 与 MCP 截图共用 `src/media.rs` 的上限。`API` 的响应若包含 `base64` / `image` 字段，也会自动
转成图片附件，而不是只把 JSON 塞进文本。

发送、请求处理、管理动作都在表里标注了作用域与是否 destructive；目标为 owner/master
的踢人/禁言等动作无条件拒绝。

## 配额与清理

| 配额 | 默认 | 行为 |
|------|------|------|
| VFS 总量 | 10 GiB | 超限拒绝写入（`0` = 不限） |
| 节点数 | 100000 | 超限拒绝新建 |
| 单文本 | 1 MiB | 超限拒绝 |
| 单二进制 | 512 MiB | 超限拒绝，blob 按 1 MiB 分块加密 |
| 消息归档 | 10 GiB | 超限删最旧完整月份，绝不删当前月 |
| transcript | 2 GiB | 超限关闭最旧分段 |
| 磁盘余量 | ≥1 GiB | 低于阈值拒绝写入 |

回收站不计入上表的 VFS 总量，但有自己的上限：字节不超过总量的 10%、节点不超过节点上限的
10%，条目保留 30 天。移入时就在同一事务里执行上限（超出时先清最早移入的；单个条目自己就超限
时直接永久删除，`RM` 会如实报告），过期条目由每天一次的后台清理移除。从回收站恢复（`MV` 出
`/trash`）按活跃配额检查，放不下就拒绝。墓碑与用量可在 `/system` 查看；`/system/events` 列出最近 100 条 notice/request/meta/unknown
原始事件（正文已解密）。

## 子代理

`Agent` 工具的动作：

```json
{"action": "spawn", "task": "...", "agent": "explore"}
{"action": "message", "id": "sa-1", "text": "..."}
{"action": "check", "id": "sa-1"}
{"action": "wait", "id": ["sa-1"], "timeout_secs": 60}
{"action": "list"}
```

- 内置类型：`general`、`explore`、`plan`、`docs`。
- 自定义：`/agents/<name>.md`，frontmatter 支持 `tools`、`model`、`thinking_level`、
  `max_steps`、`allow_send`。
- 工具集强制是父代理的子集；没有 `allow_send` 时 `API` 只读。
- 子代理有独立上下文，结果回给父代理；`max_depth` 限制嵌套。
- 要让子代理持续接收事件，用 `Monitor add`，`deliver.to` 写它的 id（或 `agent:<name>`）。

## MCP

`[mcp].enabled = true` 后，启动时连接配置的 stdio 服务器并发现工具。模型只看到
一个 `MCP` 工具，参数为 `{server, tool, args}`。服务器失败只告警；stdio 单行与
单次结果都有上限（8 MiB / 256 KiB），超出报错或截断。
