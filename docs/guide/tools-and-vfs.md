# 工具与虚拟文件系统

agent 不直接接触宿主文件系统。它看到一个 POSIX 风格的虚拟文件系统（VFS），
所有路径以 `/` 开头，深度 ≤16、长度 ≤255，`.` / `..` / 反斜杠 / 盘符一律拒绝。

## 种子布局

```text
/AGENTS.md        主人给 agent 的规则（owner context）
/USER.md          偏好（祈使句）
/MEMORY.md        记忆索引
/memory/topics/   记忆分类（可选）
/memory/notes/    其他笔记（可选）
/agents/          自定义子代理定义（<name>.md）
/skills/          技能（<name>/SKILL.md）
/trash/           回收站：RM 默认移到这里
/tmp/             临时文件
```

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

查询参数只允许出现在这些路径上：

```text
/messages/groups/123/2026-11/?limit=20&offset=0&order=desc&filter=contains:deploy
```

- `filter` 与 monitor 的 `when` 同一套谓词语法，如 `state:unread`、`from:<qq>`、
  `contains:<text>`。
- `limit` 默认 50、上限 500；`offset`；`order = asc | desc`。

### 读状态语义

- `Read` 进来的内容算“读过”，按条标记已读。
- `Grep` **不**标记：只扫描、不读入上下文。
- `LS` / `Stat` 只给元信息，不标记。
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
| `Download` | HTTP(S) 下载到 VFS，带 SSRF/域名/大小限制 |
| `WebSearch` / `WebFetch` | 搜索 / 抓取；由服务端工具或你配置的后端执行 |
| `Monitor` | 增删查事件订阅，手动触发一次 |
| `Agent` | 生成/续聊/查询/等待子代理 |
| `MCP` | 调用已连接的 MCP 服务器工具（可选） |

`API` 分三个命名空间：

- `napcat.*`：QQ 操作。hold 期间全部拒绝。
- `agent.*`：`status`、`resume`、`quarantine`、`compact`、`clear_context`。
- `api.*`：`list`（过滤/分页）、`help`（单个 action 的 schema）。

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

回收站按比例清理；墓碑与用量可在 `/system` 查看。

## 子代理

`Agent` 工具的动作：

```json
{"action": "spawn", "task": "...", "agent": "explore", "monitor": "at-me"}
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

## MCP

`[mcp].enabled = true` 后，启动时连接配置的 stdio 服务器并发现工具。模型只看到
一个 `MCP` 工具，参数为 `{server, tool, args}`。服务器失败只告警；stdio 单行与
单次结果都有上限（8 MiB / 256 KiB），超出报错或截断。
