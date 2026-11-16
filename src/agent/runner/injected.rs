use super::*;

impl Runner {
    /// 控制通道回合；主代理不受并发配额限制，串行化由 turn_lock 负责。
    ///
    /// 只看 `sbot ai stop`，不看 ESTOP：`sbot pause` 停的是 QQ / monitor 回合与子代理，宿主机
    /// 上的 `sbot chat` 在暂停期间照常可用（主人要靠它问清楚状况再决定是否恢复）。
    ///
    /// `scope` 是 `sbot chat --scope` 指定的目标会话：写进 `owner_context` 的 `scope`
    /// 属性，告诉模型这句话针对哪个群 / 私聊；省略时由模型自己判断。
    pub(crate) async fn run_control(
        &self,
        text: &str,
        control_id: i64,
        scope: Option<ChatId>,
    ) -> Result<LoopOutcome> {
        if !self
            .deps
            .ai_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            anyhow::bail!("AI calls are stopped; run `sbot ai start` first");
        }
        let message = AgentMessage::user(control_envelope(text, control_id, scope));
        self.run_injected("control", message, true).await
    }

    /// monitor 投递：以 `[monitor: name | source | time]` 信封进入上下文。
    pub(crate) async fn run_event(
        &self,
        name: &str,
        source: &str,
        text: &str,
        owner_intent: bool,
    ) -> Result<LoopOutcome> {
        if !self
            .deps
            .ai_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            anyhow::bail!("AI calls are stopped; run `sbot ai start` first");
        }
        if self.deps.estop_file.exists() {
            anyhow::bail!("ESTOP is active");
        }
        let message = AgentMessage::user(format!(
            "[monitor: {} | {} | {}]\n{}",
            name,
            source,
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            text
        ));
        self.run_injected("monitor", message, owner_intent).await
    }

    /// 注入一条上下文消息并跑一个回合（控制通道与 monitor 共用）。
    async fn run_injected(
        &self,
        origin: &str,
        message: AgentMessage,
        owner_intent: bool,
    ) -> Result<LoopOutcome> {
        let chat = ChatId::Private(self.config.owner_qq);
        let turn_lock = Arc::clone(&self.turn_lock);
        let _turn_guard = turn_lock.lock().await;
        self.discard_context_if_flagged().await;
        self.active_turns
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _active = ActiveTurnGuard(&self.active_turns);
        if let Some(budget) = &self.config.budget
            && let Some(reason) = budget.check_daily().await
        {
            warn!(%reason, "skipping this injected turn");
            return Ok(LoopOutcome {
                stop_reason: crate::agent::StopReason::Aborted,
                usage: Default::default(),
                turns: 0,
            });
        }
        let sessions = Arc::clone(&self.deps.sessions);
        let history = Arc::clone(&self.history);
        // 先落盘，成功之后才进共享历史：写不进 transcript 的消息也不留在内存里，运行中的
        // 上下文与重启后从分段恢复出来的上下文始终一致。
        sessions.append(std::slice::from_ref(&message))?;
        history.lock().await.push(message);

        let mut working = { history.lock().await.clone() };
        // 控制/monitor 回合同样可能很长：以前没有压缩，长会话会一直涨到超窗。
        self.maybe_auto_compact(&mut working, &history, &sessions)
            .await;
        let base = working.len();
        let runtime = RuntimeContext {
            origin_chat: origin.to_owned(),
            ..self.runtime_context(chat, &sessions)
        };
        let prompts = vec![AgentMessage::user(runtime.render())];
        let runtime_blocks = prompts.len();
        let prompt_tokens = crate::session::compaction::estimate_messages(&prompts);

        let evidence = if owner_intent {
            TurnEvidence::owner(origin)
        } else {
            TurnEvidence::monitor()
        };
        let executor: Arc<dyn crate::agent::ToolExecutor> = Arc::new(TurnExecutor {
            registry: Arc::clone(&self.deps.tools),
            evidence,
        });
        let slot = self.provider_snapshot();
        let loop_ = AgentLoop::new_with_fallbacks(
            Arc::clone(&slot.provider),
            self.fallbacks.clone(),
            executor,
            Arc::clone(&self.deps.gate),
            self.loop_config(&slot),
        );
        let turn_id = new_turn_id();
        let mut journal = super::journal::TurnJournal::new(
            Arc::clone(&self.deps.turn_buffer),
            turn_id.clone(),
            runtime_blocks,
        );
        let (tx, mut rx) = mpsc::channel(256);
        let logger = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    AgentEvent::Appended { message } => journal.record(&message),
                    AgentEvent::Error { message } => warn!(%message, "agent error"),
                    _ => {}
                }
            }
            journal
        });
        let mut steering = SteeringQueue::default();
        let outcome = loop_
            .run_with_steering(&mut working, prompts, &mut steering, None, &tx)
            .await;
        drop(tx);
        let journal = match logger.await {
            Ok(journal) => journal,
            // 同一个 turn id 重建：提交时按 seq 覆盖重写，不会重复。
            Err(error) => {
                warn!(%error, "the turn event task failed");
                super::journal::TurnJournal::new(Arc::clone(&self.deps.turn_buffer), turn_id, 0)
            }
        };
        // 用量已由计量 provider 逐请求记账（见 `providers::metered`）。

        for message in steering.drain() {
            working.push(message);
        }
        let new_blocks: Vec<AgentMessage> = working[base + runtime_blocks..].to_vec();
        journal.commit(&sessions, &new_blocks).await?;
        if !new_blocks.is_empty() {
            history.lock().await.extend(new_blocks.clone());
        }
        self.accumulate_review_tokens(
            prompt_tokens + crate::session::compaction::estimate_messages(&new_blocks),
        );
        self.maybe_security_review(&history).await;
        Ok(outcome)
    }
}

/// 控制输入的信封：`<owner_context source="control:<id>" [scope="group:<gid>"]>`。
fn control_envelope(text: &str, control_id: i64, scope: Option<ChatId>) -> String {
    // 目标会话只是 `group:<n>` / `private:<n>`，不含引号或尖括号，可以直接放进属性。
    let scope = scope
        .map(|chat| format!(" scope=\"{chat}\""))
        .unwrap_or_default();
    format!("<owner_context source=\"control:{control_id}\"{scope}>\n{text}\n</owner_context>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_envelope_names_the_target_chat_only_when_given() {
        assert_eq!(
            control_envelope("hi", 7, None),
            "<owner_context source=\"control:7\">\nhi\n</owner_context>"
        );
        assert_eq!(
            control_envelope("hi", 7, Some(ChatId::Group(123))),
            "<owner_context source=\"control:7\" scope=\"group:123\">\nhi\n</owner_context>"
        );
    }
}
