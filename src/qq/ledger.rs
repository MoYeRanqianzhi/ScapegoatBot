//! 发送账本：区分“本进程发出的消息”与“同一账号上有人手打的消息”。
//!
//! NapCat 对本账号发出的每条消息都会推一条 `message_sent` 回声（需要
//! `reportSelfMessage`）。回声本身分不出是谁发的：可能是 agent 通过 `API` 发的，
//! 也可能是 stand_in 模式下主人在手机上手打的。前者是 agent 自己的话（`self`），
//! 后者是主人说的话，应当像别人的消息一样进 inbox、交给 monitor 决定是否唤醒。
//!
//! 判断依据只有一个：这条消息是不是从本进程的 [`QqClient::call`](super::QqClient::call)
//! 发出去的。每次发送在调用**之前**登记（chat + 内容指纹），返回后补记 `message_id`；
//! 回声到达时按 id 或指纹认领。
//!
//! 安全方向：误判只能往“是 agent 发的”那边偏。把 agent 的话误认成主人的话，等于
//! 让 agent 伪造主人意图；反过来只是漏掉一条主人消息。所以：
//! - 同一 chat 有未认领的登记、且指纹相同（或登记无法给出指纹、或调用返回了却没有 id）
//!   → agent 发的；
//! - 同一 chat 还有发送在途、对不上 → 等它返回再按 id 判断；等不到也按 agent 发的；
//! - 只有确认没有任何对得上的发送，才认定是手打的。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::Notify;

use super::event::{ChatId, QqMessage, Segment, parse_segments};

/// 登记保留多久。回声通常在一秒内到达；留足余量应对 NapCat 重连后补推。
const RETENTION: Duration = Duration::from_secs(600);
/// 回声对不上、但同一 chat 有发送在途时，最多等多久。
const IN_FLIGHT_WAIT: Duration = Duration::from_secs(10);
/// 账本最多保留多少条；超过时丢最旧的（只影响极端突发下的认领精度）。
const MAX_ENTRIES: usize = 4096;

/// 会在 chat 里产生一条消息（因而产生回声）的 action。
///
/// 不在表里的 action（查询、群管理、戳一戳……）不登记。宁可多登记：多登记的代价只是
/// 极少数主人消息被当成 agent 的话。
pub(crate) fn produces_message(action: &str) -> bool {
    matches!(
        action,
        "send_msg"
            | "send_group_msg"
            | "send_private_msg"
            | "send_forward_msg"
            | "send_group_forward_msg"
            | "send_private_forward_msg"
            | "forward_friend_single_msg"
            | "forward_group_single_msg"
            | "upload_group_file"
            | "upload_private_file"
            | "send_group_ai_record"
    )
}

/// 一次登记。
#[derive(Debug)]
struct Entry {
    /// 本账本内唯一的序号，用来在调用返回时找回这条登记。
    ticket: u64,
    /// 目标 chat；解析不出时为 `None`，此时匹配任何 chat。
    chat: Option<ChatId>,
    /// 内容指纹；`None` 表示无法预知回声内容（转发单条消息等），匹配任何内容。
    fingerprint: Option<String>,
    /// 调用返回的消息 id。
    message_id: Option<i64>,
    /// 调用是否还没返回。
    in_flight: bool,
    /// 是否已被一条回声认领（一次发送只对应一条回声）。
    claimed: bool,
    created: Instant,
}

impl Entry {
    fn same_chat(&self, chat: ChatId) -> bool {
        self.chat.is_none_or(|own| own == chat)
    }
}

/// 发送账本。
#[derive(Debug, Default)]
pub struct OutboundLedger {
    entries: Mutex<VecDeque<Entry>>,
    next_ticket: Mutex<u64>,
    /// 任何一次发送返回时唤醒等待中的认领。
    settled: Notify,
}

impl OutboundLedger {
    /// 发送前登记，返回票据；调用返回后必须交给 [`Self::finish`]。
    pub(crate) fn begin(&self, action: &str, params: &Value) -> u64 {
        let ticket = {
            let mut next = self.next_ticket.lock().unwrap_or_else(|e| e.into_inner());
            *next += 1;
            *next
        };
        let entry = Entry {
            ticket,
            chat: target_chat(params),
            fingerprint: outgoing_fingerprint(action, params),
            message_id: None,
            in_flight: true,
            claimed: false,
            created: Instant::now(),
        };
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        prune(&mut entries);
        entries.push_back(entry);
        ticket
    }

