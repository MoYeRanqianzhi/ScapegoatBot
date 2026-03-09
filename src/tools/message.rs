// src/tools/message.rs
// 消息类工具
// 包含发送消息、撤回消息、表情回应、戳一戳等工具
// 所有发送类操作均受发白名单约束

use super::{ToolContext, ToolResult};
use napcat_link::MessageSegment;

// ============================================================================
// 发送私聊消息
// ============================================================================

/// 向指定用户发送私聊消息
/// 白名单检查：发-私聊白名单
///
/// # 参数
/// - `ctx`: 工具执行上下文
/// - `user_id`: 目标用户 QQ 号
/// - `segments`: 消息段数组
pub async fn send_private_message(
    ctx: &ToolContext,
    user_id: i64,
    segments: Vec<MessageSegment>,
) -> ToolResult {
    // 白名单检查
    if !ctx.whitelist.can_send_private(user_id) {
        return ToolResult::err(format!(
            "发送失败：用户 {user_id} 不在私聊发送白名单中"
        ));
    }

    // 调用 NapCatLink API 发送消息
    match ctx.api.message.send_private_message(user_id, segments).await {
        Ok(result) => {
            // 提取消息 ID 用于回传
            let msg_id = result
                .get("message_id")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            // 记录 agent 发送的消息 ID（用于 master=self 时过滤）
            ctx.record_agent_sent(msg_id);
            ToolResult::ok(format!("私聊消息已发送给 {user_id}，消息ID: {msg_id}"))
        }
        Err(e) => ToolResult::err(format!("发送私聊消息失败: {e}")),
    }
}

// ============================================================================
// 发送群聊消息
// ============================================================================

/// 向指定群发送群聊消息
/// 白名单检查：发-群聊白名单
///
/// # 参数
/// - `ctx`: 工具执行上下文
/// - `group_id`: 目标群号
/// - `segments`: 消息段数组
pub async fn send_group_message(
    ctx: &ToolContext,
    group_id: i64,
    segments: Vec<MessageSegment>,
) -> ToolResult {
    // 白名单检查
    if !ctx.whitelist.can_send_group(group_id) {
        return ToolResult::err(format!(
            "发送失败：群 {group_id} 不在群聊发送白名单中"
        ));
    }

    // 调用 NapCatLink API 发送消息
    match ctx.api.message.send_group_message(group_id, segments).await {
        Ok(result) => {
            let msg_id = result
                .get("message_id")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            // 记录 agent 发送的消息 ID（用于 master=self 时过滤）
            ctx.record_agent_sent(msg_id);
            ToolResult::ok(format!("群消息已发送到群 {group_id}，消息ID: {msg_id}"))
        }
        Err(e) => ToolResult::err(format!("发送群消息失败: {e}")),
    }
}

// ============================================================================
// 撤回消息
// ============================================================================

/// 撤回一条消息
/// 白名单检查：需要在发白名单中（因为撤回是对会话的"写"操作）
/// 注意：只能撤回自己发送的消息
///
/// # 参数
/// - `ctx`: 工具执行上下文
/// - `message_id`: 要撤回的消息 ID
pub async fn delete_message(ctx: &ToolContext, message_id: i64) -> ToolResult {
    match ctx.api.message.delete_message(message_id).await {
        Ok(_) => ToolResult::ok(format!("消息 {message_id} 已撤回")),
        Err(e) => ToolResult::err(format!("撤回消息失败: {e}")),
    }
}

// ============================================================================
// 消息表情回应
// ============================================================================

/// 对一条消息添加表情回应
///
/// # 参数
/// - `ctx`: 工具执行上下文
/// - `message_id`: 目标消息 ID
/// - `emoji_id`: 表情 ID（数值）
pub async fn set_msg_emoji_like(
    ctx: &ToolContext,
    message_id: i64,
    emoji_id: i64,
) -> ToolResult {
    match ctx
        .api
        .message
        .set_msg_emoji_like(message_id, emoji_id, true)
        .await
    {
        Ok(_) => ToolResult::ok(format!(
            "已对消息 {message_id} 添加表情回应 {emoji_id}"
        )),
        Err(e) => ToolResult::err(format!("添加表情回应失败: {e}")),
    }
}

// ============================================================================
// 戳一戳
// ============================================================================

/// 群聊戳一戳
/// 白名单检查：发-群聊白名单
///
/// # 参数
/// - `ctx`: 工具执行上下文
/// - `group_id`: 群号
/// - `user_id`: 要戳的用户 QQ 号
pub async fn group_poke(ctx: &ToolContext, group_id: i64, user_id: i64) -> ToolResult {
    // 白名单检查
    if !ctx.whitelist.can_send_group(group_id) {
        return ToolResult::err(format!(
            "戳一戳失败：群 {group_id} 不在群聊发送白名单中"
        ));
    }

    match ctx.api.message.group_poke(group_id, user_id).await {
        Ok(_) => ToolResult::ok(format!("已在群 {group_id} 中戳了 {user_id}")),
        Err(e) => ToolResult::err(format!("群戳一戳失败: {e}")),
    }
}

/// 好友戳一戳
/// 白名单检查：发-私聊白名单
///
/// # 参数
/// - `ctx`: 工具执行上下文
/// - `user_id`: 要戳的用户 QQ 号
pub async fn friend_poke(ctx: &ToolContext, user_id: i64) -> ToolResult {
    // 白名单检查
    if !ctx.whitelist.can_send_private(user_id) {
        return ToolResult::err(format!(
            "戳一戳失败：用户 {user_id} 不在私聊发送白名单中"
        ));
    }

    match ctx.api.message.friend_poke(user_id).await {
        Ok(_) => ToolResult::ok(format!("已戳了好友 {user_id}")),
        Err(e) => ToolResult::err(format!("好友戳一戳失败: {e}")),
    }
}
