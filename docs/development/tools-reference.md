# 工具参考手册

本文档列出 ScapegoatBot Agent 可使用的全部工具，包括参数定义、示例调用和安全备注。

工具参数定义来源：`src/agent.rs` 中的 `build_tool_specs()` 函数，蜂群工具定义来源：`src/swarm/tools.rs` 中的 `build_swarm_tool_specs()` 函数。

---

## 1. 文件工具

文件工具通过 cap-std 沙箱限制在 workspace 目录内。实现位于 `src/tools/workspace.rs`。

### Ls

列出工作区内指定目录的文件和子目录。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `path` | string | 是 | 要列出的目录路径（相对于工作区根目录，`/` 表示根目录） |

```json
{ "path": "messages/raw" }
```

### Read

读取工作区内的 UTF-8 文本文件。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `path` | string | 是 | 要读取的文件路径（相对于工作区根目录） |

```json
{ "path": "MEMORY.md" }
```

**安全备注**：单文件最大 200 KB（MAX_READ_FILE_BYTES），超过则拒绝。

### Write

创建新的 UTF-8 文本文件。如果文件已存在则失败，修改已有文件请用 Edit。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `path` | string | 是 | 要创建的文件路径（相对于工作区根目录） |
| `content` | string | 是 | 文件内容 |

```json
{ "path": "notes/todo.md", "content": "# TODO\n\n- 完成作业" }
```

**安全备注**：
- 单文件最大 1 MB（MAX_WRITE_CONTENT_BYTES）
- 磁盘剩余 < 100 MB 时拒绝写入
- 工作区总量上限 2 GB / 50000 文件
- `messages/raw/`、`data/`、`sessions/` 目录写保护

### Edit

编辑已存在的文件。必须先 Read 同一文件后才能 Edit。使用精确字符串替换。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `path` | string | 是 | 要编辑的文件路径（相对于工作区根目录） |
| `old_text` | string | 是 | 要替换的原始文本 |
| `new_text` | string | 是 | 替换后的新文本 |
| `replace_all` | boolean | 否 | 是否替换所有匹配（默认 false，仅替换第一处且要求唯一匹配） |

```json
{
  "path": "MEMORY.md",
  "old_text": "旧内容",
  "new_text": "新内容",
  "replace_all": false
}
```

**安全备注**：必须先 Read 后才能 Edit（Claude Code 模式），Read 一次后可多次 Edit。

### Grep

在工作区文件中搜索文本。返回匹配的文件路径、行号和内容。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `pattern` | string | 是 | 要搜索的文本 |
| `path` | string | 否 | 搜索起始目录（相对于工作区根，默认搜索整个工作区） |
| `file_ext` | string | 否 | 文件扩展名过滤（如 `md`、`jsonl`），不含前导点号 |
| `case_insensitive` | boolean | 否 | 是否忽略大小写（默认 false） |

```json
{ "pattern": "重要事项", "path": "messages", "file_ext": "md", "case_insensitive": true }
```

**安全备注**：最多返回 50 条结果（MAX_GREP_RESULTS），单文件最大 500 KB（MAX_GREP_FILE_BYTES）。

---

## 2. 消息工具

消息工具用于发送/撤回消息和互动。实现位于 `src/tools/message.rs`。

### SendGroupMessage

向指定群发送消息。群号必须在发送白名单中。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 目标群号 |
| `message` | array | 是 | 消息段数组（OneBot 11 格式，详见下方"消息段类型"） |

```json
{
  "group_id": 123456789,
  "message": [
    { "type": "text", "data": { "text": "你好" } }
  ]
}
```

### SendPrivateMessage

向指定用户发送私聊消息。用户 QQ 号必须在发送白名单中。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `user_id` | integer | 是 | 目标用户 QQ 号 |
| `message` | array | 是 | 消息段数组（OneBot 11 格式，详见下方"消息段类型"） |

```json
{
  "user_id": 10001,
  "message": [
    { "type": "reply", "data": { "id": "12345" } },
    { "type": "text", "data": { "text": "收到" } }
  ]
}
```

### 消息段类型

SendGroupMessage 和 SendPrivateMessage 共用的消息段格式：

