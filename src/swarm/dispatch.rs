// src/swarm/dispatch.rs
// 工具分发器（ToolDispatcher）
// 从 AgentRunner 提取的工具分发逻辑，使主 Agent 和子 Agent 可以共享同一套工具调用能力
//
// 职责：
// - 接收 LLM 的工具调用请求（工具名 + JSON 参数）
// - 路由到对应的工具实现（文件工具、消息工具、查询工具、社交工具、MCP 等）
// - 返回结构化的 ToolResult（纯文本或多模态图片）

// ============================================================================
// 导入
// ============================================================================

use crate::config::AppConfig;
use crate::providers::traits::ImageData;
use crate::tools::ToolContext;
use crate::tools::workspace::{WorkspaceToolSession, WorkspaceTools};
use napcat_link::MessageSegment;
use std::sync::Arc;

// ============================================================================
// 工具执行结果
// ============================================================================

/// 工具执行的结构化结果
/// 支持纯文本和多模态（文本 + 图片）两种返回形式
/// ReadImage 等多模态工具返回带图片的结果，其他工具返回纯文本
pub(crate) struct ToolResult {
    /// 文本输出（所有工具都会产生文本输出）
    pub text: String,
    /// 附带的图片列表（为空表示纯文本结果）
    pub images: Vec<ImageData>,
}

impl ToolResult {
    /// 创建纯文本结果（大多数工具使用此方法）
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            text: s.into(),
            images: Vec::new(),
        }
    }

    /// 创建带图片的多模态结果
    pub fn with_images(s: impl Into<String>, images: Vec<ImageData>) -> Self {
        Self {
            text: s.into(),
            images,
        }
    }
}

// ============================================================================
// 工具分发器
// ============================================================================

/// 工具分发器
/// 持有工具执行所需的全部上下文，将 LLM 工具调用路由到具体实现
/// 提取自 AgentRunner，使工具分发逻辑可在多个 Agent 之间复用
pub struct ToolDispatcher {
    /// 工具执行上下文（API + 白名单 + Skill + 图片注册表 + 消息索引）
    pub tool_ctx: Arc<ToolContext>,
    /// 工作区文件工具（cap-std 沙箱），管理文件读写操作
    pub workspace_tools: WorkspaceTools,
    /// 工作区工具会话状态（Read/Edit 前置条件追踪）
    pub workspace_session: WorkspaceToolSession,
    /// 全局应用配置（用于 Fetch 等需要配置参数的工具）
    pub config: Arc<AppConfig>,
    /// MCP 连接管理器（管理外部 MCP 服务器进程和工具）
    /// 使用 Arc<tokio::sync::Mutex<>> 实现跨 Agent 共享
    /// None 表示未配置 MCP 或连接失败
    pub mcp_manager: Option<Arc<tokio::sync::Mutex<crate::mcp::McpConnectionManager>>>,
}

impl ToolDispatcher {
    /// 分发工具调用到具体的实现函数
    /// 解析 JSON 参数，路由到对应的工具处理函数
    /// 返回 ToolResult 支持纯文本和多模态（文本 + 图片）两种结果
    pub async fn dispatch_tool(&mut self, name: &str, arguments: &str) -> ToolResult {
        // 解析参数 JSON
        let args: serde_json::Value = match serde_json::from_str(arguments) {
            Ok(v) => v,
            Err(e) => {
                // 输出原始参数字符串，方便诊断 SSE 累积问题
                tracing::error!(
                    "工具 {} 参数解析失败: {}\n  原始参数: {:?}",
                    name, e, arguments
                );
                return ToolResult::text(format!("参数解析失败: {e}"));
            }
        };

        match name {
            // ReadImage 特殊处理：返回多模态结果（文本 + 图片）
            "ReadImage" => self.dispatch_read_image(&args).await,
            // 其他所有工具：委托给纯文本分发函数
            _ => ToolResult::text(self.dispatch_tool_text(name, &args).await),
        }
    }

