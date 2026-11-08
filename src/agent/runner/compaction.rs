use super::*;

impl Runner {
    /// 自动压缩：超过工作集上限时压缩，并把 `working` 换成压缩后的历史。
    ///
    /// 两种回合（普通消息与控制/monitor 注入）共用，避免只在一边做压缩。
    pub(super) async fn maybe_auto_compact(
        &self,
        working: &mut Vec<AgentMessage>,
        history: &Arc<Mutex<Vec<AgentMessage>>>,
        sessions: &Arc<SessionStore>,
    ) {
        let estimated = crate::session::compaction::estimate_messages(working);
        if estimated <= self.compaction_budget() {
            return;
        }
        let failures = self
            .compaction_failures
            .load(std::sync::atomic::Ordering::Relaxed);
        if self.config.compaction_max_failures > 0
            && failures >= self.config.compaction_max_failures
        {
            warn!(
                failures,
                "automatic compaction is disabled after repeated failures"
            );
            return;
        }
        match self
            .compact_scope("auto", Arc::clone(history), Arc::clone(sessions))
            .await
        {
            Ok(_) => {
                self.compaction_failures
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                *working = history.lock().await.clone();
            }
            Err(error) => {
                self.compaction_failures
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                warn!(%error, "automatic compaction failed");
            }
        }
    }

    /// 清空上下文：轮转唯一 transcript，并清空内存历史。
    pub(crate) async fn clear_all_context(&self) -> Result<serde_json::Value> {
        // 清空上下文结束当前会话；SessionEnd 只允许 `log` / `notify`（设计 §7.5）。
        let _ = self
            .fire_hooks(
                crate::config::HookEvent::SessionEnd,
                None,
                Some("the context was cleared"),
            )
            .await;
        let current = self.deps.sessions.current()?;
        let next = self.deps.sessions.rotate()?;
        self.history.lock().await.clear();
        Ok(serde_json::json!({
            "cleared": true,
            "previous_session": current.file,
            "current_session": next.file,
        }))
    }

    /// 立即压缩：摘要旧消息，轮转新分段，写入摘要与保留窗口。
    ///
    /// 设计 §8.1：摘要块固定包含 `previous_session`，当前分段的路径由 runtime 注入。
    #[cfg(test)]
    pub(crate) async fn compact_now(&self, reason: &str) -> Result<serde_json::Value> {
        self.compact_chat(reason, ChatId::Private(self.config.owner_qq))
            .await
    }

    pub(crate) async fn compact_chat(
        &self,
        reason: &str,
        chat: ChatId,
    ) -> Result<serde_json::Value> {
        let history = self.history_for(chat).await;
        let sessions = self.session_for(chat).await;
        self.compact_scope(reason, history, sessions).await
    }