| 类型 | data 字段 | 说明 |
|------|-----------|------|
| `text` | `{"text": "文本内容"}` | 纯文本 |
| `at` | `{"qq": "用户QQ号"}` | @某人，`"all"` 表示 @全体成员 |
| `face` | `{"id": "表情ID"}` | QQ 系统表情 |
| `reply` | `{"id": "消息ID"}` | 回复某条消息，必须放在数组第一个位置 |
| `image` | `{"file": "URL或路径"}` | 图片，可选 `summary`（描述）、`sub_type`（0=普通, 1=表情包） |
| `record` | `{"file": "URL或路径"}` | 语音 |
| `video` | `{"file": "URL或路径"}` | 视频 |
| `json` | `{"data": "JSON卡片字符串"}` | JSON 富文本卡片 |

### DeleteMessage

撤回一条消息。只能撤回自己发送的消息。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `message_id` | integer | 是 | 要撤回的消息 ID |

```json
{ "message_id": 123456 }
```

### SetMsgEmojiLike

对一条消息添加表情回应。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `message_id` | integer | 是 | 目标消息 ID |
| `emoji_id` | integer | 是 | 表情 ID |

```json
{ "message_id": 123456, "emoji_id": 76 }
```

### GroupPoke

在群聊中戳一戳某人。群号必须在发送白名单中。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |
| `user_id` | integer | 是 | 要戳的用户 QQ 号 |

```json
{ "group_id": 123456789, "user_id": 10001 }
```

### FriendPoke

戳一戳好友。用户 QQ 号必须在发送白名单中。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `user_id` | integer | 是 | 要戳的用户 QQ 号 |

```json
{ "user_id": 10001 }
```

---

## 3. 查询工具

查询工具用于获取好友、群组和成员信息。实现位于 `src/tools/query.rs`。

### GetFriendList

获取好友列表。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| _(无)_ | — | — | 无参数 |

```json
{}
```

### GetGroupList

获取已加入的群列表。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| _(无)_ | — | — | 无参数 |

```json
{}
```

### GetGroupMemberList

获取指定群的成员列表。群号必须在接收白名单中。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |

```json
{ "group_id": 123456789 }
```

### GetGroupMemberInfo

获取指定群成员的详细信息。群号必须在接收白名单中。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |
| `user_id` | integer | 是 | 成员 QQ 号 |

```json
{ "group_id": 123456789, "user_id": 10001 }
```

---

## 4. 社交工具

社交工具用于处理好友/群组请求。实现位于 `src/tools/social.rs`。

### HandleFriendRequest

处理好友添加请求（同意或拒绝）。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `flag` | string | 是 | 请求标识（从好友请求事件中获取） |
| `approve` | boolean | 是 | 是否同意 |
| `remark` | string | 否 | 同意后的备注名称 |

```json
{ "flag": "abc123", "approve": true, "remark": "同学" }
```

### HandleGroupRequest

处理群添加或群邀请请求（同意或拒绝）。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `flag` | string | 是 | 请求标识（从群请求事件中获取） |
| `sub_type` | string | 是 | 请求子类型：`add`（加群）或 `invite`（邀请） |
| `approve` | boolean | 是 | 是否同意 |
| `reason` | string | 否 | 拒绝理由（仅拒绝时有效） |

```json
{ "flag": "xyz789", "sub_type": "add", "approve": false, "reason": "不认识" }
```

---

## 5. 搜索工具

搜索工具用于检索消息记录。实现位于 `src/tools/search.rs`。

### GetMessage

通过消息 ID 获取原始消息内容（JSON 格式）。利用数据库索引快速定位。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `message_id` | integer | 是 | 要查找的消息 ID（从消息记录的 MsgID 字段获取） |

```json
{ "message_id": 123456 }
```

### SearchMessages

搜索消息记录。支持按发送者、群号、类型、时间范围等条件过滤。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `user_id` | integer | 否 | 按发送者 QQ 号过滤 |
| `group_id` | integer | 否 | 按群号过滤 |
| `message_type` | string | 否 | 按消息类型过滤：`group`（群消息）或 `private`（私聊消息） |
| `after` | integer | 否 | 起始时间戳（Unix 秒，包含） |
| `before` | integer | 否 | 截止时间戳（Unix 秒，包含） |
| `limit` | integer | 否 | 最大返回条数（默认 20，上限 100） |
| `offset` | integer | 否 | 分页偏移量（默认 0） |

```json
{
  "user_id": 10001,
  "group_id": 123456789,
  "message_type": "group",
  "after": 1710000000,
  "limit": 50
}
```

---