    /// 分发纯文本工具调用（内部实现）
    /// 所有只返回文本结果的工具在此路由
    async fn dispatch_tool_text(&mut self, name: &str, args: &serde_json::Value) -> String {
        match name {
            // ----- 文件工具 -----
            "Ls" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("/");
                match self.workspace_tools.list_dir(path) {
                    Ok(listing) => {
                        let line_count = listing.lines().count();
                        tracing::info!("Ls({})\n  └ {} 个条目", path, line_count);
                        listing
                    }
                    Err(e) => {
                        tracing::warn!("Ls({}) 失败: {}", path, e);
                        format!("Ls 失败: {e}")
                    }
                }
            }

            "Read" => {
                let path = match args.get("path").and_then(|v| v.as_str()) {
                    Some(p) => p,
                    None => return "缺少参数: path".to_string(),
                };
                match self
                    .workspace_tools
                    .read_file(&mut self.workspace_session, path)
                {
                    Ok(content) => {
                        // codex 风格: Read(path) → 行数/字节
                        tracing::info!(
                            "Read({})\n  └ {} 行, {} 字节",
                            path, content.lines().count(), content.len()
                        );
                        content
                    }
                    Err(e) => {
                        tracing::warn!("Read({})\n  └ 失败: {}", path, e);
                        format!("Read 失败: {e}")
                    }
                }
            }

            "Write" => {
                let path = match args.get("path").and_then(|v| v.as_str()) {
                    Some(p) => p,
                    None => return "缺少参数: path".to_string(),
                };
                let content = match args.get("content").and_then(|v| v.as_str()) {
                    Some(c) => c,
                    None => return "缺少参数: content".to_string(),
                };
                // codex 风格: Write(path) 换行跟内容预览
                let preview_lines = format_content_preview(content, 8);
                tracing::info!("Write({})\n{}", path, preview_lines);
                match self.workspace_tools.write_file(path, content) {
                    Ok(msg) => msg,
                    Err(e) => {
                        tracing::warn!("Write({}) 失败: {}", path, e);
                        format!("Write 失败: {e}")
                    }
                }
            }

            "Edit" => {
                let path = match args.get("path").and_then(|v| v.as_str()) {
                    Some(p) => p,
                    None => return "缺少参数: path".to_string(),
                };
                let old_text = match args.get("old_text").and_then(|v| v.as_str()) {
                    Some(t) => t,
                    None => return "缺少参数: old_text".to_string(),
                };
                let new_text = match args.get("new_text").and_then(|v| v.as_str()) {
                    Some(t) => t,
                    None => return "缺少参数: new_text".to_string(),
                };
                let replace_all = args
                    .get("replace_all")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // codex 风格: Edit(path) 换行跟 -/+ 差异
                let diff = format_edit_diff(old_text, new_text, 6);
                tracing::info!(
                    "Edit({}{})\n{}",
                    path,
                    if replace_all { ", all" } else { "" },
                    diff
                );
                match self.workspace_tools.edit_file(
                    &mut self.workspace_session,
                    path,
                    old_text,
                    new_text,
                    replace_all,
                ) {
                    Ok(msg) => msg,
                    Err(e) => {
                        tracing::warn!("Edit({}) 失败: {}", path, e);
                        format!("Edit 失败: {e}")
                    }
                }
            }

            // ----- 消息工具 -----
            "SendGroupMessage" => {
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: group_id".to_string(),
                };
                let segments = match parse_message_segments(args, "message") {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                let preview = format_segments_preview(&segments);
                tracing::info!("SendGroupMessage(群 {})\n  └ {}", group_id, preview);
                let result =
                    crate::tools::message::send_group_message(&self.tool_ctx, group_id, segments)
                        .await;
                if !result.success {
                    tracing::warn!("  └ 失败: {}", result.output);
                }
                result.output
            }

            "SendPrivateMessage" => {
                let user_id = match args.get("user_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: user_id".to_string(),
                };
                let segments = match parse_message_segments(args, "message") {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                let preview = format_segments_preview(&segments);
                tracing::info!("SendPrivateMessage(用户 {})\n  └ {}", user_id, preview);
                let result = crate::tools::message::send_private_message(
                    &self.tool_ctx,
                    user_id,
                    segments,
                )
                .await;
                if !result.success {
                    tracing::warn!("  └ 失败: {}", result.output);
                }
                result.output
            }

            "DeleteMessage" => {
                let message_id = match args.get("message_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: message_id".to_string(),
                };
                tracing::info!("DeleteMessage({})", message_id);
                let result =
                    crate::tools::message::delete_message(&self.tool_ctx, message_id).await;
                result.output
            }

            "SetMsgEmojiLike" => {
                let message_id = match args.get("message_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: message_id".to_string(),
                };
                let emoji_id = match args.get("emoji_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: emoji_id".to_string(),
                };
                tracing::info!("SetMsgEmojiLike(msg={}, emoji={})", message_id, emoji_id);
                let result = crate::tools::message::set_msg_emoji_like(
                    &self.tool_ctx,
                    message_id,
                    emoji_id,
                )
                .await;
                result.output
            }

            "GroupPoke" => {
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: group_id".to_string(),
                };
                let user_id = match args.get("user_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: user_id".to_string(),
                };
                tracing::info!("GroupPoke(群 {}, 用户 {})", group_id, user_id);
                let result =
                    crate::tools::message::group_poke(&self.tool_ctx, group_id, user_id).await;
                result.output
            }

            "FriendPoke" => {
                let user_id = match args.get("user_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: user_id".to_string(),
                };
                tracing::info!("FriendPoke(用户 {})", user_id);
                let result = crate::tools::message::friend_poke(&self.tool_ctx, user_id).await;
                result.output
            }

            // ----- 查询工具 -----
            "GetFriendList" => {
                tracing::info!("GetFriendList()");
                let result = crate::tools::query::get_friend_list(&self.tool_ctx).await;
                result.output
            }

            "GetGroupList" => {
                tracing::info!("GetGroupList()");
                let result = crate::tools::query::get_group_list(&self.tool_ctx).await;
                result.output
            }

            "GetGroupMemberList" => {
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: group_id".to_string(),
                };
                tracing::info!("GetGroupMemberList(群 {})", group_id);
                let result =
                    crate::tools::query::get_group_member_list(&self.tool_ctx, group_id).await;
                result.output
            }

