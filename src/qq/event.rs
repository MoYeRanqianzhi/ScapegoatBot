//! QQ 事件归一化。
//!
//! 设计依据：设计 §4。
//!
//! NapCatLink 的 `EventData` 是“事件名 + 原始 JSON”；本模块把它翻译成内部类型：
//! - 消息（群 / 私聊 / 自身 echo）；
//! - 通知（撤回、入群、管理员变动等）；
//! - 请求（好友 / 群）；
//! - 元事件（心跳、生命周期）。
//!
//! 未知事件不丢弃原文：保留 `name` 与 `data`，供审计与扩展。

use std::fmt;

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 会话标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum ChatId {
    Group(u64),
    Private(u64),
}

impl ChatId {
    /// 解析 `group:<gid>` / `private:<qq>`。
    pub fn parse(raw: &str) -> Option<Self> {
        if let Some(id) = raw.strip_prefix("group:") {
            return id.parse().ok().filter(|id| *id != 0).map(Self::Group);
        }
        if let Some(id) = raw.strip_prefix("private:") {
            return id.parse().ok().filter(|id| *id != 0).map(Self::Private);
        }
        None
    }

    /// 群号或 QQ 号。
    pub fn id(self) -> u64 {
        match self {
            Self::Group(id) | Self::Private(id) => id,
        }
    }

    pub fn is_group(self) -> bool {
        matches!(self, Self::Group(_))
    }
}

impl fmt::Display for ChatId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Group(id) => write!(f, "group:{id}"),
            Self::Private(id) => write!(f, "private:{id}"),
        }
    }
}

/// 发送者。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sender {
    /// `None` 表示 agent 自己（`self`）。
    pub qq: Option<u64>,
    pub nickname: String,
    pub card: String,
    /// `owner` / `admin` / `member`（群角色）；私聊为空。
    pub role: String,
}

impl Sender {
    /// 渲染用标签：`self` 或 QQ 号。
    pub fn label(&self) -> String {
        match self.qq {
            None => "self".to_owned(),
            Some(qq) => qq.to_string(),
        }
    }

    /// 显示名：优先群名片，其次昵称。
    pub fn display_name(&self) -> &str {
        if !self.card.is_empty() {
            &self.card
        } else {
            &self.nickname
        }
    }
}

/// 消息段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Segment {
    Text {
        text: String,
    },
    At {
        qq: String,
    },
    Reply {
        id: String,
    },
    Image {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        url: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        file: String,
    },
    Face {
        id: String,
    },
    File {
        #[serde(default)]
        name: String,
        #[serde(default)]
        url: String,
    },
    Record {
        #[serde(default)]
        url: String,
        #[serde(default)]
        file: String,
    },
    Video {
        #[serde(default)]
        url: String,
    },
    /// 其它段保留原始 JSON，不丢信息。
    Other {
        kind: String,
        data: Value,
    },
}

impl Segment {
    /// 是否为纯文本（用于过滤与摘要）。
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }
}

/// 一条归一化消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QqMessage {
    pub message_id: i64,
    pub chat: ChatId,
    pub sender: Sender,
    pub time: DateTime<Utc>,
    pub segments: Vec<Segment>,
    pub reply_to: Option<i64>,
    /// 是否为 agent 自己发出的（`message_sent` echo 或 `self_id` 匹配）。
    pub is_self: bool,
    /// 是否由断线补消息产生（`[backfill]`，不触发回合）。
    pub backfilled: bool,
    /// 原始事件，保留供审计。
    pub raw: Value,
}