## 6. 多模态工具

### ReadImage

查看消息中的图片。通过消息 ID 定位图片并返回视觉内容。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `message_id` | integer | 是 | 包含图片的消息 ID（从消息记录的 MsgID 字段获取） |
| `index` | integer | 否 | 图片在消息中的位置索引（从 0 开始，默认 0 即第一张图片） |

```json
{ "message_id": 123456, "index": 0 }
```

**实现说明**：通过 `images.db` SQLite 数据库查找图片 URL，下载图片并编码为 base64 供多模态 LLM 识别。

---

## 7. 技能工具

实现位于 `src/tools/skill.rs`。

### Skill

加载已安装的技能文件。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `name` | string | 是 | 技能名称 |
| `path` | string | 否 | 技能内相对路径（默认为 `SKILL.md`） |

```json
{ "name": "weather" }
```

```json
{ "name": "coding", "path": "examples/rust.md" }
```

**安全备注**：每个 Skill 目录独立 cap-std 沙箱，只能读取该 Skill 自身目录内的文件。

---

## 8. 群管理工具

群管理工具分为两类：需要管理员权限（14 个）和无需管理员权限（4 个）。

Bot 的管理员状态由 `group_role.rs` 模块自动同步（启动全量同步 + 事件驱动 + 12 小时定时轮询）。管理员工具仅在 Bot 具有管理员/群主身份的群中可用。

### 管理员工具（14 个）

#### SetGroupBan

群组单人禁言。设置 `duration=0` 可解除禁言。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |
| `user_id` | integer | 是 | 要禁言的用户 QQ 号 |
| `duration` | integer | 否 | 禁言时长（秒），默认 1800（30 分钟），0 = 解除禁言 |

```json
{ "group_id": 123456789, "user_id": 10001, "duration": 3600 }
```

#### SetGroupWholeBan

群组全员禁言。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |
| `enable` | boolean | 否 | true = 开启全员禁言，false = 关闭，默认 true |

```json
{ "group_id": 123456789, "enable": true }
```

#### SetGroupKick

踢出群成员。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |
| `user_id` | integer | 是 | 要踢出的用户 QQ 号 |
| `reject_add_request` | boolean | 否 | 是否拒绝此人再次加群，默认 false |

```json
{ "group_id": 123456789, "user_id": 10001, "reject_add_request": true }
```

#### SetGroupName

设置群名。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |
| `group_name` | string | 是 | 新群名 |

```json
{ "group_id": 123456789, "group_name": "新群名称" }
```

#### SetGroupPortrait

设置群头像。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |
| `file` | string | 是 | 图片文件路径或 URL |

```json
{ "group_id": 123456789, "file": "https://example.com/avatar.jpg" }
```

#### SetEssenceMsg

设置精华消息。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `message_id` | integer | 是 | 要设为精华的消息 ID |

```json
{ "message_id": 123456 }
```

#### DeleteEssenceMsg

移出精华消息。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `message_id` | integer | 是 | 要移出精华的消息 ID |

```json
{ "message_id": 123456 }
```

#### GetEssenceMsgList

获取精华消息列表。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |

```json
{ "group_id": 123456789 }
```

#### GetGroupAtAllRemain

获取群 @全体成员 剩余次数。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |

```json
{ "group_id": 123456789 }
```

#### GetGroupShutList

获取群禁言列表。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |

```json
{ "group_id": 123456789 }
```

#### SendGroupNotice

发送群公告。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |
| `content` | string | 是 | 公告内容 |
| `image` | string | 否 | 图片 URL |
| `pinned` | integer | 否 | 是否置顶，1 = 置顶，0 = 不置顶 |

```json
{ "group_id": 123456789, "content": "群规更新通知", "pinned": 1 }
```

#### DelGroupNotice

删除群公告。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |
| `notice_id` | string | 是 | 要删除的公告 ID |

```json
{ "group_id": 123456789, "notice_id": "abc123" }
```

#### SetGroupAdmin

设置或取消群管理员（仅群主可用）。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |
| `user_id` | integer | 是 | 目标用户 QQ 号 |
| `enable` | boolean | 否 | true = 设置为管理员，false = 取消管理员，默认 true |

```json
{ "group_id": 123456789, "user_id": 10001, "enable": true }
```

#### SetGroupSpecialTitle