    /// 调用返回：记下消息 id（失败或没有 id 时为 `None`），唤醒等待者。
    ///
    /// 失败的调用也保留登记：超时不代表没发出去。
    pub(crate) fn finish(&self, ticket: u64, response: Option<&Value>) {
        let message_id = response
            .and_then(|data| data.get("message_id"))
            .and_then(Value::as_i64);
        {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = entries.iter_mut().find(|entry| entry.ticket == ticket) {
                entry.in_flight = false;
                entry.message_id = message_id;
            }
        }
        self.settled.notify_waiters();
    }

    /// 这条本账号消息是不是本进程发出的；是则认领（之后同一次发送不再匹配）。
    pub async fn claim(&self, message: &QqMessage) -> bool {
        let fingerprint = echo_fingerprint(&message.segments);
        let deadline = Instant::now() + IN_FLIGHT_WAIT;
        loop {
            // 先登记 `notified`，再检查状态：检查与等待之间返回的发送不会漏掉唤醒。
            let settled = self.settled.notified();
            match self.try_claim(message, &fingerprint) {
                Verdict::Ours => return true,
                Verdict::NotOurs => return false,
                Verdict::Pending => {}
            }
            let now = Instant::now();
            if now >= deadline {
                // 等不到在途发送返回：按安全方向处理。
                return true;
            }
            let _ = tokio::time::timeout(deadline - now, settled).await;
        }
    }

    fn try_claim(&self, message: &QqMessage, fingerprint: &str) -> Verdict {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        prune(&mut entries);
        // 1) 精确：返回过的消息 id。
        if let Some(entry) = entries
            .iter_mut()
            .find(|entry| !entry.claimed && entry.message_id == Some(message.message_id))
        {
            entry.claimed = true;
            return Verdict::Ours;
        }
        // 2) 指纹：同一 chat、内容一致；登记无法给出指纹，或调用已返回却没拿到 id
        //    （失败/超时不代表没发出去）时，任何内容都算——按安全方向偏向“是 agent 发的”。
        if let Some(entry) = entries.iter_mut().find(|entry| {
            !entry.claimed
                && entry.same_chat(message.chat)
                && (entry
                    .fingerprint
                    .as_deref()
                    .is_none_or(|own| own == fingerprint)
                    || (!entry.in_flight && entry.message_id.is_none()))
        }) {
            entry.claimed = true;
            return Verdict::Ours;
        }
        // 3) 同一 chat 还有发送在途：可能就是它，等它返回看 id。
        if entries
            .iter()
            .any(|entry| entry.in_flight && !entry.claimed && entry.same_chat(message.chat))
        {
            return Verdict::Pending;
        }
        Verdict::NotOurs
    }
}

/// 认领结果。
enum Verdict {
    Ours,
    NotOurs,
    Pending,
}

/// 丢掉过期与超量的登记。
fn prune(entries: &mut VecDeque<Entry>) {
    while entries
        .front()
        .is_some_and(|entry| !entry.in_flight && entry.created.elapsed() > RETENTION)
    {
        entries.pop_front();
    }
    while entries.len() > MAX_ENTRIES {
        entries.pop_front();
    }
}