impl QqMessage {
    /// 纯文本拼接（图片等以占位符出现）。
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Text { text } => out.push_str(text),
                Segment::At { qq } => out.push_str(&format!("@{qq}")),
                Segment::Reply { id } => out.push_str(&format!("[reply:{id}]")),
                Segment::Image { url, file } => {
                    let reference = if !url.is_empty() { url } else { file };
                    out.push_str(&format!("[image: {reference}]"));
                }
                Segment::Face { id } => out.push_str(&format!("[face:{id}]")),
                Segment::File { name, .. } => out.push_str(&format!("[file: {name}]")),
                Segment::Record { .. } => out.push_str("[voice]"),
                Segment::Video { .. } => out.push_str("[video]"),
                Segment::Other { kind, .. } => out.push_str(&format!("[{kind}]")),
            }
        }
        out
    }

    /// 消息里的媒体引用（URL 优先，其次文件名）；用于记录来源。
    pub fn media_origins(&self) -> Vec<String> {
        let mut out = Vec::new();
        for segment in &self.segments {
            let origin = match segment {
                Segment::Image { url, file } => {
                    if url.is_empty() {
                        file
                    } else {
                        url
                    }
                }
                Segment::File { url, .. }
                | Segment::Record { url, .. }
                | Segment::Video { url } => url,
                _ => continue,
            };
            if !origin.is_empty() {
                out.push(origin.clone());
            }
        }
        out
    }

    /// 是否 @ 了本账号。
    pub fn mentions_self(&self, self_id: u64) -> bool {
        self.segments.iter().any(|segment| match segment {
            Segment::At { qq } => qq == "all" || qq.parse::<u64>().ok() == Some(self_id),
            _ => false,
        })
    }
}

/// 通知事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QqNotice {
    pub kind: String,
    pub chat: Option<ChatId>,
    pub actor: Option<u64>,
    pub target: Option<u64>,
    pub raw: Value,
}

/// 请求事件（好友 / 群）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QqRequest {
    pub kind: String,
    pub group: Option<u64>,
    pub actor: Option<u64>,
    pub comment: String,
    pub flag: String,
    pub raw: Value,
}

/// 归一化事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum QqEvent {
    Message(QqMessage),
    Notice(QqNotice),
    Request(QqRequest),
    Meta {
        kind: String,
    },
    Unknown {
        name: String,
        data: Value,
    },
    /// 连接状态变化（由本层产生，不是 OneBot 事件）。
    Connection {
        connected: bool,
        reason: String,
    },
}

