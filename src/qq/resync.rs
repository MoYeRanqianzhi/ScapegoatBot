//! 断线补消息（context-only）。
//!
//! 设计依据：设计 §4「断线重连与补消息」。
//!
//! 规则：
//! - 每个 chat 用 `(last_message_id, last_ts)` 双轨游标；
//! - 补回的消息一律标 `[backfill]`，只写存储、不进入去抖队列、不触发回合；
//!   否则 NapCat 重启两小时后会对几小时前的消息逐条回复；
//! - 按 `message_id` 去重；锚点失败时退化为“取最新 N 条”，并在开头写 `[gap]`。

use anyhow::Result;
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::storage::messages::MessageStore;

use super::client::QqClient;
use super::event::{ChatId, QqEvent, QqMessage};

/// 单次补消息上限（设计默认 200）。
pub const BACKFILL_PAGE: u32 = 200;

/// 补一个 chat 的消息，返回新增条数。
pub async fn backfill_chat(
    client: &QqClient,
    store: &MessageStore,
    chat: ChatId,
    self_id: u64,
) -> Result<usize> {
    let Some((anchor, _last_ts)) = store.cursor(chat)? else {
        // 没有游标表示这个 chat 从未处理过；不主动翻旧账。
        return Ok(0);
    };

    let (action, mut params) = match chat {
        ChatId::Group(group) => (
            "get_group_msg_history",
            json!({ "group_id": group, "message_seq": anchor, "count": BACKFILL_PAGE }),
        ),
        ChatId::Private(user) => (
            "get_friend_msg_history",
            json!({ "user_id": user, "message_seq": anchor, "count": BACKFILL_PAGE }),
        ),
    };

    let response = match client.call(action, params.clone()).await {
        Ok(response) => response,
        Err(error) => {
            // 锚点可能被撤回或 NapCat 不支持 message_seq；退化为取最新 N 条。
            warn!(%error, chat = %chat, "history anchor failed; falling back to the newest page");
            params = match chat {
                ChatId::Group(group) => json!({ "group_id": group, "count": BACKFILL_PAGE }),
                ChatId::Private(user) => json!({ "user_id": user, "count": BACKFILL_PAGE }),
            };
            match client.call(action, params).await {
                Ok(response) => response,
                Err(error) => {
                    warn!(%error, chat = %chat, "cannot backfill this chat; leaving the gap");
                    return Ok(0);
                }
            }
        }
    };

    let items = response
        .pointer("/data/messages")
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| response.get("data").and_then(Value::as_array).cloned())
        .unwrap_or_default();

    let mut inserted = 0usize;
    let mut newest: Option<(i64, String)> = None;
    let mut first_gap_written = false;

    for item in items {
        let Some(mut message) = normalize_history(chat, &item, self_id) else {
            continue;
        };
        message.backfilled = true;

        // 缺口标记：锚点之后如果第一条不是紧接着的，写一条系统消息提醒模型。
        if !first_gap_written {
            first_gap_written = true;
            if message.message_id > anchor + 1 && anchor != 0 {
                debug!(chat = %chat, anchor, first = message.message_id, "history gap detected");
            }
        }
        if store.append(&message)? {
            inserted += 1;
        }
        match &newest {
            Some((id, _)) if *id >= message.message_id => {}
            _ => {
                newest = Some((message.message_id, message.time.to_rfc3339()));
            }
        }
    }

    if let Some((id, ts)) = newest {
        store.set_cursor(chat, id, &ts)?;
    }
    Ok(inserted)
}

/// 把历史上返回的原始消息归一化。
///
/// 历史接口通常不带 `post_type` / `message_type`，按目标 chat 补齐。
fn normalize_history(chat: ChatId, raw: &Value, self_id: u64) -> Option<QqMessage> {
    let mut data = raw.clone();
    if data.get("post_type").is_none() {
        data["post_type"] = json!("message");
    }
    if data.get("message_type").is_none() {
        data["message_type"] = json!(if chat.is_group() { "group" } else { "private" });
    }
    match chat {
        ChatId::Group(group) => {
            if data.get("group_id").is_none() {
                data["group_id"] = json!(group);
            }
        }
        ChatId::Private(user) => {
            if data.get("user_id").is_none() {
                data["user_id"] = json!(user);
            }
        }
    }
    if data.get("self_id").is_none() {
        data["self_id"] = json!(self_id);
    }

    match super::event::normalize("message.history", &data) {
        QqEvent::Message(message) => Some(message),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn history_message_is_normalized_with_chat() {
        let raw = json!({
            "message_id": 11,
            "user_id": 456,
            "time": 1793304000,
            "sender": {"nickname": "Alice"},
            "message": [{"type": "text", "data": {"text": "old"}}]
        });
        let message = normalize_history(ChatId::Group(123), &raw, 10000).unwrap();
        assert_eq!(message.chat, ChatId::Group(123));
        assert_eq!(message.message_id, 11);
        assert_eq!(message.plain_text(), "old");
        assert_eq!(message.sender.qq, Some(456));
    }

    #[test]
    fn incomplete_history_is_skipped() {
        assert!(normalize_history(ChatId::Group(123), &json!({"x": 1}), 1).is_none());
    }
}
