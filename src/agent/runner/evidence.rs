use super::*;

impl Runner {
    /// 触发批次里是否有主人本人的消息（转发/引用不算）。
    pub(super) fn evidence_for_batch(
        &self,
        chat: ChatId,
        messages: &[crate::qq::event::QqMessage],
    ) -> TurnEvidence {
        let owner = self.config.owner_qq;
        let owner_intent = owner != 0
            && messages
                .iter()
                .any(|message| !message.is_self && message.sender.qq == Some(owner));
        let evidence = TurnEvidence {
            owner_intent,
            origin_chat: Some(chat.to_string()),
            ..TurnEvidence::default()
        };
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