    /// 在指定链上压缩。
    async fn compact_scope(
        &self,
        reason: &str,
        history: Arc<Mutex<Vec<AgentMessage>>>,
        sessions: Arc<SessionStore>,
    ) -> Result<serde_json::Value> {
        let _guard = self.compaction_lock.lock().await;
        let snapshot = history.lock().await.clone();
        let (old, keep) = crate::session::compaction::split_for_compaction(
            &snapshot,
            self.config.keep_recent_tokens as usize,
        );
        if old.is_empty() {
            return Ok(serde_json::json!({
                "compacted": false,
                "reason": "nothing older than the retention window",
            }));
        }
        if self.config.check_on_compaction {
            self.run_security_review(&snapshot).await;
        }
        let _ = self
            .fire_hooks(crate::config::HookEvent::PreCompact, None, Some(reason))
            .await;
        // 压缩前把 durable facts 静默写进记忆，避免摘要把它们吞掉。
        if let Some(flusher) = &self.config.memory_flush
            && let Err(error) = flusher.flush(&snapshot, &self.slot).await
        {
            warn!(%error, "memory flush before compaction failed");
        }
        let mut input = crate::session::compaction::render_for_summary(&old, 80_000);
        let pre_compact = self.hook_context(crate::config::HookEvent::PreCompact, None);
        if !pre_compact.is_empty() {
            input.push_str("\n\n[hook context]\n");
            input.push_str(&pre_compact.join("\n"));
        }
        let previous = sessions.current()?;
        // 摘要可以用独立模型；默认仍是主 provider。
        let (provider, model) = match &self.config.compaction {
            Some(compaction) => (Arc::clone(&compaction.provider), compaction.model.clone()),
            None => {
                let slot = self.provider_snapshot();
                (slot.provider, slot.api_model)
            }
        };
        let summary = crate::session::compaction::summarize(&*provider, &model, &input, 4096)
            .await
            .map_err(|error| anyhow::anyhow!("compaction request failed: {error}"))?;

        let summary_message = AgentMessage::System {
            text: format!(
                "[checkpoint summary | previous_session=/sessions/{} | reason={reason}]\n\
{summary}",
                previous.file
            ),
        };
        let next = sessions.rotate()?;
        // 检查点与分段首行摘要同时落库；分段首行仍是权威。
        sessions
            .record_checkpoint(&previous.file, &summary)
            .map_err(|error| anyhow::anyhow!("cannot record the compaction checkpoint: {error}"))?;
        let mut new_history = vec![summary_message];
        // 保留消息上的 `usage` 描述的是压缩前那段（已经不存在的）提示：留着它，
        // `estimate_messages` 会拿它当基准，判定仍然超限，下次唤醒又压缩一次。
        new_history.extend(keep.into_iter().map(|mut message| {
            if let AgentMessage::Assistant(assistant) = &mut message {
                assistant.usage = Default::default();
            }
            message
        }));
        for text in self.hook_context(crate::config::HookEvent::PostCompact, None) {
            new_history.push(AgentMessage::System {
                text: format!("[hook context | PostCompact]\n{text}"),
            });
        }
        // PostCompact recall：把记忆索引作为新分段的第一条上下文，避免压缩后
        // 新回合在 system prompt 重建前“失忆”。只读、不标记已读。
        if self.config.post_compact_recall
            && let Ok(index) = self
                .deps
                .vfs
                .read_text_unrecorded(&crate::vfs::VfsPath::parse("/MEMORY.md").expect("valid"))
            && !index.trim().is_empty()
        {
            let mut text: String = index.chars().take(8_000).collect();
            if index.chars().count() > 8_000 {
                text.push('…');
            }
            new_history.push(AgentMessage::System {
                text: format!(
                    "<memory_context source=\"post_compact\">\n{text}\n</memory_context>"
                ),
            });
        }
        sessions.append(&new_history)?;
        let kept_messages = new_history.len() - 1;
        *history.lock().await = new_history;
        // PostCompact：hooks 可能依赖刚写入的记忆/技能，重新读盘并重建。
        self.rebuild_system();
        let _ = self
            .fire_hooks(
                crate::config::HookEvent::PostCompact,
                None,
                Some(&next.file),
            )
            .await;

        Ok(serde_json::json!({
            "compacted": true,
            "previous_session": previous.file,
            "current_session": next.file,
            "summary_bytes": summary.len(),
            "kept_messages": kept_messages,
        }))
    }

    /// 自动压缩阈值：配置的工作集上限与模型窗口减去预留后的较小者。
    ///
    /// 窗口与输出上限取当前 provider 槽位：热切换到窗口更小的模型后，阈值随之收紧。
    fn compaction_budget(&self) -> usize {
        let slot = self.provider_snapshot();
        // `context_window = 0` 表示模型没有声明窗口；只用配置的工作集上限。
        let by_window = if slot.context_window == 0 {
            self.config.turn_context_budget as usize
        } else {
            let reserve =
                u64::from(self.config.reserve_tokens).saturating_add(u64::from(slot.max_tokens));
            slot.context_window.saturating_sub(reserve).max(1) as usize
        };
        by_window
            .min(self.config.turn_context_budget as usize)
            .max(self.config.keep_recent_tokens as usize + 1)
    }
}