设置群组专属头衔（仅群主可用）。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |
| `user_id` | integer | 是 | 目标用户 QQ 号 |
| `special_title` | string | 是 | 头衔内容，空字符串表示删除 |
| `duration` | integer | 否 | 有效期（秒），-1 = 永久 |

```json
{ "group_id": 123456789, "user_id": 10001, "special_title": "活跃达人", "duration": -1 }
```

### 非管理员工具（4 个）

#### SetGroupLeave

退出群组。如果是群主且 `is_dismiss=true` 则解散群。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |
| `is_dismiss` | boolean | 否 | 是否解散群（仅群主有效），默认 false |

```json
{ "group_id": 123456789 }
```

#### GetGroupNotice

获取群公告列表。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |

```json
{ "group_id": 123456789 }
```

#### GetGroupHonorInfo

获取群荣誉信息（龙王、群聊之火等）。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |
| `honor_type` | string | 是 | 荣誉类型：`talkative`=龙王, `performer`=群聊之火, `legend`=群聊炽焰, `strong_newbie`=冒尖小春笋, `emotion`=快乐之源, `all`=全部 |

```json
{ "group_id": 123456789, "honor_type": "all" }
```

#### SetGroupSign

群签到/打卡。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `group_id` | integer | 是 | 群号 |

```json
{ "group_id": 123456789 }
```

---

## 9. 网络工具

### Fetch（即将新增）

获取网页内容并转换为 Markdown。需要在配置文件中显式启用。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `url` | string | 是 | 要获取的 URL |

```json
{ "url": "https://example.com/article" }
```

**安全备注**：
- 默认禁用，需要 `[fetch] enabled = true` 显式启用
- SSRF 防护：自动拦截对私有/保留 IP 地址的请求（127.0.0.0/8, 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16 等）
- 域名过滤：支持 whitelist/blacklist 模式控制可访问域名
- 响应限制：最大响应体 512 KB，最大输出 50000 字符

**配置参考**：`[fetch]` 段，详见 [config.example.toml](../../config.example.toml)。

---

## 10. MCP 工具（动态发现）

MCP (Model Context Protocol) 工具通过外部服务器动态提供。Agent 启动时自动发现所有已配置 MCP 服务器的工具。

### 命名约定

MCP 工具名称格式：`mcp.<服务器名>.<工具名>`

例如，配置了 `[mcp.filesystem]` 服务器，其提供 `read_file` 工具，则工具名为 `mcp.filesystem.read_file`。

### 工具发现流程

1. 根据 `[mcp.*]` 配置启动子进程
2. 通过 JSON-RPC 发送 `initialize` 请求 + `initialized` 通知完成握手
3. 调用 `tools/list` 获取服务器提供的所有工具定义
4. 将 MCP 工具的 `inputSchema` 直接作为 ToolSpec 的 parameters 透传给 LLM
5. Agent 调用时路由到对应的 MCP 服务器子进程

### 配置示例

```toml
[mcp.filesystem]
command = "npx"
args = ["-y", "@anthropic/mcp-filesystem", "/home/user/docs"]
env = {}
enabled = true
tool_timeout_secs = 30
```

**安全备注**：MCP 服务器作为独立子进程运行，通过 stdin/stdout 通信，不共享主进程的内存或文件句柄。

---

## 11. 蜂群工具

蜂群工具用于管理子代理的生命周期和通信。仅当 `config.swarm.enabled = true` 时注册。实现位于 `src/swarm/tools.rs`，分发逻辑位于 `src/swarm/dispatch.rs`。

子代理运行在独立的 tokio 任务中，拥有独立的 LLM 对话和工具访问权限。完成后通过 Finish 工具的 `result` 字段返回结果给父代理。子代理也可以嵌套 spawn 子代理（受 `swarm.max_depth` 限制）。

### SpawnAgent

生成一个子代理来并行执行任务。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `name` | string | 是 | 子代理名称（必须唯一，用于后续的 MessageAgent / CheckAgent / WaitAgent 引用） |
| `task` | string | 是 | 分配给子代理的任务描述（尽量详细具体，子代理会作为独立任务执行） |
| `model` | string | 否 | 使用的模型名称（引用 `[[models]]` 配置中的 `name` 字段，不指定则使用主 `[llm]` 配置） |
| `context` | string | 否 | 额外上下文信息（帮助子代理理解任务背景） |

```json
{
  "name": "searcher",
  "task": "在工作区中搜索所有包含 TODO 的文件，汇总为清单",
  "model": "fast"
}
```

