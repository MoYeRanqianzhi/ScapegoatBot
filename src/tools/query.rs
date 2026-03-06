// src/tools/query.rs
// 查询类工具
// 包含获取好友列表、群列表、群成员列表等查询操作
// 查询好友/群列表不受白名单约束
// 查询群成员列表/信息需要在收白名单中

use super::{ToolContext, ToolResult};

// ============================================================================
// 获取好友列表
// ============================================================================

/// 获取 Bot 的好友列表
/// 无白名单限制
pub async fn get_friend_list(ctx: &ToolContext) -> ToolResult {
    match ctx.api.account.get_friend_list().await {
        Ok(result) => {
            // 将结果序列化为 JSON 字符串返回
            let output = serde_json::to_string_pretty(&result)
                .unwrap_or_else(|_| format!("{result:?}"));
            ToolResult::ok(output)
        }
        Err(e) => ToolResult::err(format!("获取好友列表失败: {e}")),
    }
}

// ============================================================================
// 获取群列表
// ============================================================================

/// 获取 Bot 加入的群列表
/// 无白名单限制
pub async fn get_group_list(ctx: &ToolContext) -> ToolResult {
    match ctx.api.account.get_group_list().await {
        Ok(result) => {
            let output = serde_json::to_string_pretty(&result)
                .unwrap_or_else(|_| format!("{result:?}"));
            ToolResult::ok(output)
        }
        Err(e) => ToolResult::err(format!("获取群列表失败: {e}")),
    }
}

// ============================================================================
// 获取群成员列表
// ============================================================================

/// 获取指定群的成员列表
/// 白名单检查：收-群聊白名单
///
/// # 参数
/// - `ctx`: 工具执行上下文
/// - `group_id`: 群号
pub async fn get_group_member_list(ctx: &ToolContext, group_id: i64) -> ToolResult {
    // 白名单检查：只有收白名单中的群才允许查询
    if !ctx.whitelist.can_recv_group(group_id) {
        return ToolResult::err(format!(
            "查询失败：群 {group_id} 不在接收白名单中"
        ));
    }

    match ctx.api.account.get_group_member_list(group_id).await {
        Ok(result) => {
            let output = serde_json::to_string_pretty(&result)
                .unwrap_or_else(|_| format!("{result:?}"));
            ToolResult::ok(output)
        }
        Err(e) => ToolResult::err(format!("获取群成员列表失败: {e}")),
    }
}

// ============================================================================
// 获取群成员信息
// ============================================================================

/// 获取指定群成员的详细信息
/// 白名单检查：收-群聊白名单
///
/// # 参数
/// - `ctx`: 工具执行上下文
/// - `group_id`: 群号
/// - `user_id`: 成员 QQ 号
pub async fn get_group_member_info(
    ctx: &ToolContext,
    group_id: i64,
    user_id: i64,
) -> ToolResult {
    // 白名单检查
    if !ctx.whitelist.can_recv_group(group_id) {
        return ToolResult::err(format!(
            "查询失败：群 {group_id} 不在接收白名单中"
        ));
    }

    match ctx
        .api
        .account
        .get_group_member_info(group_id, user_id, None)
        .await
    {
        Ok(result) => {
            let output = serde_json::to_string_pretty(&result)
                .unwrap_or_else(|_| format!("{result:?}"));
            ToolResult::ok(output)
        }
        Err(e) => ToolResult::err(format!("获取群成员信息失败: {e}")),
    }
}
