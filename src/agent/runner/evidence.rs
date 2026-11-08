use super::*;

/// 本条消息是否来自主人本人：`owner_qq` 发出、且不是本进程自己发的。
///
/// stand_in 下主人在手机上手打的消息已被改记为 `owner_qq` 的非 self 消息
/// （见 `QqMessage::mark_typed_by_hand`），这里同样算主人。
pub(super) fn from_owner(owner_qq: u64, message: &QqMessage) -> bool {
    owner_qq != 0 && !message.is_self && message.sender.qq == Some(owner_qq)
}

/// 一批消息是否构成 owner intent：非 self 消息至少一条，且**全部**来自主人本人。
///
/// 别人的消息与主人的同批进入上下文时，模型照着谁的话行动无法区分，
/// 主人的授权不能被同批的其他人借用。转发/引用里的主人内容不算（发送者不是主人）。
pub(super) fn batch_owner_intent(owner_qq: u64, messages: &[QqMessage]) -> bool {
    let mut inbound = messages
        .iter()
        .filter(|message| !message.is_self)
        .peekable();
    inbound.peek().is_some() && inbound.all(|message| from_owner(owner_qq, message))
}

/// 聊天回合的 steering 来源：把队列送来的消息渲染进上下文，并据发送者更新证据。
///
/// 回合开始时的 owner intent 只覆盖开始时的批次；之后进入的非 self 消息只要有一条
/// 不是主人的，就撤销 intent——模型接下来照着谁的话行动无法区分。
pub(super) struct SteeredInbound<'a> {
    chat: ChatId,
    owner_qq: u64,
    evidence: TurnEvidence,
    receiver: &'a mut mpsc::UnboundedReceiver<QqMessage>,
}

impl<'a> SteeredInbound<'a> {
    pub(super) fn new(
        chat: ChatId,
        owner_qq: u64,
        evidence: TurnEvidence,
        receiver: &'a mut mpsc::UnboundedReceiver<QqMessage>,
    ) -> Self {
        Self {
            chat,
            owner_qq,
            evidence,
            receiver,
        }
    }
}

impl crate::agent::agent_loop::SteeringSource for SteeredInbound<'_> {
    fn try_next(&mut self) -> Option<AgentMessage> {
        let message = self.receiver.try_recv().ok()?;
        if !message.is_self && !from_owner(self.owner_qq, &message) {
            self.evidence.revoke_owner_intent();
        }
        let text = format::render_inbound(&message);
        self.evidence.record_source(self.chat.to_string(), &text);
        Some(AgentMessage::user(text))
    }
}

impl Runner {
    /// 触发批次的运行时证据：owner intent 见 [`batch_owner_intent`]，
    /// 批次内容记为本 chat 的上下文来源。
    pub(super) fn evidence_for_batch(&self, chat: ChatId, messages: &[QqMessage]) -> TurnEvidence {
        let owner_intent = batch_owner_intent(self.config.owner_qq, messages);
        let evidence = TurnEvidence::new(owner_intent, chat.to_string());
        for message in messages {
            evidence.record_source(chat.to_string(), format::render_inbound(message));
        }
        evidence
    }

    /// “上次回复以来的群消息”：提供上下文，但不触发回合。
    pub(super) fn pending_history(
        &self,
        chat: ChatId,
        first_batch_id: Option<i64>,
    ) -> Option<String> {
        let recent = self
            .deps
            .messages
            .recent(chat, self.config.chat_history_limit)
            .ok()?;
        let last_self = recent.iter().rposition(|message| message.is_self);
        let start = last_self.map(|index| index + 1).unwrap_or(0);
        let mut lines = Vec::new();
        for message in recent.iter().skip(start) {
            if message.is_self {
                continue;
            }
            if first_batch_id.is_some_and(|first| message.message_id >= first) {
                continue;
            }
            lines.push(format::render_inbound(message));
        }
        if lines.is_empty() {
            None
        } else {
            Some(format!(
                "[chat history since your last reply]\n{}",
                lines.join("\n")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::SteeringSource;
    use crate::qq::event::{Segment, Sender};

    const OWNER: u64 = 10001;

    fn from(qq: u64, is_self: bool) -> QqMessage {
        QqMessage {
            message_id: 1,
            chat: ChatId::Group(1),
            sender: Sender {
                qq: Some(qq),
                nickname: format!("u{qq}"),
                card: String::new(),
                role: "member".to_owned(),
            },
            time: chrono::Utc::now(),
            segments: vec![Segment::Text {
                text: "hello".to_owned(),
            }],
            reply_to: None,
            is_self,
            backfilled: false,
            raw: serde_json::Value::Null,
        }
    }

    #[test]
    fn owner_intent_needs_every_inbound_message_to_be_the_owners() {
        assert!(batch_owner_intent(OWNER, &[from(OWNER, false)]));
        // 本进程自己发的消息不参与判断。
        assert!(batch_owner_intent(
            OWNER,
            &[from(OWNER, false), from(OWNER, true)]
        ));
        // 别人的消息同批进入：主人的授权不能被借用。
        assert!(!batch_owner_intent(
            OWNER,
            &[from(OWNER, false), from(20002, false)]
        ));
        // 只有 self 消息、空批次、未配置 owner_qq 都没有 intent。
        assert!(!batch_owner_intent(OWNER, &[from(OWNER, true)]));
        assert!(!batch_owner_intent(OWNER, &[]));
        assert!(!batch_owner_intent(0, &[from(0, false)]));
    }

    #[test]
    fn a_steered_message_from_someone_else_revokes_owner_intent() {
        let evidence = TurnEvidence::owner("group:1");
        // 工具调用拿到的是克隆：撤销必须对它可见。
        let tool_view = evidence.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut inbound = SteeredInbound::new(ChatId::Group(1), OWNER, evidence, &mut rx);

        tx.send(from(OWNER, false)).unwrap();
        let rendered = inbound
            .try_next()
            .expect("the owner's message is delivered");
        assert!(matches!(rendered, AgentMessage::User { ref text, .. } if text.contains("hello")));
        assert!(
            tool_view.owner_intent(),
            "the owner's own message keeps intent"
        );

        tx.send(from(20002, false)).unwrap();
        assert!(inbound.try_next().is_some());
        assert!(
            !tool_view.owner_intent(),
            "someone else's message revokes intent"
        );

        // 撤销不可恢复：主人再说一句也不会重新授予。
        tx.send(from(OWNER, false)).unwrap();
        assert!(inbound.try_next().is_some());
        assert!(!tool_view.owner_intent());
        assert!(inbound.try_next().is_none());
        // 进入回合的内容都记为本 chat 的上下文来源。
        assert_eq!(tool_view.context_sources.lock().unwrap().len(), 3);
    }
}
