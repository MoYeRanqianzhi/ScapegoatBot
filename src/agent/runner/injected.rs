use super::*;

impl Runner {
    /// 控制通道回合；主代理不受并发配额限制，串行化由 turn_lock 负责。
    pub(crate) async fn run_control(&self, text: &str, control_id: i64) -> Result<LoopOutcome> {
        if !self
            .deps
            .ai_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            anyhow::bail!("AI calls are stopped; run `sbot ai start` first");
        }
        let message = AgentMessage::user(format!(
            "<owner_context source=\"control:{control_id}\">\n{text}\n</owner_context>"
        ));
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
        let turn_lock = self.turn_lock_for(chat).await;
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
        let sessions = self.session_for(chat).await;
        let history = self.history_for(chat).await;
        {
            let mut history = history.lock().await;
            history.push(message.clone());
        }
        sessions.append(&[message])?;

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
            self.loop_config(),
        );
        let (tx, mut rx) = mpsc::channel(256);
        let logger = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if let AgentEvent::Error { message } = event {
                    warn!(%message, "agent error");
                }
            }
        });
        let mut steering = SteeringQueue::default();
        let outcome = loop_
            .run_with_steering(&mut working, prompts, &mut steering, None, &tx)
            .await;
        drop(tx);
        let _ = logger.await;
        if let Some(budget) = &self.config.budget {
            budget
                .record_usage(outcome.usage.input_tokens, outcome.usage.output_tokens)
                .await;
        }

        for message in steering.drain() {
            working.push(message);
        }
        let new_blocks: Vec<AgentMessage> = working[base + runtime_blocks..].to_vec();
        if !new_blocks.is_empty() {
            let turn_id = new_turn_id();
            let scope = "shared".to_owned();
            for (seq, message) in new_blocks.iter().enumerate() {
                self.deps
                    .turn_buffer
                    .append(&turn_id, seq as u64, &scope, message)?;
            }
            self.deps.turn_buffer.commit(&sessions, &turn_id).await?;
            history.lock().await.extend(new_blocks.clone());
        }
        self.accumulate_review_tokens(
            prompt_tokens + crate::session::compaction::estimate_messages(&new_blocks),
        );
        self.maybe_security_review(&history).await;
        Ok(outcome)
    }
}
