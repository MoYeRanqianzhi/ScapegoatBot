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
        #[serde(default, skip_serializing_if = "String::is_empty")]
        path: String,
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
    /// 是否为 agent 自己发出的。
    ///
    /// 解析时凡是本账号的消息（`message_sent` 回声或 `self_id` 匹配）都先记为 `true`；
    /// 事件循环再用发送账本复核，不是本进程发出的改成手打（[`Self::mark_typed_by_hand`]）。
    pub is_self: bool,
    /// 历史字段：断线补消息已删除，生产路径恒为 false；仅为旧记录兼容保留。
    pub backfilled: bool,
    /// 原始事件，保留供审计。
    pub raw: Value,
}

impl QqMessage {
    /// 本账号的消息不是本进程发出的：有人在这个账号上手打。
    ///
    /// 它不再是 agent 自己的话：`is_self` 清掉，发送者记成本账号的 QQ。
    /// stand_in 下本账号就是 `owner_qq`，于是这条消息自然就是主人的消息。
    pub fn mark_typed_by_hand(&mut self, account_qq: u64) {
        self.is_self = false;
        self.sender.qq = Some(account_qq);
    }

    /// 纯文本拼接（图片等以占位符出现）。
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Text { text } => out.push_str(text),
                Segment::At { qq } => out.push_str(&format!("@{qq}")),
                Segment::Reply { id } => out.push_str(&format!("[reply:{id}]")),
                Segment::Image { url, file, path } => {
                    let reference = if !url.is_empty() {
                        url
                    } else if !file.is_empty() {
                        file
                    } else {
                        path
                    };
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
                Segment::Image { url, file, path } => {
                    if !url.is_empty() {
                        url
                    } else if !file.is_empty() {
                        file
                    } else {
                        path
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
        data: Value,
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
            data: data.clone(),
        };
    }
    QqEvent::Unknown {
        name: name.to_owned(),
        data: data.clone(),
    }
}

fn parse_message(data: &Value, sent_event: bool) -> Option<QqMessage> {
    // 消息 id 是去重键的一部分；没有 id 的事件不按消息处理。
    let message_id = data.get("message_id").and_then(Value::as_i64)?;
    // 本账号 QQ；缺失时记 0，后面的“是否自己发的”判断会把 0 排除在外。
    let self_id = data.get("self_id").and_then(Value::as_u64).unwrap_or(0);
    // OneBot 的 `user_id` 永远是发送者，不是会话对象。
    let user_id = data.get("user_id").and_then(Value::as_u64);
    // 自己发出的消息：`message_sent` 回声，或发送者就是本账号。
    let from_self = sent_event || (self_id != 0 && user_id == Some(self_id));
    // 私聊的会话对象。别人发来的私聊，对象就是发送者；自己发出的私聊，
    // `user_id` 是本账号，对象在 `target_id`（NapCat 对私聊统一填 peerUin，
    // 见 NapCatQQ `packages/napcat-onebot/index.ts` 的 `target_id` 赋值）。
    // 缺 `target_id` 的实现只能退回本账号，即记成“自己和自己的会话”。
    let private_peer = if from_self {
        data.get("target_id")
            .and_then(Value::as_u64)
            .filter(|id| *id != 0)
            .or(user_id)
    } else {
        user_id
    };

    let chat = match data.get("message_type").and_then(Value::as_str) {
        Some("group") => ChatId::Group(data.get("group_id").and_then(Value::as_u64)?),
        // 临时会话也是 `private`（同时带来源群 `group_id`），按私聊对象归档。
        Some("private") => ChatId::Private(private_peer?),
        // `message_sent` 可能没有 message_type；用 group_id 判断。
        _ => match data.get("group_id").and_then(Value::as_u64) {
            Some(group) if group != 0 => ChatId::Group(group),
            _ => ChatId::Private(private_peer?),
        },
    };

    let sender = data.get("sender").cloned().unwrap_or(Value::Null);
    let sender = Sender {
        // 本账号的消息先统一记为 `self`（`None`）：在发送账本确认它不是本进程发的
        // 之前，绝不能让它被认成 owner_qq（stand_in 模式下二者相同）。
        qq: if from_self {
            None
        } else {
            user_id.filter(|id| *id != 0)
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
        is_self: from_self,
        backfilled: false,
        raw: data.clone(),
    })
}

pub(crate) fn parse_segments(message: Option<&Value>) -> Vec<Segment> {
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

/// 解析一个 OneBot 消息段；入站消息与自己发出的消息共用这一份规则。
pub(crate) fn parse_segment(value: &Value) -> Segment {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let data = value.get("data").cloned().unwrap_or(Value::Null);
    // 字段取字符串；id 一类字段有的实现（和模型）写成数字，按十进制文本保留。
    let text = |key: &str| match data.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    };
    match kind {
        "text" => Segment::Text { text: text("text") },
        "at" => Segment::At { qq: text("qq") },
        "reply" => Segment::Reply { id: text("id") },
        "image" => Segment::Image {
            url: text("url"),
            file: text("file"),
            path: text("path"),
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

/// 本地合成的消息 id：负数，避免与 NapCat 分配的 id 冲突（按微秒取值）。
pub(crate) fn synthetic_event_id() -> i64 {
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

    /// 发送账本认领不到的本账号消息是手打的：不再是 `self`，发送者是本账号。
    #[test]
    fn a_hand_typed_echo_carries_the_account_as_sender() {
        let mut data = group_message();
        data["post_type"] = json!("message_sent");
        let QqEvent::Message(mut message) = normalize("message_sent", &data) else {
            panic!("expected message");
        };
        message.mark_typed_by_hand(10000);
        assert!(!message.is_self);
        assert_eq!(message.sender.qq, Some(10000));
        assert_eq!(message.sender.label(), "10000");
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

    /// NapCat 对自己发出的私聊：`user_id` 是本账号，对方在 `target_id`。
    /// 以前按 `user_id` 归档，所有私聊回复都落进了 `private:<本账号>`。
    #[test]
    fn private_self_echo_is_filed_under_the_peer() {
        let data = json!({
            "post_type": "message_sent",
            "message_type": "private",
            "self_id": 10000,
            "user_id": 10000,
            "target_id": 456,
            "message_id": 2,
            "time": 1793304000,
            "sender": {"user_id": 10000, "nickname": "Me"},
            "message": [{"type": "text", "data": {"text": "hello"}}]
        });
        let QqEvent::Message(message) = normalize("message_sent.private.friend", &data) else {
            panic!("expected message");
        };
        assert_eq!(message.chat, ChatId::Private(456));
        assert!(message.is_self);
        assert_eq!(message.sender.qq, None);
    }

    /// 没有 `target_id` 的实现无法知道对方是谁，只能记成本账号自己的会话。
    #[test]
    fn private_self_echo_without_target_falls_back_to_the_account() {
        let data = json!({
            "post_type": "message_sent",
            "message_type": "private",
            "self_id": 10000,
            "user_id": 10000,
            "message_id": 3,
            "time": 1793304000,
            "message": [{"type": "text", "data": {"text": "note to self"}}]
        });
        let QqEvent::Message(message) = normalize("message_sent.private.friend", &data) else {
            panic!("expected message");
        };
        assert_eq!(message.chat, ChatId::Private(10000));
        assert!(message.is_self);
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

    #[test]
    fn image_path_is_preserved_when_url_and_file_are_empty() {
        let data = serde_json::json!({
            "post_type": "message",
            "message_type": "private",
            "self_id": 1,
            "user_id": 2,
            "message_id": 3,
            "time": 1793304000,
            "sender": {"nickname": "n"},
            "message": [{"type": "image", "data": {"path": "/tmp/a.png"}}]
        });
        let QqEvent::Message(message) = normalize("message.private.normal", &data) else {
            panic!("expected message");
        };
        assert_eq!(message.plain_text(), "[image: /tmp/a.png]");
    }
}