            "GetGroupMemberInfo" => {
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: group_id".to_string(),
                };
                let user_id = match args.get("user_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: user_id".to_string(),
                };
                tracing::info!("GetGroupMemberInfo(群 {}, 用户 {})", group_id, user_id);
                let result = crate::tools::query::get_group_member_info(
                    &self.tool_ctx,
                    group_id,
                    user_id,
                )
                .await;
                result.output
            }

            // ----- 社交工具 -----
            "HandleFriendRequest" => {
                let flag = match args.get("flag").and_then(|v| v.as_str()) {
                    Some(f) => f,
                    None => return "缺少参数: flag".to_string(),
                };
                let approve = match args.get("approve").and_then(|v| v.as_bool()) {
                    Some(a) => a,
                    None => return "缺少参数: approve".to_string(),
                };
                let remark = args.get("remark").and_then(|v| v.as_str());
                let action = if approve { "同意" } else { "拒绝" };
                tracing::info!("HandleFriendRequest({}, {})", flag, action);
                let result = crate::tools::social::handle_friend_request(
                    &self.tool_ctx,
                    flag,
                    approve,
                    remark,
                )
                .await;
                result.output
            }

            "HandleGroupRequest" => {
                let flag = match args.get("flag").and_then(|v| v.as_str()) {
                    Some(f) => f,
                    None => return "缺少参数: flag".to_string(),
                };
                let sub_type = match args.get("sub_type").and_then(|v| v.as_str()) {
                    Some(s) => s,
                    None => return "缺少参数: sub_type".to_string(),
                };
                let approve = match args.get("approve").and_then(|v| v.as_bool()) {
                    Some(a) => a,
                    None => return "缺少参数: approve".to_string(),
                };
                let reason = args.get("reason").and_then(|v| v.as_str());
                let action = if approve { "同意" } else { "拒绝" };
                tracing::info!("HandleGroupRequest({}, {}, {})", flag, sub_type, action);
                let result = crate::tools::social::handle_group_request(
                    &self.tool_ctx,
                    flag,
                    sub_type,
                    approve,
                    reason,
                )
                .await;
                result.output
            }

            // ----- 好友请求查询工具 -----
            "GetFriendRequests" => {
                tracing::info!("GetFriendRequests()");
                let result =
                    crate::tools::query::get_friend_requests(&self.tool_ctx).await;
                result.output
            }

            // ----- 好友管理工具 -----
            "SetFriendRemark" => {
                let user_id = match args.get("user_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: user_id".to_string(),
                };
                let remark = match args.get("remark").and_then(|v| v.as_str()) {
                    Some(r) => r,
                    None => return "缺少参数: remark".to_string(),
                };
                tracing::info!("SetFriendRemark(用户={}, 备注={})", user_id, remark);
                let result =
                    crate::tools::social::set_friend_remark(&self.tool_ctx, user_id, remark)
                        .await;
                result.output
            }

            "DeleteFriend" => {
                let user_id = match args.get("user_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: user_id".to_string(),
                };
                tracing::info!("DeleteFriend(用户={})", user_id);
                let result =
                    crate::tools::social::delete_friend(&self.tool_ctx, user_id).await;
                result.output
            }

            "GetUnidirectionalFriendList" => {
                tracing::info!("GetUnidirectionalFriendList()");
                let result =
                    crate::tools::query::get_unidirectional_friend_list(&self.tool_ctx).await;
                result.output
            }

            // ----- Skill 工具 -----
            "Skill" => {
                let skill_name = match args.get("name").and_then(|v| v.as_str()) {
                    Some(n) => n,
                    None => return "缺少参数: name".to_string(),
                };
                let skill_path = args.get("path").and_then(|v| v.as_str());
                tracing::info!(
                    "Skill({}{})",
                    skill_name,
                    skill_path.map(|p| format!(", {}", p)).unwrap_or_default()
                );
                let result =
                    crate::tools::skill::execute_skill(&self.tool_ctx, skill_name, skill_path);
                result.output
            }

            // ----- 搜索工具 -----
            "GetMessage" => {
                let message_id = match args.get("message_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: message_id".to_string(),
                };
                tracing::info!("GetMessage({})", message_id);
                let result =
                    crate::tools::search::get_message(&self.tool_ctx, message_id);
                if !result.success {
                    tracing::warn!("GetMessage({}) 失败: {}", message_id, result.output);
                }
                result.output
            }

            "SearchMessages" => {
                let user_id = args.get("user_id").and_then(|v| v.as_i64());
                let group_id = args.get("group_id").and_then(|v| v.as_i64());
                let message_type = args
                    .get("message_type")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let after = args.get("after").and_then(|v| v.as_i64());
                let before = args.get("before").and_then(|v| v.as_i64());
                let limit = args.get("limit").and_then(|v| v.as_u64()).map(|l| l as u32);
                let offset = args.get("offset").and_then(|v| v.as_u64()).map(|o| o as u32);
                tracing::info!(
                    "SearchMessages(user={:?}, group={:?}, type={:?}, after={:?}, before={:?})",
                    user_id, group_id, message_type, after, before
                );
                let result = crate::tools::search::search_messages(
                    &self.tool_ctx,
                    user_id,
                    group_id,
                    message_type,
                    after,
                    before,
                    limit,
                    offset,
                );
                result.output
            }

            "Grep" => {
                let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
                    Some(p) => p,
                    None => return "缺少参数: pattern".to_string(),
                };
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or(".");
                let file_ext = args.get("file_ext").and_then(|v| v.as_str());
                let case_insensitive = args
                    .get("case_insensitive")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                tracing::info!(
                    "Grep(\"{}\", path={}, ext={:?}, ci={})",
                    pattern, path, file_ext, case_insensitive
                );
                match self.workspace_tools.grep(pattern, path, file_ext, case_insensitive) {
                    Ok(output) => {
                        // 统计匹配数用于日志
                        let match_count = output.lines().count().saturating_sub(2);
                        tracing::info!("  └ {} 处匹配", match_count);
                        output
                    }
                    Err(e) => {
                        tracing::warn!("Grep 失败: {}", e);
                        format!("Grep 失败: {e}")
                    }
                }
            }

            // ===== 群管理工具 dispatch（仅管理员/群主） =====

            "SetGroupBan" => {
                // 提取必需参数：群号
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: group_id".to_string(),
                };
                // 提取必需参数：要禁言的用户 QQ 号
                let user_id = match args.get("user_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: user_id".to_string(),
                };
                // 可选参数：禁言时长（秒），None 表示使用默认值
                let duration = args.get("duration").and_then(|v| v.as_i64());
                tracing::info!("SetGroupBan(群={}, 用户={}, 时长={:?})", group_id, user_id, duration);
                // 调用 NapCat API 执行禁言操作
                match self.tool_ctx.api.group.set_group_ban(group_id, user_id, duration).await {
                    Ok(_) => {
                        // 根据 duration 值生成可读描述
                        let dur_desc = duration
                            .map(|d| {
                                if d == 0 { "解除禁言".to_string() } else { format!("{}秒", d) }
                            })
                            .unwrap_or("默认时长".to_string());
                        format!("已对群 {} 用户 {} 执行禁言操作（{}）", group_id, user_id, dur_desc)
                    }
                    Err(e) => format!("群 {} 禁言用户 {} 失败: {e}", group_id, user_id),
                }
            }

            "SetGroupWholeBan" => {
                // 提取必需参数：群号
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: group_id".to_string(),
                };
                // 可选参数：是否开启全员禁言，None 表示默认开启
                let enable = args.get("enable").and_then(|v| v.as_bool());
                tracing::info!("SetGroupWholeBan(群={}, enable={:?})", group_id, enable);
                // 调用 NapCat API 设置全员禁言
                match self.tool_ctx.api.group.set_group_whole_ban(group_id, enable).await {
                    Ok(_) => {
                        // 根据 enable 值生成描述
                        let state = if enable.unwrap_or(true) { "开启" } else { "关闭" };
                        format!("已{}群 {} 全员禁言", state, group_id)
                    }
                    Err(e) => format!("群 {} 全员禁言操作失败: {e}", group_id),
                }
            }

            "SetGroupKick" => {
                // 提取必需参数：群号
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: group_id".to_string(),
                };
                // 提取必需参数：要踢出的用户 QQ 号
                let user_id = match args.get("user_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: user_id".to_string(),
                };
                // 可选参数：是否拒绝此人再次加群
                let reject_add_request = args.get("reject_add_request").and_then(|v| v.as_bool());
                tracing::info!("SetGroupKick(群={}, 用户={}, reject={:?})", group_id, user_id, reject_add_request);
                // 调用 NapCat API 踢出群成员
                match self.tool_ctx.api.group.set_group_kick(group_id, user_id, reject_add_request).await {
                    Ok(_) => {
                        // 附加是否拒绝再次加群的说明
                        let reject_desc = if reject_add_request.unwrap_or(false) { "，并拒绝再次加群" } else { "" };
                        format!("已将用户 {} 踢出群 {}{}", user_id, group_id, reject_desc)
                    }
                    Err(e) => format!("踢出群 {} 用户 {} 失败: {e}", group_id, user_id),
                }
            }

            "SetGroupName" => {
                // 提取必需参数：群号
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: group_id".to_string(),
                };
                // 提取必需参数：新群名
                let group_name = match args.get("group_name").and_then(|v| v.as_str()) {
                    Some(v) => v.to_string(),
                    None => return "缺少参数: group_name".to_string(),
                };
                tracing::info!("SetGroupName(群={}, 名称={})", group_id, group_name);
                // 调用 NapCat API 设置群名
                match self.tool_ctx.api.group.set_group_name(group_id, &group_name).await {
                    Ok(_) => format!("已将群 {} 的名称设置为「{}」", group_id, group_name),
                    Err(e) => format!("设置群 {} 名称失败: {e}", group_id),
                }
            }

            "SetGroupPortrait" => {
                // 提取必需参数：群号
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: group_id".to_string(),
                };
                // 提取必需参数：图片文件路径或 URL
                let file = match args.get("file").and_then(|v| v.as_str()) {
                    Some(v) => v.to_string(),
                    None => return "缺少参数: file".to_string(),
                };
                tracing::info!("SetGroupPortrait(群={}, file={})", group_id, file);
                // 调用 NapCat 文件 API 设置群头像
                match self.tool_ctx.api.file.set_group_portrait(group_id, &file).await {
                    Ok(_) => format!("已设置群 {} 的头像", group_id),
                    Err(e) => format!("设置群 {} 头像失败: {e}", group_id),
                }
            }

            "SetEssenceMsg" => {
                // 提取必需参数：要设为精华的消息 ID
                let message_id = match args.get("message_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: message_id".to_string(),
                };
                tracing::info!("SetEssenceMsg(msg={})", message_id);
                // 调用 NapCat API 设置精华消息
                match self.tool_ctx.api.message.set_essence_message(message_id).await {
                    Ok(_) => format!("已将消息 {} 设为精华消息", message_id),
                    Err(e) => format!("设置精华消息 {} 失败: {e}", message_id),
                }
            }

            "DeleteEssenceMsg" => {
                // 提取必需参数：要移出精华的消息 ID
                let message_id = match args.get("message_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: message_id".to_string(),
                };
                tracing::info!("DeleteEssenceMsg(msg={})", message_id);
                // 调用 NapCat API 移出精华消息
                match self.tool_ctx.api.message.delete_essence_message(message_id).await {
                    Ok(_) => format!("已将消息 {} 移出精华消息", message_id),
                    Err(e) => format!("移出精华消息 {} 失败: {e}", message_id),
                }
            }

            "GetEssenceMsgList" => {
                // 提取必需参数：群号
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: group_id".to_string(),
                };
                tracing::info!("GetEssenceMsgList(群={})", group_id);
                // 调用 NapCat API 获取精华消息列表，返回 JSON 数据
                match self.tool_ctx.api.message.get_essence_message_list(group_id).await {
                    Ok(result) => serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|_| format!("{:?}", result)),
                    Err(e) => format!("获取群 {} 精华消息列表失败: {e}", group_id),
                }
            }

            "GetGroupAtAllRemain" => {
                // 提取必需参数：群号
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: group_id".to_string(),
                };
                tracing::info!("GetGroupAtAllRemain(群={})", group_id);
                // 调用 NapCat API 获取 @全体成员 剩余次数，返回 JSON 数据
                match self.tool_ctx.api.message.get_group_at_all_remain(group_id).await {
                    Ok(result) => serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|_| format!("{:?}", result)),
                    Err(e) => format!("获取群 {} @全体成员剩余次数失败: {e}", group_id),
                }
            }

            "GetGroupShutList" => {
                // 提取必需参数：群号
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: group_id".to_string(),
                };
                tracing::info!("GetGroupShutList(群={})", group_id);
                // 调用 NapCat API 获取禁言列表，返回 JSON 数据
                match self.tool_ctx.api.napcat.get_group_shut_list(group_id).await {
                    Ok(result) => serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|_| format!("{:?}", result)),
                    Err(e) => format!("获取群 {} 禁言列表失败: {e}", group_id),
                }
            }

            "SendGroupNotice" => {
                // 提取必需参数：群号
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: group_id".to_string(),
                };
                // 提取必需参数：公告内容
                let content = match args.get("content").and_then(|v| v.as_str()) {
                    Some(v) => v.to_string(),
                    None => return "缺少参数: content".to_string(),
                };
                // 可选参数：图片 URL，先提取为 Option<String> 以延长生命周期
                let image_owned: Option<String> = args
                    .get("image")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                // 将 Option<String> 转为 Option<&str> 传入 API
                let image = image_owned.as_deref();
                // 可选参数：是否置顶（从 JSON integer 转为 Option<i32>）
                let pinned = args
                    .get("pinned")
                    .and_then(|v| v.as_i64())
                    .map(|v| v as i32);
                tracing::info!("SendGroupNotice(群={}, 内容长={}, 图片={:?}, 置顶={:?})",
                    group_id, content.len(), image, pinned);
                // 调用 NapCat API 发送群公告，后 4 个参数不使用传 None
                match self.tool_ctx.api.napcat.send_group_notice(
                    group_id, &content, image, pinned, None, None, None, None,
                ).await {
                    Ok(_) => format!("已向群 {} 发送公告", group_id),
                    Err(e) => format!("向群 {} 发送公告失败: {e}", group_id),
                }
            }

            "DelGroupNotice" => {
                // 提取必需参数：群号
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: group_id".to_string(),
                };
                // 提取必需参数：公告 ID
                let notice_id = match args.get("notice_id").and_then(|v| v.as_str()) {
                    Some(v) => v.to_string(),
                    None => return "缺少参数: notice_id".to_string(),
                };
                tracing::info!("DelGroupNotice(群={}, notice_id={})", group_id, notice_id);
                // 调用 NapCat API 删除群公告
                match self.tool_ctx.api.napcat.del_group_notice(group_id, &notice_id).await {
                    Ok(_) => format!("已删除群 {} 的公告 {}", group_id, notice_id),
                    Err(e) => format!("删除群 {} 公告 {} 失败: {e}", group_id, notice_id),
                }
            }

            "SetGroupAdmin" => {
                // 提取必需参数：群号
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: group_id".to_string(),
                };
                // 提取必需参数：目标用户 QQ 号
                let user_id = match args.get("user_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: user_id".to_string(),
                };
                // 可选参数：是否设置为管理员，None 表示默认设置
                let enable = args.get("enable").and_then(|v| v.as_bool());
                tracing::info!("SetGroupAdmin(群={}, 用户={}, enable={:?})", group_id, user_id, enable);
                // 调用 NapCat API 设置/取消管理员
                match self.tool_ctx.api.group.set_group_admin(group_id, user_id, enable).await {
                    Ok(_) => {
                        // 根据 enable 值生成描述
                        let action = if enable.unwrap_or(true) { "设置为管理员" } else { "取消管理员" };
                        format!("已将群 {} 用户 {} {}", group_id, user_id, action)
                    }
                    Err(e) => format!("群 {} 设置用户 {} 管理员失败: {e}", group_id, user_id),
                }
            }

            "SetGroupSpecialTitle" => {
                // 提取必需参数：群号
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: group_id".to_string(),
                };
                // 提取必需参数：目标用户 QQ 号
                let user_id = match args.get("user_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: user_id".to_string(),
                };
                // 提取必需参数：头衔内容（空字符串表示删除）
                let special_title = match args.get("special_title").and_then(|v| v.as_str()) {
                    Some(v) => v.to_string(),
                    None => return "缺少参数: special_title".to_string(),
                };
                // 可选参数：有效期（秒），-1 表示永久
                let duration = args.get("duration").and_then(|v| v.as_i64());
                tracing::info!("SetGroupSpecialTitle(群={}, 用户={}, 头衔={}, 时长={:?})",
                    group_id, user_id, special_title, duration);
                // 调用 NapCat API 设置专属头衔
                match self.tool_ctx.api.group.set_group_special_title(
                    group_id, user_id, &special_title, duration,
                ).await {
                    Ok(_) => {
                        // 空头衔表示删除操作
                        if special_title.is_empty() {
                            format!("已删除群 {} 用户 {} 的专属头衔", group_id, user_id)
                        } else {
                            format!("已将群 {} 用户 {} 的专属头衔设为「{}」", group_id, user_id, special_title)
                        }
                    }
                    Err(e) => format!("设置群 {} 用户 {} 专属头衔失败: {e}", group_id, user_id),
                }
            }

            // ===== 群组工具 dispatch（无需管理员权限） =====

            "SetGroupLeave" => {
                // 提取必需参数：群号
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: group_id".to_string(),
                };
                // 可选参数：是否解散群（仅群主有效）
                let is_dismiss = args.get("is_dismiss").and_then(|v| v.as_bool());
                tracing::info!("SetGroupLeave(群={}, dismiss={:?})", group_id, is_dismiss);
                // 调用 NapCat API 退出/解散群组
                match self.tool_ctx.api.group.set_group_leave(group_id, is_dismiss).await {
                    Ok(_) => {
                        // 根据 is_dismiss 判断是退出还是解散
                        if is_dismiss.unwrap_or(false) {
                            format!("已解散群 {}", group_id)
                        } else {
                            format!("已退出群 {}", group_id)
                        }
                    }
                    Err(e) => format!("退出群 {} 失败: {e}", group_id),
                }
            }

            "GetGroupNotice" => {
                // 提取必需参数：群号
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: group_id".to_string(),
                };
                tracing::info!("GetGroupNotice(群={})", group_id);
                // 调用 NapCat API 获取群公告列表，返回 JSON 数据
                match self.tool_ctx.api.napcat.get_group_notice(group_id).await {
                    Ok(result) => serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|_| format!("{:?}", result)),
                    Err(e) => format!("获取群 {} 公告列表失败: {e}", group_id),
                }
            }

            "GetGroupHonorInfo" => {
                // 提取必需参数：群号
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: group_id".to_string(),
                };
                // 提取必需参数：荣誉类型
                let honor_type = match args.get("honor_type").and_then(|v| v.as_str()) {
                    Some(v) => v.to_string(),
                    None => return "缺少参数: honor_type".to_string(),
                };
                tracing::info!("GetGroupHonorInfo(群={}, 类型={})", group_id, honor_type);
                // 调用 NapCat API 获取群荣誉信息，返回 JSON 数据
                match self.tool_ctx.api.message.get_group_honor_info(group_id, &honor_type).await {
                    Ok(result) => serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|_| format!("{:?}", result)),
                    Err(e) => format!("获取群 {} 荣誉信息失败: {e}", group_id),
                }
            }

            "SetGroupSign" => {
                // 提取必需参数：群号
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(v) => v,
                    None => return "缺少参数: group_id".to_string(),
                };
                tracing::info!("SetGroupSign(群={})", group_id);
                // 调用 NapCat API 执行群签到
                match self.tool_ctx.api.napcat.set_group_sign(group_id).await {
                    Ok(_) => format!("已在群 {} 完成签到", group_id),
                    Err(e) => format!("群 {} 签到失败: {e}", group_id),
                }
            }

            // ----- 网络工具 -----
            "Fetch" => {
                // 检查 Fetch 工具是否已启用（防止配置变更后绕过）
                if !self.config.fetch.enabled {
                    return "Fetch 工具未启用。\
                            请在 config.toml 的 [fetch] 段设置 enabled = true"
                        .to_string();
                }
                // 提取必需参数：url
                let url = match args.get("url").and_then(|v| v.as_str()) {
                    Some(u) => u,
                    None => return "缺少参数: url".to_string(),
                };
                tracing::info!("Fetch({})", url);
                // 调用 Fetch 工具核心逻辑
                let result =
                    crate::tools::fetch::fetch_url(url, &self.config.fetch).await;
                // 失败时记录警告日志
                if !result.success {
                    tracing::warn!("Fetch 失败: {}", result.output);
                }
                result.output
            }

            // ===== MCP 外部工具路由 =====
            // 以 "mcp." 开头的工具名由外部 MCP 服务器提供
            // 分割工具名为 (服务器名, 工具名) 后委托给 MCP 连接管理器
            other if other.starts_with("mcp.") => {
                if let Some(ref mgr_arc) = self.mcp_manager {
                    // 获取 MCP 管理器的互斥锁
                    let mut mgr = mgr_arc.lock().await;
                    // 解析 MCP 工具名：mcp.<服务器名>.<工具名>
                    if let Some((server, tool)) = crate::mcp::manager::split_mcp_tool_name(other) {
                        tracing::info!("MCP 工具调用: {}.{}", server, tool);
                        // 委托给 MCP 连接管理器执行工具调用
                        match mgr.call_tool(server, tool, args.clone()).await {
                            Ok(result) => result,
                            Err(e) => {
                                tracing::warn!("MCP 工具调用失败: {}", e);
                                format!("MCP 工具调用失败: {e}")
                            }
                        }
                    } else {
                        // 工具名格式不合法（缺少服务器名或工具名部分）
                        format!("无效的 MCP 工具名格式: {other}")
                    }
                } else {
                    // MCP 连接管理器未初始化（未配置或连接失败）
                    format!("MCP 未初始化，无法调用: {other}")
                }
            }

            _ => format!("未知工具: {name}"),
        }
    }

    /// 执行 ReadImage 工具：通过消息 ID 查找并下载图片，返回多模态结果
    /// 图片 URL 从接收消息时注册的安全注册表中查找，LLM 无法注入任意 URL
    ///
    /// # 安全保障
    /// - URL 来源：仅从已接收消息的 MessageSegment::Image 中提取
    /// - LLM 只能提供 message_id 和 index，无法直接指定 URL
    /// - 图片大小限制 20 MB，防止内存溢出
    /// - 下载超时 30 秒，防止阻塞 Agent 循环
    async fn dispatch_read_image(&self, args: &serde_json::Value) -> ToolResult {
        // 提取 message_id 参数
        let message_id = match args.get("message_id").and_then(|v| v.as_i64()) {
            Some(id) => id,
            None => return ToolResult::text("缺少参数: message_id"),
        };

        // 图片索引，默认 0（第一张图片）
        let index = args
            .get("index")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        // 从安全注册表中查找图片 URL
        let url = match self.tool_ctx.get_image_url(message_id, index) {
            Some(u) => u,
            None => {
                // 区分"消息无图片"和"索引越界"两种情况
                let count = self.tool_ctx.get_image_count(message_id);
                if count == 0 {
                    return ToolResult::text(format!(
                        "消息 {} 中没有已注册的图片。只能查看当前会话期间接收到的消息中的图片。",
                        message_id
                    ));
                } else {
                    return ToolResult::text(format!(
                        "索引超出范围：消息 {} 共有 {} 张图片（索引 0~{}），请求的索引为 {}",
                        message_id,
                        count,
                        count - 1,
                        index
                    ));
                }
            }
        };

        tracing::info!("ReadImage(msg={}, index={}, url={})", message_id, index, url);

        // 构建带超时的 HTTP 客户端（每次调用独立创建，ReadImage 调用频率低）
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
        {
            Ok(c) => c,
            Err(e) => return ToolResult::text(format!("创建 HTTP 客户端失败: {e}")),
        };

        // 从已验证的 URL 下载图片
        let response = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("ReadImage(msg={}) 下载失败: {}", message_id, e);
                return ToolResult::text(format!("图片下载失败: {e}"));
            }
        };

        // 检查 HTTP 响应状态
        if !response.status().is_success() {
            return ToolResult::text(format!(
                "图片下载失败: HTTP {}",
                response.status()
            ));
        }

        // 从 Content-Type 头推断 MIME 类型，回退到 URL 扩展名推断
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|ct| ct.to_str().ok())
            .map(|ct| ct.split(';').next().unwrap_or(ct).trim().to_string())
            .unwrap_or_else(|| guess_image_mime(&url));

        // 读取图片字节
        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("ReadImage(msg={}) 读取响应体失败: {}", message_id, e);
                return ToolResult::text(format!("图片数据读取失败: {e}"));
            }
        };

        // 图片大小限制（20 MB），防止下载超大图片撑爆上下文
        const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
        if bytes.len() > MAX_IMAGE_BYTES {
            return ToolResult::text(format!(
                "图片过大（{:.1} MB，上限 20 MB）",
                bytes.len() as f64 / (1024.0 * 1024.0)
            ));
        }

        // Base64 编码
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);

        let size_kb = bytes.len() as f64 / 1024.0;
        tracing::info!(
            "ReadImage(msg={}, index={})\n  └ {} ({:.1} KB)",
            message_id, index, mime, size_kb
        );

        // 构造多模态结果（文本描述 + 图片数据）
        ToolResult::with_images(
            format!(
                "已加载图片: 消息 {} 第 {} 张 ({}, {:.1} KB)",
                message_id, index, mime, size_kb
            ),
            vec![ImageData {
                mime,
                base64: encoded,
            }],
        )
    }
}

