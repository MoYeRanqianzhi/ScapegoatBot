//! 回合开始前的准备：接收入站消息、预算检查、组装即时提示。
//!
//! 这三步都发生在请求模型之前，由 `Runner::run_turn` 按顺序调用；模型循环与提交在
//! `runner.rs` 里。

use super::*;

impl Runner {
    /// 新回合现在是否被暂停：`sbot ai stop` 或 `ESTOP`。
    ///
    /// 两者都只挡新回合；消息已经入库，恢复之后的回合看得到。
    pub(super) fn new_turns_paused(&self, chat: ChatId) -> bool {
        if !self
            .deps
            .ai_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            debug!(chat = %chat, "ai is stopped; skipping this turn");
            return true;
        }
        if self.deps.estop_file.exists() {
            debug!(chat = %chat, "ESTOP is active; skipping this turn");
            return true;
        }
        false
    }

    /// 把触发回合的这批消息写进 transcript，并推进这个会话的游标。
    ///
    /// 返回两样东西：进入上下文的块（调用方并进共享历史），以及被接受的原始消息——
    /// `message_received` hook 拦下的消息不在其中，它们既不进 transcript 也不进上下文。
    ///
    /// 整批一次追加（要么全部写进分段，要么分段不变），成功之后调用方才把它们放进共享
    /// 历史：这批消息要么两边都有，要么两边都没有。逐条追加的话，中途失败时前几条只在
    /// transcript 里，运行中的上下文与重启后恢复出来的上下文就不一样了。
    pub(super) async fn accept_inbound(
        &self,
        chat: ChatId,
        messages: &[QqMessage],
        sessions: &SessionStore,
    ) -> Result<(Vec<AgentMessage>, Vec<QqMessage>)> {
        let mut inbound = Vec::with_capacity(messages.len());
        let mut accepted = Vec::new();
        for message in messages {
            let text = format::render_inbound(message);
            let hook = self
                .fire_hooks(
                    crate::config::HookEvent::MessageReceived,
                    Some(chat),
                    Some(&text),
                )
                .await;
            if let Some(reason) = hook.blocked {
                warn!(chat = %chat, %reason, "MessageReceived hook dropped this message");
                continue;
            }
            accepted.push(message.clone());
            inbound.push(AgentMessage::user(text));
        }
        sessions.append(&inbound)?;
        for message in &accepted {
            if let Err(error) =
                self.deps
                    .messages
                    .set_cursor(chat, message.message_id, &message.time.to_rfc3339())
            {
                warn!(%error, "cannot update chat cursor");
            }
        }
        Ok((inbound, accepted))
    }

    /// 这个回合是否因为预算跳过：每日 token / USD 用完，或这个会话一小时内的回合数到了
    /// `[budget].chat_turns_per_hour`。
    ///
    /// 没到上限时把这一回合记进该会话的小时窗口，所以每个回合只能调用一次。
    pub(super) async fn turn_budget_exhausted(&self, chat: ChatId) -> bool {
        let Some(budget) = &self.config.budget else {
            return false;
        };
        if let Some(reason) = budget.check_daily().await {
            warn!(chat = %chat, %reason, "skipping this turn");
            return true;
        }
        let limit = budget.chat_turns_per_hour();
        if limit == 0 {
            return false;
        }
        let mut turns = self.turn_times.lock().await;
        let queue = turns.entry(chat.to_string()).or_default();
        while queue
            .front()
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(3600))
        {
            queue.pop_front();
        }
        if queue.len() >= limit as usize {
            warn!(
                chat = %chat,
                limit,
                "chat turn budget exhausted; skipping this turn"
            );
            return true;
        }
        queue.push_back(Instant::now());
        false
    }

    /// 本回合的即时提示：runtime 块、`message_received` hook 注入的上下文、这个会话里
    /// 还没读过的近期消息、队列丢弃提示。
    ///
    /// 这些块只给这一次请求看，不进 transcript。近期消息同时记进 `evidence` 的来源：
    /// 把它们复述到别的会话要过跨会话的发送规则。
    pub(super) fn turn_prompts(
        &self,
        input: &TurnInput,
        any_accepted: bool,
        evidence: &TurnEvidence,
        sessions: &SessionStore,
    ) -> Vec<AgentMessage> {
        let chat = input.chat;
        let mut prompts = vec![AgentMessage::user(
            self.runtime_context(chat, sessions).render(),
        )];
        // MessageReceived 的 inject_context 只在有消息被接受时加入，每回合一次。
        if any_accepted {
            for text in self.hook_context(crate::config::HookEvent::MessageReceived, Some(chat)) {
                prompts.push(AgentMessage::user(format!(
                    "[hook context | MessageReceived]\n{text}"
                )));
            }
        }
        let first_batch_id = input.messages.first().map(|message| message.message_id);
        if let Some(pending) = self.pending_history(chat, first_batch_id) {
            evidence.record_source(chat.to_string(), &pending);
            prompts.push(AgentMessage::user(pending));
        }
        if input.dropped > 0 {
            prompts.push(AgentMessage::user(format!(
                "Note: {} earlier message(s) from this chat were dropped because the queue \
                 exceeded its limits. They were not read and cannot be recovered from context.",
                input.dropped
            )));
        }
        prompts
    }
}
