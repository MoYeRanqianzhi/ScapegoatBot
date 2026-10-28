---
name: legacy-tools-reference
description: ScapegoatBot 重构前内置工具的分组、数量、命名与实现位置，重构工具层时用于盘点既有能力
metadata:
  type: reference
  scope: ScapegoatBot 重构前的工具层（截至 2026-10-28）；重构后工具集合可能变化
  status: active
  last_verified: 2026-10-28
---

重构前 ScapegoatBot 提供 **43 个内置工具 + 动态 MCP 工具**。定义来源：`src/agent.rs::build_tool_specs()`；蜂群工具：`src/swarm/tools.rs::build_swarm_tool_specs()`；路由：`src/swarm/dispatch.rs::dispatch_tool()`。

## 工具分组

| 分类 | 数量 | 工具 | 实现文件 |
|------|------|------|----------|
| 文件 | 5 | Ls, Read, Write, Edit, Grep | `src/tools/workspace.rs` |
| 消息 | 6 | SendGroupMessage, SendPrivateMessage, DeleteMessage, SetMsgEmojiLike, GroupPoke, FriendPoke | `src/tools/message.rs` |
| 查询 | 4 | GetFriendList, GetGroupList, GetGroupMemberList, GetGroupMemberInfo | `src/tools/query.rs` |
| 社交 | 2 | HandleFriendRequest, HandleGroupRequest | `src/tools/social.rs` |
| 搜索 | 2 | GetMessage, SearchMessages | `src/tools/search.rs` |
| 多模态 | 1 | ReadImage（经 `images.db` 取 URL，下载后 base64 交多模态 LLM） | — |
| 技能 | 1 | Skill（每个 Skill 目录独立 cap-std 沙箱） | `src/tools/skill.rs` |
| 群管理-管理员 | 14 | SetGroupBan, SetGroupWholeBan, SetGroupKick, SetGroupName, SetGroupPortrait, SetEssenceMsg, DeleteEssenceMsg, GetEssenceMsgList, GetGroupAtAllRemain, GetGroupShutList, SendGroupNotice, DelGroupNotice, SetGroupAdmin, SetGroupSpecialTitle | `src/tools/message.rs` |
| 群管理-非管理员 | 4 | SetGroupLeave, GetGroupNotice, GetGroupHonorInfo, SetGroupSign | `src/tools/message.rs` |
| 蜂群 | 4 | SpawnAgent, MessageAgent, CheckAgent, WaitAgent | `src/swarm/tools.rs` |
| 控制 | 1 | Finish（不调用则本轮不结束） | `src/agent.rs` |
| 网络 | 1 | Fetch（HTML→Markdown，SSRF 防护） | `src/tools/fetch.rs` |
| MCP | 动态 | `mcp.<server>.<tool>`，启动时 `tools/list` 发现，inputSchema 直接透传 | `src/mcp/` |

## 关键约束

- 消息段格式（Send* 共用，OneBot 11）：`text`、`at`（`qq="all"` 为全体）、`face`、`reply`（必须在数组首位）、`image`（可选 `summary`/`sub_type`）、`record`、`video`、`json`。
- 工具级配额：Grep 最多 50 条结果、单文件 500KB；Read 200KB；Write 1MB。
- 管理员群管理工具仅在 Bot 为管理员/群主的群可用，状态由 `group_role.rs` 同步；`SetGroupAdmin` / `SetGroupSpecialTitle` 仅群主可用。
- 白名单：Send*、GroupPoke、FriendPoke 受发送白名单约束；GetGroupMemberList/GetGroupMemberInfo 受接收白名单约束。
- Swarm 工具仅在 `swarm.enabled = true` 时注册；`WaitAgent` 期间当前代理阻塞且同一子代理只能 wait 一次。
- MCP 工具超时按服务器配置（示例 `tool_timeout_secs = 30`），服务器为独立子进程。

**Evidence:** 归档自 `docs/development/tools-reference.md`；删除前内容可在提交 `f49019d` 中检索：`git show f49019d:docs/development/tools-reference.md`。参数细节以该提交中的文档或旧代码为准。
**Recheck when:** 重构后工具注册表（`build_tool_specs` 的替代者）落地后，按新实现重新盘点；旧参数默认值可能已变化。
