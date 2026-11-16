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

/// 周期检查窗口的基本长度（字符）；再加上 `check_overlap_tokens` 折算的重叠部分。
const REVIEW_WINDOW_CHARS: usize = 48_000;
/// 窗口长度的上限（字符）：`check_overlap_tokens` 没有上限，窗口不能跟着它无限变长——
/// 超出审查模型的上下文窗口时请求失败，而审查失败是按放行处理的。
const REVIEW_WINDOW_MAX_CHARS: usize = 200_000;

/// 渲染周期检查窗口：最近的消息按时间顺序排列，总长不超过字符预算。
///
/// 永远保留最新的内容：从最新一条往回取，放不下的那一条只留它的结尾。最新一条本身就
/// 超过预算时（一个很长的工具结果）窗口里是它的结尾，而不是空的——空窗口等于没有审查。
fn render_review_window(history: &[AgentMessage], overlap_tokens: u64) -> String {
    let budget = REVIEW_WINDOW_CHARS
        .saturating_add((overlap_tokens as usize).saturating_mul(4))
        .min(REVIEW_WINDOW_MAX_CHARS);
    let mut left = budget;
    // 先按“最新在前”收集，最后倒过来。
    let mut parts: Vec<String> = Vec::new();
    for message in history.iter().rev() {
        let rendered = crate::agent::agent_loop::render_message(message);
        let length = rendered.chars().count();
        if length <= left {
            left -= length;
            parts.push(rendered);
            continue;
        }
        if left > 0 {
            let tail: String = rendered.chars().skip(length - left).collect();
            parts.push(format!("…{tail}"));
        }
        break;
    }
    parts.reverse();
    parts.concat()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 历史比预算长时，窗口里是最新的那一段，按时间顺序；最旧的被丢掉。
    #[test]
    fn the_review_window_keeps_the_newest_messages() {
        // 每条约 1000 个字符，共 100 条：远超默认预算。
        let history: Vec<AgentMessage> = (0..100)
            .map(|index| AgentMessage::user(format!("message-{index:03} {}", "x".repeat(985))))
            .collect();
        let window = render_review_window(&history, 4_096);
        let length = window.chars().count();
        // 被截断的那一条前面多一个省略号，不计入预算。
        assert!(
            length <= REVIEW_WINDOW_CHARS + 4_096 * 4 + 1,
            "window is {length} characters"
        );
        // 预算基本用满，没有因为一条放不下就提前停住。
        assert!(
            length > REVIEW_WINDOW_CHARS,
            "window is {length} characters"
        );
        assert!(window.contains("message-099"), "the newest message is kept");
        assert!(!window.contains("message-000"), "the oldest is dropped");
        let newest = window.find("message-099").unwrap();
        let older = window.find("message-090").unwrap();
        assert!(older < newest, "messages stay in chronological order");
    }

    /// 最新一条自己就超过预算：窗口里是它的结尾，而不是空的。
    #[test]
    fn an_oversized_newest_message_is_reviewed_by_its_tail() {
        let history = [
            AgentMessage::user("earlier"),
            AgentMessage::user(format!("{}THE-END", "y".repeat(300_000))),
        ];
        let window = render_review_window(&history, 0);
        assert_eq!(window.chars().count(), REVIEW_WINDOW_CHARS + 1);
        assert!(window.starts_with('…'));
        assert!(window.trim_end().ends_with("THE-END"));
        assert!(!window.contains("earlier"));
    }

    /// 重叠再大，窗口也不超过上限。
    #[test]
    fn the_review_window_is_capped() {
        let history = [AgentMessage::user("z".repeat(1_000_000))];
        let window = render_review_window(&history, u64::MAX);
        assert_eq!(window.chars().count(), REVIEW_WINDOW_MAX_CHARS + 1);
    }

    /// 多字节字符按字符计数：中文内容不会被按字节多算而少看一大半。
    #[test]
    fn the_review_window_counts_characters() {
        let history = [AgentMessage::user("好".repeat(40_000))];
        let window = render_review_window(&history, 0);
        assert_eq!(window.matches('好').count(), 40_000);
    }
}
