// src/tools/social.rs
// 社交操作工具
// 包含处理好友请求、群请求、好友管理（备注、删除）等工具
// 这些操作不受会话白名单约束（是对请求的响应，而非主动发消息）

use super::{ToolContext, ToolResult};

// ============================================================================
// 处理好友添加请求
// ============================================================================

/// 处理好友添加请求（同意或拒绝）
///
/// # 参数
/// - `ctx`: 工具执行上下文
/// - `flag`: 请求标识（从 FriendRequestEvent 中获取）
/// - `approve`: 是否同意
/// - `remark`: 同意后的备注名称（可选）
pub async fn handle_friend_request(
    ctx: &ToolContext,
    flag: &str,
    approve: bool,
    remark: Option<&str>,
) -> ToolResult {
    match ctx
        .api
        .request
        .handle_friend_request(flag, approve, remark)
        .await
    {
        Ok(_) => {
            let action = if approve { "同意" } else { "拒绝" };
            ToolResult::ok(format!("已{action}好友请求 (flag: {flag})"))
        }
        Err(e) => ToolResult::err(format!("处理好友请求失败: {e}")),
    }
}

// ============================================================================
// 处理群添加/邀请请求
// ============================================================================

/// 处理群添加或群邀请请求（同意或拒绝）
///
/// # 参数
/// - `ctx`: 工具执行上下文
/// - `flag`: 请求标识（从 GroupRequestEvent 中获取）
/// - `sub_type`: 请求子类型（"add" 加群 / "invite" 邀请）
/// - `approve`: 是否同意
/// - `reason`: 拒绝理由（可选，仅拒绝时有效）
pub async fn handle_group_request(
    ctx: &ToolContext,
    flag: &str,
    sub_type: &str,
    approve: bool,
    reason: Option<&str>,
) -> ToolResult {
    match ctx
        .api
        .request
        .handle_group_request(flag, sub_type, approve, reason)
        .await
    {
        Ok(_) => {
            let action = if approve { "同意" } else { "拒绝" };
            let req_type = if sub_type == "invite" {
                "群邀请"
            } else {
                "加群请求"
            };
            ToolResult::ok(format!("已{action}{req_type} (flag: {flag})"))
        }
        Err(e) => ToolResult::err(format!("处理群请求失败: {e}")),
    }
}

// ============================================================================
// 设置好友备注
// ============================================================================

/// 设置好友备注名称
/// 调用 NapCat 扩展 API `set_friend_remark`
///
/// # 参数
/// - `ctx`: 工具执行上下文
/// - `user_id`: 好友 QQ 号
/// - `remark`: 备注名称
pub async fn set_friend_remark(ctx: &ToolContext, user_id: i64, remark: &str) -> ToolResult {
    match ctx.api.napcat.set_friend_remark(user_id, remark).await {
        Ok(_) => ToolResult::ok(format!(
            "已设置好友 {} 的备注为 \"{}\"",
            user_id, remark
        )),
        Err(e) => ToolResult::err(format!("设置好友备注失败: {e}")),
    }
}

// ============================================================================
// 删除好友
// ============================================================================

/// 删除好友（不可撤销操作）
/// 调用 NapCat 扩展 API `delete_friend`
///
/// # 参数
/// - `ctx`: 工具执行上下文
/// - `user_id`: 要删除的好友 QQ 号
pub async fn delete_friend(ctx: &ToolContext, user_id: i64) -> ToolResult {
    match ctx.api.napcat.delete_friend(user_id).await {
        Ok(_) => ToolResult::ok(format!("已删除好友 {}", user_id)),
        Err(e) => ToolResult::err(format!("删除好友失败: {e}")),
    }
}
