use super::*;

struct PreparedCompaction {
    snapshot: Vec<AgentMessage>,
    cut: usize,
    session_file: String,
    summary: String,
    reason: String,
}

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
        // 上下文空了，之前压缩失败的原因不再成立：自动压缩重新开始尝试。
        self.compaction_failures
            .store(0, std::sync::atomic::Ordering::Relaxed);
        Ok(serde_json::json!({
            "cleared": true,
            "previous_session": current.file,
            "current_session": next.file,
        }))
    }

    /// 立即压缩：摘要旧消息，轮转新分段，写入摘要与保留窗口。
    ///
    /// 设计 §8.1：摘要块固定包含 `previous_session`，当前分段的路径由 runtime 注入。
    pub(crate) async fn compact_now(&self, reason: &str) -> Result<serde_json::Value> {
        self.compact_scope(
            reason,
            Arc::clone(&self.history),
            Arc::clone(&self.deps.sessions),
        )
        .await
    }

    /// 把要压缩掉的那段历史交给模型摘要（附上 PreCompact hook 的上下文）。
    ///
    /// 摘要可以用独立模型（`[compaction].model`）；默认是主 provider。
    async fn summarize_for_compaction(&self, old: &[AgentMessage]) -> Result<String> {
        let mut input = crate::session::compaction::render_for_summary(old, 80_000);
        let pre_compact = self.hook_context(crate::config::HookEvent::PreCompact, None);
        if !pre_compact.is_empty() {
            input.push_str("\n\n[hook context]\n");
            input.push_str(&pre_compact.join("\n"));
        }
        let (provider, model) = match &self.config.compaction {
            Some(compaction) => (Arc::clone(&compaction.provider), compaction.model.clone()),
            None => {
                let slot = self.provider_snapshot();
                (slot.provider, slot.api_model)
            }
        };
        crate::session::compaction::summarize(&*provider, &model, &input, 4096)
            .await
            .map_err(|error| anyhow::anyhow!("compaction request failed: {error}"))
    }

    /// 压缩后追加在新历史末尾的上下文：PostCompact hook 的输出与记忆索引。
    fn post_compact_context(&self) -> Vec<AgentMessage> {
        let mut context: Vec<AgentMessage> = self
            .hook_context(crate::config::HookEvent::PostCompact, None)
            .into_iter()
            .map(|text| AgentMessage::System {
                text: format!("[hook context | PostCompact]\n{text}"),
            })
            .collect();
        // PostCompact recall：把记忆索引作为新分段里的上下文，避免压缩后
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
            context.push(AgentMessage::System {
                text: format!(
                    "<memory_context source=\"post_compact\">\n{text}\n</memory_context>"
                ),
            });
        }
        context
    }

    /// 在指定链上压缩。
    ///
    /// 调用方必须在回合锁之内（自动压缩在回合开头，`agent.compact` 是回合里的一次工具调用）：
    /// 复核和替换之间没有别的回合能改写或追加未复核的消息，摘要期间不占用历史锁。
    async fn compact_scope(
        &self,
        reason: &str,
        history: Arc<Mutex<Vec<AgentMessage>>>,
        sessions: Arc<SessionStore>,
    ) -> Result<serde_json::Value> {
        let _guard = self.compaction_lock.lock().await;
        let snapshot = history.lock().await.clone();
        let session_file = sessions.current().map(|current| current.file);
        if self.config.check_on_compaction {
            self.run_security_review(&snapshot).await;
            if self
                .discard_context
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Ok(serde_json::json!({
                    "compacted": false,
                    "reason": "the context will be discarded after a critical security review",
                }));
            }
        }
        let Some(prepared) = self
            .prepare_compaction(reason, snapshot, session_file)
            .await?
        else {
            return Ok(serde_json::json!({
                "compacted": false,
                "reason": "nothing older than the retention window",
            }));
        };
        let Some((result, _appended)) = self
            .install_compaction(prepared, &history, &sessions)
            .await?
        else {
            return Err(anyhow::anyhow!(
                "the history changed while it was being compacted; nothing was replaced"
            ));
        };
        Ok(result)
    }

    /// 准备摘要，不轮转分段、不替换历史。分段名随历史一起快照，不能在等待 hook 和记忆
    /// 提取之后再取：那时当前分段可能已经不是这份历史的来源。
    async fn prepare_compaction(
        &self,
        reason: &str,
        snapshot: Vec<AgentMessage>,
        session_file: Result<String>,
    ) -> Result<Option<PreparedCompaction>> {
        let (old, _) = crate::session::compaction::split_for_compaction(
            &snapshot,
            self.config.keep_recent_tokens as usize,
        );
        if old.is_empty() {
            return Ok(None);
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
        // 先完成 hook 与记忆提取，再处理取得分段名时的错误；保留压缩的副作用顺序。
        let session_file = session_file?;
        let summary = self.summarize_for_compaction(&old).await?;
        Ok(Some(PreparedCompaction {
            snapshot,
            cut: old.len(),
            session_file,
            summary,
            reason: reason.to_owned(),
        }))
    }

    /// 校验与替换共用同一把历史锁，确保校验过的快照前缀和追加的后缀正是写进新分段的来源。
    async fn install_compaction(
        &self,
        prepared: PreparedCompaction,
        history: &Mutex<Vec<AgentMessage>>,
        sessions: &SessionStore,
    ) -> Result<Option<(serde_json::Value, usize)>> {
        let PreparedCompaction {
            snapshot,
            cut,
            session_file,
            summary,
            reason,
        } = prepared;
        let mut current = history.lock().await;
        if !current.starts_with(&snapshot) || sessions.current()?.file != session_file {
            return Ok(None);
        }
        let suffix = &current[snapshot.len()..];
        let appended = suffix.len();
        let summary_message = AgentMessage::System {
            text: format!(
                "[checkpoint summary | previous_session=/sessions/{session_file} | reason={reason}]\n{summary}"
            ),
        };
        let mut new_history = vec![summary_message];
        // 保留窗口和后缀上的 `usage` 都描述压缩前的提示。留着它，估算会把已经不存在的
        // 提示当作基准，下次唤醒可能又压缩一次；除此之外，后缀保持原样和原顺序。
        new_history.extend(
            snapshot[cut..]
                .iter()
                .chain(suffix)
                .cloned()
                .map(|mut message| {
                    if let AgentMessage::Assistant(assistant) = &mut message {
                        assistant.usage = Default::default();
                    }
                    message
                }),
        );
        new_history.extend(self.post_compact_context());
        // 关闭旧分段、建新分段、写入新历史是一步（`rotate_with`）：失败时当前分段还是旧的那个，
        // 内存里的历史也没动，下次照常重试。
        let next = sessions.rotate_with(&new_history)?;
        let kept_messages = new_history.len() - 1;
        *current = new_history;
        drop(current);
        // 任何一次成功的压缩都把连续失败数清零，包括 agent 自己调用的 `agent.compact`：
        // 自动压缩停掉之后，agent 手动压缩成功一次就恢复，不必等人来重启进程。
        self.compaction_failures
            .store(0, std::sync::atomic::Ordering::Relaxed);
        // PostCompact：hooks 可能依赖刚写入的记忆/技能，重新读盘并重建。
        self.rebuild_system();
        let _ = self
            .fire_hooks(
                crate::config::HookEvent::PostCompact,
                None,
                Some(&next.file),
            )
            .await;

        Ok(Some((
            serde_json::json!({
                "compacted": true,
                "previous_session": session_file,
                "current_session": next.file,
                "summary_bytes": summary.len(),
                "kept_messages": kept_messages,
            }),
            appended,
        )))
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