// ============================================================================
// 辅助函数（从 agent.rs 提取，供 ToolDispatcher 使用）
// ============================================================================

/// 从 URL 路径扩展名推断图片 MIME 类型
/// 用于 Content-Type 响应头缺失时的回退判断
fn guess_image_mime(url: &str) -> String {
    // 取 URL 路径部分（去掉查询参数和 fragment）
    let path = url.split('?').next().unwrap_or(url);
    let path = path.split('#').next().unwrap_or(path);
    // 取最后一个 '.' 之后的扩展名
    if let Some(ext) = path.rsplit('.').next() {
        match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => return "image/jpeg".to_string(),
            "png" => return "image/png".to_string(),
            "gif" => return "image/gif".to_string(),
            "webp" => return "image/webp".to_string(),
            "bmp" => return "image/bmp".to_string(),
            "svg" => return "image/svg+xml".to_string(),
            _ => {}
        }
    }
    // 默认假设 JPEG（QQ 图片最常见的格式）
    "image/jpeg".to_string()
}

/// 格式化文件内容预览（用于 Write 日志）
/// 每行前加 `  │ ` 前缀，最多显示 max_lines 行，超出部分用省略号
fn format_content_preview(content: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let mut out = String::new();
    for (i, line) in lines.iter().take(max_lines).enumerate() {
        // 截断过长的行
        let display: String = line.chars().take(120).collect();
        let ellipsis = if line.chars().count() > 120 { "..." } else { "" };
        if i + 1 == max_lines && total > max_lines {
            out.push_str(&format!("  │ {}{}\n", display, ellipsis));
            out.push_str(&format!("  … +{} 行", total - max_lines));
        } else {
            out.push_str(&format!("  │ {}{}\n", display, ellipsis));
        }
    }
    // 去掉末尾换行
    out.trim_end_matches('\n').to_string()
}

