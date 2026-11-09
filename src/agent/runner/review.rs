use super::*;

impl Runner {
    /// 累计新增上下文 token；到达阈值后在回合边界触发检查。
    pub(super) fn accumulate_review_tokens(&self, tokens: usize) {
        if self.config.security_review.is_none() || self.config.check_every_tokens == 0 {
            return;
        }
        self.unchecked_tokens
            .fetch_add(tokens as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// 回合边界：达到阈值就同步检查一次。
    pub(super) async fn maybe_security_review(&self, history: &Arc<Mutex<Vec<AgentMessage>>>) {
        if self.config.security_review.is_none() || self.config.check_every_tokens == 0 {
            return;
        }
        if self
            .unchecked_tokens
            .load(std::sync::atomic::Ordering::Relaxed)
            < self.config.check_every_tokens
        {
            return;
        }
        let _guard = self.review_lock.lock().await;
        // 拿到锁后重新确认：并发回合可能已经检查过。
        if self
            .unchecked_tokens
            .load(std::sync::atomic::Ordering::Relaxed)
            < self.config.check_every_tokens
        {
            return;
        }
        let snapshot = history.lock().await.clone();
        self.run_security_review(&snapshot).await;
    }

    /// 用独立模型检查窗口；只写安全事件，不写 transcript。
    pub(super) async fn run_security_review(&self, history: &[AgentMessage]) {
        let Some(checker) = &self.config.security_review else {
            return;
        };
        // 主 system 前缀相同，provider 可以命中 prompt cache。
        checker.set_system(self.system());
        let window = render_review_window(history, self.config.check_overlap_tokens);
        match checker.review(&window).await {
            Ok(outcome) => {
                self.review_failures
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                self.unchecked_tokens
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                let findings = outcome.findings.join("; ");
                let reason = format!(
                    "security review: {} risk, complied={}{}",
                    outcome.risk.as_str(),
                    outcome.complied,
                    if findings.is_empty() {
                        String::new()
                    } else {
                        format!("; {findings}")
                    }
                );
                let score = match outcome.risk {
                    crate::security::review::Risk::Low => 0.1,
                    crate::security::review::Risk::Medium => 0.4,
                    crate::security::review::Risk::High => 0.8,
                    crate::security::review::Risk::Critical => 1.0,
                };
                let blocked = matches!(
                    outcome.risk,
                    crate::security::review::Risk::High | crate::security::review::Risk::Critical
                ) && (outcome.complied
                    || outcome.risk == crate::security::review::Risk::Critical);
                self.deps.gate.record_review(!blocked, score, &reason);
                match outcome.risk {
                    crate::security::review::Risk::Low => debug!(%reason, "security review"),
                    crate::security::review::Risk::Medium => {
                        warn!(%reason, "security review raised medium risk")
                    }
                    crate::security::review::Risk::High if outcome.complied => {
                        warn!(%reason, "security review: injection appears to have worked");
                        self.deps.gate.force_hold(&reason);
                    }
                    crate::security::review::Risk::High => {
                        warn!(%reason, "security review raised high risk")
                    }
                    crate::security::review::Risk::Critical => {
                        // 以前这里“暂停所有新回合”，只有宿主机上的控制通道能解开；
                        // 这个 agent 无人值守，那等于永久停机。现在：hold 关掉 QQ 出口，
                        // 被污染的上下文在下一回合开始前整体丢弃（旧分段留档）。agent
                        // 醒来时上下文是干净的，自己复核后用 `agent.resume` 解除。
                        warn!(%reason, "security review: critical; holding and discarding the context");
                        self.deps.gate.force_hold(&reason);
                        self.discard_context
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            }
            Err(error) => {
                let failures = self
                    .review_failures
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                // 检查超时/失败按放行处理（设计 §5.9）；连续失败只告警。
                self.unchecked_tokens
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                warn!(failures, %error, "security review failed; outbound stays allowed");
            }
        }
    }
}

/// 渲染周期检查窗口：从最新消息向前，直到字符预算。
fn render_review_window(history: &[AgentMessage], overlap_tokens: u64) -> String {
    let budget = 48_000usize.saturating_add((overlap_tokens as usize).saturating_mul(4));
    let mut out = String::new();
    for message in history.iter().rev() {
        let rendered = crate::agent::agent_loop::render_message(message);
        if out.len() + rendered.len() + 1 > budget {
            break;
        }
        out.insert_str(0, &rendered);
        out.insert(0, '\n');
    }
    out
}

impl Runner {
    /// 回合开始前调用：`critical` 检查留下的“丢弃上下文”标记在这里执行。
    ///
    /// 放在回合锁之内、构建工作副本之前，保证本回合看到的已经是新分段。
    pub(super) async fn discard_context_if_flagged(&self) {
        if !self
            .discard_context
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        match self.clear_all_context().await {
            Ok(result) => warn!(
                previous = %result["previous_session"],
                "discarded the context after a critical security review"
            ),
            Err(error) => {
                // 清理失败就保留标记，下一回合再试；hold 仍然关着 QQ 出口。
                warn!(%error, "cannot discard the context after a critical security review");
                self.discard_context
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}