```json
{
  "name": "analyzer",
  "task": "分析 src/agent.rs 的代码结构，输出模块依赖关系",
  "context": "项目使用 Rust 2024 edition，异步运行时为 tokio"
}
```

**安全备注**：
- 名称在蜂群中必须唯一，重复名称会被拒绝
- 嵌套深度受 `swarm.max_depth` 限制（默认 32）
- 并发数受 `swarm.max_concurrency` 限制（默认 1024）
- 每个子代理有独立的 `max_steps` 限制（防止无限循环）

### MessageAgent

向指定的运行中子代理发送消息。消息会在子代理下一次 LLM 调用前作为 user 消息注入其对话历史。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `name` | string | 是 | 目标子代理名称 |
| `message` | string | 是 | 要发送的消息内容 |

```json
{
  "name": "searcher",
  "message": "请额外关注 workspace.rs 中的安全相关 TODO"
}
```

**说明**：消息以 `[父代理消息]` 前缀注入子代理对话历史。只能向运行中的子代理发送消息。

### CheckAgent

查询子代理的当前状态。可查询单个子代理或列出所有子代理状态。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `name` | string | 否 | 要查询的子代理名称（不指定且 `list_all=true` 时列出所有子代理） |
| `list_all` | boolean | 否 | 是否列出所有子代理状态（默认 false） |

```json
{ "name": "searcher" }
```

```json
{ "list_all": true }
```

**返回信息**：子代理名称、模型、运行状态（运行中/已完成/已失败）和已运行时间。

### WaitAgent

阻塞等待子代理完成并返回其结果。可等待单个、多个或全部子代理。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `name` | string | 否 | 等待单个子代理的名称 |
| `names` | array[string] | 否 | 等待多个子代理的名称列表 |
| `all` | boolean | 否 | 是否等待所有运行中的子代理（默认 false） |

```json
{ "name": "searcher" }
```

```json
{ "names": ["searcher", "analyzer"] }
```

```json
{ "all": true }
```

**说明**：
- 等待期间当前代理不会处理其他消息
- 同一个子代理只能 wait 一次（JoinHandle 被消费）
- 返回子代理 Finish 工具的 result 字段内容

**配置参考**：`[swarm]` 段和 `[[models]]` 段，详见 [config.example.toml](../../config.example.toml)。

---

## 12. 控制工具

### Finish

结束本轮处理。当 Agent 完成了所有需要做的事情或明确决定沉默时调用。

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `reason` | string | 是 | 结束原因的简要说明 |

```json
{ "reason": "已回复群消息" }
```

```json
{ "reason": "决定沉默：话题与我无关" }
```

**说明**：不调用 Finish 则轮次不会结束。Agent 必须在完成任务后显式调用此工具。

---

## 工具统计

| 分类 | 数量 | 工具列表 |
|------|------|----------|
| 文件工具 | 5 | Ls, Read, Write, Edit, Grep |
| 消息工具 | 6 | SendGroupMessage, SendPrivateMessage, DeleteMessage, SetMsgEmojiLike, GroupPoke, FriendPoke |
| 查询工具 | 4 | GetFriendList, GetGroupList, GetGroupMemberList, GetGroupMemberInfo |
| 社交工具 | 2 | HandleFriendRequest, HandleGroupRequest |
| 搜索工具 | 2 | GetMessage, SearchMessages |
| 多模态工具 | 1 | ReadImage |
| 技能工具 | 1 | Skill |
| 群管理工具-管理员 | 14 | SetGroupBan, SetGroupWholeBan, SetGroupKick, SetGroupName, SetGroupPortrait, SetEssenceMsg, DeleteEssenceMsg, GetEssenceMsgList, GetGroupAtAllRemain, GetGroupShutList, SendGroupNotice, DelGroupNotice, SetGroupAdmin, SetGroupSpecialTitle |
| 群管理工具-非管理员 | 4 | SetGroupLeave, GetGroupNotice, GetGroupHonorInfo, SetGroupSign |
| 蜂群工具 | 4 | SpawnAgent, MessageAgent, CheckAgent, WaitAgent |
| 控制工具 | 1 | Finish |
| 网络工具 | 1 (即将) | Fetch |
| MCP 工具 | 动态 | mcp.\<server\>.\<tool\> |
| **合计** | **43 + 动态** | |