/// 发送参数里的目标 chat：`group_id` 优先，其次 `user_id`。
///
/// 与 NapCat `send_msg` 的取舍一致（同时带两个字段时 NapCat 发到群里）。
fn target_chat(params: &Value) -> Option<ChatId> {
    let id = |key: &str| {
        params.get(key).and_then(|value| match value {
            Value::Number(number) => number.as_u64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
    };
    id("group_id")
        .filter(|group| *group != 0)
        .map(ChatId::Group)
        .or_else(|| id("user_id").filter(|user| *user != 0).map(ChatId::Private))
}

/// 发出去的内容会在回声里长成什么样；无法预知时返回 `None`。
fn outgoing_fingerprint(action: &str, params: &Value) -> Option<String> {
    match action {
        "send_msg" | "send_group_msg" | "send_private_msg" => {
            Some(echo_fingerprint(&parse_segments(params.get("message"))))
        }
        // 合并转发的回声是一个 forward 段。
        "send_forward_msg" | "send_group_forward_msg" | "send_private_forward_msg" => {
            Some("[forward]".to_owned())
        }
        "upload_group_file" | "upload_private_file" => Some("[file]".to_owned()),
        // 转发单条消息、AI 语音：回声内容取决于原消息或服务端，无法预知。
        _ => None,
    }
}

/// 内容指纹：文本去掉全部空白，@ 与表情保留目标，媒体只保留种类；回复段不计。
///
/// NapCat 回声会重写媒体地址、在 @ 后补空格，所以只比较这些稳定的部分。
fn echo_fingerprint(segments: &[Segment]) -> String {
    let mut out = String::new();
    for segment in segments {
        match segment {
            Segment::Text { text } => {
                out.extend(text.chars().filter(|c| !c.is_whitespace()));
            }
            Segment::At { qq } => out.push_str(&format!("[at:{qq}]")),
            Segment::Face { id } => out.push_str(&format!("[face:{id}]")),
            Segment::Reply { .. } => {}
            Segment::Image { .. } => out.push_str("[image]"),
            Segment::File { .. } => out.push_str("[file]"),
            Segment::Record { .. } => out.push_str("[record]"),
            Segment::Video { .. } => out.push_str("[video]"),
            Segment::Other { kind, .. } => out.push_str(&format!("[{kind}]")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qq::event::Sender;
    use serde_json::json;

    fn echo(chat: ChatId, id: i64, segments: Vec<Segment>) -> QqMessage {
        QqMessage {
            message_id: id,
            chat,
            sender: Sender {
                qq: None,
                nickname: String::new(),
                card: String::new(),
                role: String::new(),
            },
            time: chrono::Utc::now(),
            segments,
            reply_to: None,
            is_self: true,
            backfilled: false,
            raw: Value::Null,
        }
    }

    fn text(text: &str) -> Vec<Segment> {
        vec![Segment::Text {
            text: text.to_owned(),
        }]
    }

    #[tokio::test]
    async fn an_echo_of_our_send_is_ours_even_before_the_call_returns() {
        let ledger = OutboundLedger::default();
        let ticket = ledger.begin(
            "send_group_msg",
            &json!({"group_id": 1, "message": [{"type": "text", "data": {"text": "hi there"}}]}),
        );
        // 回声先于响应到达：按指纹认领。
        assert!(
            ledger
                .claim(&echo(ChatId::Group(1), 9, text("hi  there")))
                .await
        );
        ledger.finish(ticket, Some(&json!({"message_id": 9})));
        // 一次发送只认领一条回声：主人随后手打的同样文字是主人的。
        assert!(
            !ledger
                .claim(&echo(ChatId::Group(1), 10, text("hi there")))
                .await
        );
    }

    #[tokio::test]
    async fn a_hand_typed_message_is_not_ours() {
        let ledger = OutboundLedger::default();
        let ticket = ledger.begin("send_private_msg", &json!({"user_id": 2, "message": "ok"}));
        ledger.finish(ticket, Some(&json!({"message_id": 5})));
        assert!(ledger.claim(&echo(ChatId::Private(2), 5, text("ok"))).await);
        assert!(
            !ledger
                .claim(&echo(ChatId::Private(2), 6, text("我自己打的")))
                .await
        );
        // 别的 chat 里的同样内容也与这次发送无关。
        assert!(!ledger.claim(&echo(ChatId::Private(3), 7, text("ok"))).await);
    }

    #[tokio::test]
    async fn a_mismatched_echo_waits_for_the_send_in_flight_and_matches_by_id() {
        let ledger = std::sync::Arc::new(OutboundLedger::default());
        let ticket = ledger.begin(
            "send_group_msg",
            &json!({"group_id": 1, "message": [{"type": "image", "data": {"file": "/tmp/a.png"}}]}),
        );
        let waiting = {
            let ledger = std::sync::Arc::clone(&ledger);
            tokio::spawn(async move {
                // NapCat 改写成了别的形状：指纹对不上，只能等 id。
                ledger
                    .claim(&echo(ChatId::Group(1), 42, text("[unexpected]")))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        ledger.finish(ticket, Some(&json!({"message_id": 42})));
        assert!(waiting.await.unwrap());
    }

    #[tokio::test]
    async fn sends_without_a_predictable_echo_claim_the_next_echo_in_that_chat() {
        let ledger = OutboundLedger::default();
        let ticket = ledger.begin(
            "forward_group_single_msg",
            &json!({"group_id": 1, "message_id": 3}),
        );
        ledger.finish(ticket, None);
        assert!(
            ledger
                .claim(&echo(ChatId::Group(1), 11, text("anything")))
                .await
        );
        assert!(
            !ledger
                .claim(&echo(ChatId::Group(1), 12, text("anything")))
                .await
        );
    }

    #[tokio::test]
    async fn a_send_that_failed_without_an_id_still_claims_its_echo() {
        let ledger = OutboundLedger::default();
        let ticket = ledger.begin("send_group_msg", &json!({"group_id": 1, "message": "hi"}));
        // 超时：没有响应，但消息可能已经发出。
        ledger.finish(ticket, None);
        assert!(
            ledger
                .claim(&echo(ChatId::Group(1), 8, text("rewritten")))
                .await
        );
    }

    #[test]
    fn only_message_producing_actions_are_recorded() {
        assert!(produces_message("send_group_msg"));
        assert!(produces_message("upload_private_file"));
        assert!(!produces_message("get_msg"));
        assert!(!produces_message("send_poke"));
    }
}
