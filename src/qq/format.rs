//! QQ 消息的渲染与出站转换。
//!
//! 设计依据：设计 §4「入站信封」与 §7.1「内部标签不得出现在 QQ」。
//!
//! 入站信封固定英文标签 + 原文（原文不翻译）：
//! ```text
//! [group:123 | 2026-10-29 21:04:03 | 10001 (nickname) | id: 55555 | reply_to: 98765]
//! <original text>
//! ```
//! 时间使用主人本地时区，精确到秒。

use chrono::Local;
use napcat_link::MessageSegment;

use super::event::{ChatId, QqMessage, Segment};

/// 渲染单条入站消息。
pub fn render_inbound(message: &QqMessage) -> String {
    let time = message
        .time
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S");
    let mut header = format!("[{} | {} | {}", message.chat, time, message.sender.label());
    let name = message.sender.display_name();
    if !name.is_empty() {
        header.push_str(&format!(" ({name})"));
    }
    header.push_str(&format!(" | id: {}", message.message_id));
    if let Some(reply_to) = message.reply_to {
        header.push_str(&format!(" | reply_to: {reply_to}"));
    }
    if message.backfilled {
        header.push_str(" | backfill");
    }
    header.push(']');

    format!("{header}\n{}", message.plain_text())
}

/// 渲染一批消息（同一回合内合并注入）。
pub fn render_batch(messages: &[QqMessage]) -> String {
    messages
        .iter()
        .map(render_inbound)
        .collect::<Vec<_>>()
        .join("\n")
}

/// 内部注入标签；发送前必须从模型输出里剥离（设计 §7.1 / §13）。
const INTERNAL_TAGS: &[&str] = &[
    "runtime_context",
    "owner_context",
    "user_context",
    "memory_context",
    "style_profile",
    "monitor",
    "system_prompt",
];

/// 剥离内部标签与边界标记。
///
/// 只处理成对标签；未闭合的标签保留原文，避免误删正常文本。
pub fn strip_internal_tags(text: &str) -> String {
    let mut cleaned = text.to_owned();
    for tag in INTERNAL_TAGS {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        while let Some(start) = cleaned.find(&open) {
            // 找到标签名后的 `>`，确保是完整开标签。
            let Some(open_end) = cleaned[start..].find('>') else {
                break;
            };
            let open_end = start + open_end;
            let Some(close_start) = cleaned[open_end..].find(&close) else {
                break;
            };
            let close_start = open_end + close_start;
            let close_end = close_start + close.len();
            cleaned.replace_range(start..close_end, "");
        }
    }
    cleaned.replace("__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__", "")
}

/// 把模型输出转换成 NapCat 消息段。
///
/// 默认纯文本；`reply_to` 存在时在最前面加 reply 段。
pub fn to_segments(reply_to: Option<i64>, text: &str) -> Vec<MessageSegment> {
    let mut segments = Vec::new();
    if let Some(id) = reply_to {
        segments.push(MessageSegment::Reply {
            id: Some(id.to_string()),
            seq: None,
        });
    }
    let cleaned = strip_internal_tags(text);
    if !cleaned.trim().is_empty() {
        segments.push(MessageSegment::Text { text: cleaned });
    }
    segments
}

/// 把内部消息段转成 NapCat 段（发送文件、图片等 action 用）。
pub fn segment_to_napcat(segment: &Segment) -> Option<MessageSegment> {
    Some(match segment {
        Segment::Text { text } => MessageSegment::Text { text: text.clone() },
        Segment::At { qq } => MessageSegment::At {
            qq: qq.clone(),
            name: None,
        },
        Segment::Face { id } => MessageSegment::Face {
            id: id.clone(),
            result_id: None,
            chain_count: None,
        },
        Segment::Image { url, file, path } => MessageSegment::Image {
            file: if url.is_empty() {
                file.clone()
            } else {
                url.clone()
            },
            summary: None,
            sub_type: None,
            url: None,
            name: None,
            path: if path.is_empty() {
                None
            } else {
                Some(path.clone())
            },
            thumb: None,
        },
        // 其余段类型差异大，统一交给 `API` 工具按原始 JSON 透传。
        _ => return None,
    })
}

/// 会话类型用于统计与路径。
pub fn scope_name(chat: ChatId) -> &'static str {
    if chat.is_group() { "group" } else { "private" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qq::event::Sender;
    use chrono::TimeZone;

    fn message() -> QqMessage {
        QqMessage {
            message_id: 55555,
            chat: ChatId::Group(123),
            sender: Sender {
                qq: Some(10001),
                nickname: "nickname".to_owned(),
                card: String::new(),
                role: "member".to_owned(),
            },
            time: Local
                .with_ymd_and_hms(2026, 10, 29, 21, 4, 3)
                .unwrap()
                .with_timezone(&chrono::Utc),
            segments: vec![Segment::Text {
                text: "hello".to_owned(),
            }],
            reply_to: Some(98765),
            is_self: false,
            backfilled: false,
            raw: serde_json::Value::Null,
        }
    }

    #[test]
    fn envelope_matches_design() {
        let rendered = render_inbound(&message());
        assert!(rendered.starts_with("[group:123 | "));
        assert!(rendered.contains("| 10001 (nickname) | id: 55555 | reply_to: 98765]"));
        assert!(rendered.ends_with("\nhello"));
    }

    #[test]
    fn backfill_is_marked() {
        let mut message = message();
        message.backfilled = true;
        assert!(render_inbound(&message).contains("| backfill]"));
    }

    #[test]
    fn strip_removes_paired_internal_tags() {
        let text = "hi <runtime_context>secret</runtime_context> there";
        assert_eq!(strip_internal_tags(text), "hi  there");
        let text = "<owner_context source=\"/AGENTS.md\">rules</owner_context>answer";
        assert_eq!(strip_internal_tags(text), "answer");
        // 未闭合的标签保留。
        assert_eq!(
            strip_internal_tags("<runtime_context>oops"),
            "<runtime_context>oops"
        );
        assert_eq!(
            strip_internal_tags("a__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__b"),
            "ab"
        );
    }

    #[test]
    fn outbound_reply_is_first_segment() {
        let segments = to_segments(Some(42), "hello <memory_context>x</memory_context>");
        assert!(matches!(
            &segments[0],
            MessageSegment::Reply { id: Some(id), .. } if id == "42"
        ));
        match &segments[1] {
            MessageSegment::Text { text } => assert_eq!(text, "hello "),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn empty_outbound_has_no_text_segment() {
        assert!(to_segments(None, "   ").is_empty());
    }
}