/// 把 NapCatLink 的 `(name, data)` 转成内部事件。
pub fn normalize(name: &str, data: &Value) -> QqEvent {
    let post_type = data
        .get("post_type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if (post_type == "message" || post_type == "message_sent" || name.starts_with("message."))
        && let Some(message) = parse_message(data, post_type == "message_sent")
    {
        return QqEvent::Message(message);
    }
    if post_type == "notice" || name.starts_with("notice.") {
        return QqEvent::Notice(parse_notice(name, data));
    }
    if post_type == "request" || name.starts_with("request.") {
        return QqEvent::Request(parse_request(name, data));
    }
    if post_type == "meta_event" || name.starts_with("meta_event.") {
        return QqEvent::Meta {
            kind: name.to_owned(),
        };
    }
    QqEvent::Unknown {
        name: name.to_owned(),
        data: data.clone(),
    }
}

fn parse_message(data: &Value, sent_event: bool) -> Option<QqMessage> {
    let message_id = data.get("message_id").and_then(Value::as_i64)?;
    let self_id = data.get("self_id").and_then(Value::as_u64).unwrap_or(0);
    let user_id = data.get("user_id").and_then(Value::as_u64);

    let chat = match data.get("message_type").and_then(Value::as_str) {
        Some("group") => ChatId::Group(data.get("group_id").and_then(Value::as_u64)?),
        Some("private") => ChatId::Private(user_id?),
        // `message_sent` 可能没有 message_type；用 group_id 判断。
        _ => match data.get("group_id").and_then(Value::as_u64) {
            Some(group) if group != 0 => ChatId::Group(group),
            _ => ChatId::Private(user_id?),
        },
    };

    let sender = data.get("sender").cloned().unwrap_or(Value::Null);
    let sender_qq = user_id.filter(|id| *id != self_id);
    let sender = Sender {
        qq: if sent_event || sender_qq.is_none() {
            None
        } else {
            sender_qq
        },
        nickname: sender
            .get("nickname")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        card: sender
            .get("card")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        role: sender
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    };

    let timestamp = data.get("time").and_then(Value::as_i64).unwrap_or(0);
    let time = Utc
        .timestamp_opt(timestamp, 0)
        .single()
        .unwrap_or_else(Utc::now);

    let segments = parse_segments(data.get("message"));
    let reply_to = segments.iter().find_map(|segment| match segment {
        Segment::Reply { id } => id.parse().ok(),
        _ => None,
    });

    Some(QqMessage {
        message_id,
        chat,
        sender,
        time,
        segments,
        reply_to,
        is_self: sent_event || user_id == Some(self_id),
        backfilled: false,
        raw: data.clone(),
    })
}

fn parse_segments(message: Option<&Value>) -> Vec<Segment> {
    let Some(Value::Array(items)) = message else {
        // 字符串形式（`messagePostFormat = "string"`）降级为纯文本；配置要求 array，
        // 但这里仍要能解析，不能因为 NapCat 配置错误就整个事件丢失。
        if let Some(text) = message.and_then(Value::as_str) {
            return vec![Segment::Text {
                text: text.to_owned(),
            }];
        }
        return Vec::new();
    };

    items.iter().map(parse_segment).collect()
}

fn parse_segment(value: &Value) -> Segment {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let data = value.get("data").cloned().unwrap_or(Value::Null);
    let text = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    match kind {
        "text" => Segment::Text { text: text("text") },
        "at" => Segment::At { qq: text("qq") },
        "reply" => Segment::Reply { id: text("id") },
        "image" => Segment::Image {
            url: text("url"),
            file: text("file"),
        },
        "face" => Segment::Face { id: text("id") },
        "file" => Segment::File {
            name: text("name"),
            url: text("url"),
        },
        "record" => Segment::Record {
            url: text("url"),
            file: text("file"),
        },
        "video" => Segment::Video { url: text("url") },
        other => Segment::Other {
            kind: other.to_owned(),
            data,
        },
    }
}

fn parse_notice(name: &str, data: &Value) -> QqNotice {
    let group = data.get("group_id").and_then(Value::as_u64);
    let actor = data.get("user_id").and_then(Value::as_u64);
    let target = data.get("target_id").and_then(Value::as_u64);
    let chat = group
        .map(ChatId::Group)
        .or_else(|| actor.map(ChatId::Private));
    QqNotice {
        kind: name.strip_prefix("notice.").unwrap_or(name).to_owned(),
        chat,
        actor,
        target,
        raw: data.clone(),
    }
}

impl QqNotice {
    /// 把通知转成一条可进 inbox 的伪消息。
    ///
    /// QQ 的拍一拍、撤回等是 notice 而不是 message；不转换的话它们既不会
    /// 出现在 `/inbox`，也不会被 agent 看到。原文保留在 `raw`，正文只放
    /// 一个稳定的占位符，避免把原始 JSON 塞进上下文。
    pub fn to_inbox_message(&self) -> Option<QqMessage> {
        let chat = self.chat?;
        let subtype = self
            .raw
            .get("sub_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let message_id = self
            .raw
            .get("message_id")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let label = if self.kind.contains("notify") && subtype == "poke" {
            "poke".to_owned()
        } else if self.kind.contains("recall") {
            "recall".to_owned()
        } else {
            self.kind.clone()
        };
        let summary = match label.as_str() {
            "poke" => format!(
                "[poke] {} -> {}",
                self.actor.unwrap_or_default(),
                self.target.unwrap_or_default()
            ),
            "recall" if message_id != 0 => format!("[recall] message_id={message_id}"),
            "recall" => "[recall]".to_owned(),
            other => format!("[{other}]"),
        };
        let text = with_event_details(summary, &self.raw);
        let time = self
            .raw
            .get("time")
            .and_then(Value::as_i64)
            .and_then(|seconds| Utc.timestamp_opt(seconds, 0).single())
            .unwrap_or_else(Utc::now);
        Some(QqMessage {
            message_id: synthetic_event_id(),
            chat,
            sender: Sender {
                qq: self.actor,
                nickname: String::new(),
                card: String::new(),
                role: String::new(),
            },
            time,
            segments: vec![Segment::Text { text }],
            reply_to: None,
            is_self: false,
            backfilled: false,
            raw: self.raw.clone(),
        })
    }
}

/// 事件正文 = 一行摘要 + 压缩后的原始 JSON；inbox/上下文需要细节。
fn with_event_details(summary: String, raw: &Value) -> String {
    let details = serde_json::to_string(raw).unwrap_or_default();
    let details: String = details.chars().take(1200).collect();
    if details.is_empty() {
        summary
    } else {
        format!("{summary} {details}")
    }
}

impl QqRequest {
    /// 好友/群请求也进 inbox；`comment` 与原始字段都保留。
    pub fn to_inbox_message(&self) -> Option<QqMessage> {
        let chat = self
            .group
            .map(ChatId::Group)
            .or_else(|| self.actor.map(ChatId::Private))?;
        let summary = format!(
            "[request] {} from {}: {}",
            self.kind,
            self.actor.unwrap_or_default(),
            self.comment
        );
        Some(QqMessage {
            message_id: synthetic_event_id(),
            chat,
            sender: Sender {
                qq: self.actor,
                nickname: String::new(),
                card: String::new(),
                role: String::new(),
            },
            time: Utc::now(),
            segments: vec![Segment::Text {
                text: with_event_details(summary, &self.raw),
            }],
            reply_to: None,
            is_self: false,
            backfilled: false,
            raw: self.raw.clone(),
        })
    }
}

/// 未知事件只要带 group_id / user_id 也入库，避免丢细节。
pub fn unknown_to_inbox_message(name: &str, data: &Value) -> Option<QqMessage> {
    let chat = data
        .get("group_id")
        .and_then(Value::as_u64)
        .map(ChatId::Group)
        .or_else(|| {
            data.get("user_id")
                .and_then(Value::as_u64)
                .map(ChatId::Private)
        })?;
    let summary = format!("[unknown] {name}");
    Some(QqMessage {
        message_id: synthetic_event_id(),
        chat,
        sender: Sender {
            qq: data.get("user_id").and_then(Value::as_u64),
            nickname: String::new(),
            card: String::new(),
            role: String::new(),
        },
        time: Utc::now(),
        segments: vec![Segment::Text {
            text: with_event_details(summary, data),
        }],
        reply_to: None,
        is_self: false,
        backfilled: false,
        raw: data.clone(),
    })
}

fn synthetic_event_id() -> i64 {
    -(Utc::now().timestamp_micros().unsigned_abs() as i64).max(1)
}

fn parse_request(name: &str, data: &Value) -> QqRequest {
    QqRequest {
        kind: name.strip_prefix("request.").unwrap_or(name).to_owned(),
        group: data.get("group_id").and_then(Value::as_u64),
        actor: data.get("user_id").and_then(Value::as_u64),
        comment: data
            .get("comment")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        flag: data
            .get("flag")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        raw: data.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn group_message() -> Value {
        json!({
            "post_type": "message",
            "message_type": "group",
            "self_id": 10000,
            "group_id": 123,
            "user_id": 456,
            "message_id": 789,
            "time": 1793304000,
            "sender": {"nickname": "Alice", "card": "小A", "role": "admin"},
            "message": [
                {"type": "text", "data": {"text": "hello "}},
                {"type": "at", "data": {"qq": "10000"}},
                {"type": "image", "data": {"url": "https://example.com/a.png"}}
            ]
        })
    }

    #[test]
    fn parses_group_message() {
        let event = normalize("message.group.normal", &group_message());
        let QqEvent::Message(message) = event else {
            panic!("expected message");
        };
        assert_eq!(message.chat, ChatId::Group(123));
        assert_eq!(message.sender.qq, Some(456));
        assert_eq!(message.sender.display_name(), "小A");
        assert_eq!(message.message_id, 789);
        assert!(message.mentions_self(10000));
        assert_eq!(
            message.plain_text(),
            "hello @10000[image: https://example.com/a.png]"
        );
        assert!(!message.is_self);
    }

    #[test]
    fn self_echo_is_self_and_does_not_become_owner() {
        let mut data = group_message();
        data["post_type"] = json!("message_sent");
        let event = normalize("message_sent", &data);
        let QqEvent::Message(message) = event else {
            panic!("expected message");
        };
        assert!(message.is_self);
        assert_eq!(message.sender.qq, None);
        assert_eq!(message.sender.label(), "self");
    }

    #[test]
    fn private_message_uses_user_id() {
        let data = json!({
            "post_type": "message",
            "message_type": "private",
            "self_id": 10000,
            "user_id": 456,
            "message_id": 1,
            "time": 1793304000,
            "sender": {"nickname": "Bob"},
            "message": [{"type": "text", "data": {"text": "hi"}}]
        });
        let QqEvent::Message(message) = normalize("message.private.friend", &data) else {
            panic!("expected message");
        };
        assert_eq!(message.chat, ChatId::Private(456));
    }

    #[test]
    fn chat_id_roundtrip() {
        for raw in ["group:123", "private:456"] {
            let chat = ChatId::parse(raw).unwrap();
            assert_eq!(chat.to_string(), raw);
        }
        assert!(ChatId::parse("group:0").is_none());
        assert!(ChatId::parse("nope").is_none());
    }

    #[test]
    fn notices_and_requests_keep_raw() {
        let notice = json!({
            "post_type": "notice",
            "notice_type": "group_recall",
            "group_id": 123,
            "user_id": 456,
            "target_id": 789
        });
        match normalize("notice.group_recall", &notice) {
            QqEvent::Notice(notice) => {
                assert_eq!(notice.kind, "group_recall");
                assert_eq!(notice.chat, Some(ChatId::Group(123)));
                assert_eq!(notice.target, Some(789));
            }
            other => panic!("unexpected {other:?}"),
        }

        let request = json!({
            "post_type": "request",
            "request_type": "friend",
            "user_id": 456,
            "comment": "hi",
            "flag": "f1"
        });
        match normalize("request.friend", &request) {
            QqEvent::Request(request) => {
                assert_eq!(request.kind, "friend");
                assert_eq!(request.flag, "f1");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unknown_event_is_preserved() {
        match normalize("weird.event", &json!({"x": 1})) {
            QqEvent::Unknown { name, data } => {
                assert_eq!(name, "weird.event");
                assert_eq!(data["x"], 1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn string_message_format_still_parses() {
        let mut data = group_message();
        data["message"] = json!("plain text");
        let QqEvent::Message(message) = normalize("message.group.normal", &data) else {
            panic!("expected message");
        };
        assert_eq!(message.plain_text(), "plain text");
    }

    #[test]
    fn poke_notice_becomes_an_inbox_message() {
        let data = json!({
            "post_type": "notice",
            "notice_type": "notify",
            "sub_type": "poke",
            "self_id": 10000,
            "group_id": 123,
            "user_id": 456,
            "target_id": 10000,
            "time": 1793304000
        });
        let QqEvent::Notice(notice) = normalize("notice.notify.poke", &data) else {
            panic!("expected notice");
        };
        let message = notice
            .to_inbox_message()
            .expect("notice should map to inbox");
        assert_eq!(message.chat, ChatId::Group(123));
        assert_eq!(message.sender.qq, Some(456));
        assert!(message.plain_text().starts_with("[poke] 456 -> 10000"));
        assert!(message.plain_text().contains("\"sub_type\":\"poke\""));
        assert!(!message.is_self);
    }

    #[test]
    fn recall_notice_becomes_an_inbox_message() {
        let data = json!({
            "post_type": "notice",
            "notice_type": "friend_recall",
            "self_id": 10000,
            "user_id": 456,
            "message_id": 789,
            "time": 1793304000
        });
        let QqEvent::Notice(notice) = normalize("notice.friend_recall", &data) else {
            panic!("expected notice");
        };
        let message = notice
            .to_inbox_message()
            .expect("notice should map to inbox");
        assert_eq!(message.chat, ChatId::Private(456));
        assert!(message.plain_text().starts_with("[recall] message_id=789"));
    }

    #[test]
    fn request_and_unknown_events_enter_inbox_with_details() {
        let request = parse_request(
            "request.group.add",
            &json!({
                "group_id": 123,
                "user_id": 456,
                "comment": "hello",
                "flag": "f1"
            }),
        );
        let message = request.to_inbox_message().expect("request should map");
        assert_eq!(message.chat, ChatId::Group(123));
        assert!(
            message
                .plain_text()
                .starts_with("[request] group.add from 456: hello")
        );
        assert!(message.plain_text().contains("\"flag\":\"f1\""));

        let unknown = unknown_to_inbox_message(
            "custom.event",
            &json!({"group_id": 123, "x": {"nested": true}}),
        )
        .expect("unknown with chat should map");
        assert_eq!(unknown.chat, ChatId::Group(123));
        assert!(unknown.plain_text().contains("custom.event"));
        assert!(unknown.plain_text().contains("\"nested\":true"));
    }
}