/// 格式化编辑差异（用于 Edit 日志）
/// 删除行前加 `  - `，新增行前加 `  + `，最多各显示 max_lines 行
fn format_edit_diff(old_text: &str, new_text: &str, max_lines: usize) -> String {
    let mut out = String::new();
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    // 删除部分
    for (i, line) in old_lines.iter().take(max_lines).enumerate() {
        let display: String = line.chars().take(120).collect();
        out.push_str(&format!("  - {}\n", display));
        if i + 1 == max_lines && old_lines.len() > max_lines {
            out.push_str(&format!("  … +{} 行删除\n", old_lines.len() - max_lines));
            break;
        }
    }
    // 新增部分
    for (i, line) in new_lines.iter().take(max_lines).enumerate() {
        let display: String = line.chars().take(120).collect();
        out.push_str(&format!("  + {}\n", display));
        if i + 1 == max_lines && new_lines.len() > max_lines {
            out.push_str(&format!("  … +{} 行新增\n", new_lines.len() - max_lines));
            break;
        }
    }
    out.trim_end_matches('\n').to_string()
}

/// 从工具参数 JSON 中解析消息段数组
/// `field` 为包含消息段数组的字段名（通常为 "message"）
fn parse_message_segments(
    args: &serde_json::Value,
    field: &str,
) -> Result<Vec<MessageSegment>, String> {
    let array = match args.get(field) {
        Some(v) => v,
        None => return Err(format!("缺少参数: {field}")),
    };
    serde_json::from_value::<Vec<MessageSegment>>(array.clone())
        .map_err(|e| format!("解析消息段数组失败: {e}"))
}

/// 将消息段数组格式化为人类可读的预览字符串
/// 用于日志输出，展示发送消息的具体内容
fn format_segments_preview(segments: &[MessageSegment]) -> String {
    if segments.is_empty() {
        return "(空消息)".to_string();
    }
    let mut parts = Vec::new();
    for seg in segments {
        match seg {
            // 文本消息段：直接输出文本内容
            MessageSegment::Text { text } => {
                parts.push(text.clone());
            }
            // @消息段
            MessageSegment::At { qq, .. } => {
                parts.push(format!("[@{}]", qq));
            }
            // 回复消息段
            MessageSegment::Reply { id, .. } => {
                let id_str = id.as_deref().unwrap_or("?");
                parts.push(format!("[回复:{}]", id_str));
            }
            // 表情消息段
            MessageSegment::Face { id, .. } => {
                parts.push(format!("[表情:{}]", id));
            }
            // 其他类型：利用 serde 序列化获取类型名
            other => {
                if let Ok(v) = serde_json::to_value(other) {
                    let type_name = v.get("type").and_then(|t| t.as_str()).unwrap_or("unknown");
                    parts.push(format!("[{}]", type_name));
                } else {
                    parts.push("[?]".to_string());
                }
            }
        }
    }
    parts.join("")
}
