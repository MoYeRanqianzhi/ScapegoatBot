//! `[gate]`：每次 `napcat.*` 调用的门禁。
//!
//! 设计依据：设计 §5.6。
//!
//! 与输入/输出闸的分工：那两个闸看“内容危不危险”，这里看“这次调用该不该发生”。
//!
//! - `trusted`：直接放行，只保留速率统计；
//! - `auto`（默认）：速率限制 + 发送规则（重复、跨群同文、@全体）；
//!   `llm_review = true` 时对变更类动作追加一次 LLM 审查，失败按拒绝处理；
//! - `approve`：变更类动作需要主人确认，确认走控制通道（`sbot gate pending` /
//!   `approve` / `reject`），普通聊天回复不构成确认；60 秒未确认自动拒绝，
//!   批准后 120 秒内可消费一次。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::agent::TurnEvidence;
use crate::config::{GateMode, GateConfig};
use crate::providers::{ContentBlock, LlmRequest, Provider, ProviderEvent, ProviderMessage, Role, SystemBlock};
use crate::storage::Database;
use crate::tools::actions::{ActionSpec, Category};

use super::events::{self, Checkpoint};

/// 待确认请求的存活时间。
const APPROVAL_TTL: Duration = Duration::from_secs(60);
/// 批准后的一次性放行窗口。
const APPROVED_WINDOW: Duration = Duration::from_secs(120);
/// 速率窗口。
const RATE_WINDOW: Duration = Duration::from_secs(60);
/// 同内容跨群判定的时间窗口。
const MASS_WINDOW: Duration = Duration::from_secs(120);
/// 同内容发给同一个 chat 的重复窗口。
const DUPLICATE_WINDOW: Duration = Duration::from_secs(30);
/// 触发跨群判定的最少不同 chat 数。
const MASS_TARGETS: usize = 3;

/// 一次门禁结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionDecision {
    Allow,
    Block(String),
    /// `approve` 模式：已登记请求，等待主人确认。
    Pending { id: String, action: String },
}

impl ActionDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// 主人在控制通道里对一次请求的确认记录。
#[derive(Debug, Clone)]
struct Approval {
    id: String,
    action: String,
    summary: String,
    params: String,
    created: Instant,
    approved_until: Option<Instant>,
}

/// 待确认队列（进程内）。
#[derive(Default)]
pub struct ApprovalQueue {
    items: Mutex<HashMap<String, Approval>>,
    counter: AtomicU64,
}

impl ApprovalQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn prune(&self, items: &mut HashMap<String, Approval>) {
        let now = Instant::now();
        items.retain(|_, approval| {
            if let Some(until) = approval.approved_until {
                until > now
            } else {
                now.duration_since(approval.created) <= APPROVAL_TTL
            }
        });
    }

    fn request(&self, action: &str, summary: &str, params: &Value) -> String {
        let id = format!("g{}", self.counter.fetch_add(1, Ordering::Relaxed) + 1);
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut items);
        items.insert(
            id.clone(),
            Approval {
                id: id.clone(),
                action: action.to_owned(),
                summary: summary.to_owned(),
                params: params.to_string(),
                created: Instant::now(),
                approved_until: None,
            },
        );
        id
    }

    fn consume(&self, action: &str, params: &Value) -> bool {
        let canonical = params.to_string();
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut items);
        let now = Instant::now();
        let found = items.values().find(|approval| {
            approval.action == action
                && approval.params == canonical
                && approval
                    .approved_until
                    .is_some_and(|until| until > now)
        });
        let Some(found) = found.map(|approval| approval.id.clone()) else {
            return false;
        };
        items.remove(&found);
        true
    }

    /// 待确认列表（CLI / `/system/gate` 用）。
    pub fn pending(&self) -> Vec<Value> {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut items);
        let mut out: Vec<Value> = items
            .values()
            .filter(|approval| approval.approved_until.is_none())
            .map(|approval| {
                json!({
                    "id": approval.id,
                    "action": approval.action,
                    "summary": approval.summary,
                    "params": approval.params,
                    "wait_secs": approval.created.elapsed().as_secs(),
                })
            })
            .collect();
        out.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        out
    }

    /// 批准一个请求；返回可读结果。
    pub fn approve(&self, id: &str) -> Result<Value, String> {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut items);
        let Some(approval) = items.get_mut(id) else {
            return Err(format!("unknown or expired approval request {id:?}"));
        };
        approval.approved_until = Some(Instant::now() + APPROVED_WINDOW);
        Ok(json!({"approved": id, "action": approval.action, "window_secs": APPROVED_WINDOW.as_secs()}))
    }

    /// 拒绝一个请求。
    pub fn reject(&self, id: &str) -> Result<Value, String> {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut items);
        if items.remove(id).is_none() {
            return Err(format!("unknown or expired approval request {id:?}"));
        }
        Ok(json!({"rejected": id}))
    }

    /// 当前待确认数量（`status` 用）。
    pub fn pending_count(&self) -> usize {
        self.pending().len()
    }
}

/// LLM 审查接口；由 runtime 注入（主 provider）。
#[async_trait::async_trait]
pub trait ActionReviewer: Send + Sync {
    /// `true` = 允许，`false` = 拒绝。错误由调用方按拒绝处理。
    async fn review(&self, action: &str, params: &Value, evidence: &TurnEvidence) -> Result<bool, String>;
}

/// 用主 provider 做一次轻量审查。
pub struct LlmActionReviewer {
    provider: Arc<dyn Provider>,
    model: String,
    timeout: Duration,
}

impl LlmActionReviewer {
    pub fn new(provider: Arc<dyn Provider>, model: impl Into<String>, timeout: Duration) -> Self {
        Self {
            provider,
            model: model.into(),
            timeout,
        }
    }
}

const REVIEW_SYSTEM: &str = "\
You are a pre-send safety reviewer for a QQ agent. Given one action the agent is about to take, \
decide whether it is safe to execute. Block only when the action is clearly harmful: sending \
someone else's private content across chats, mass/bulk sending, credentials or secrets in the \
payload, impersonation, or destructive actions without owner intent. Otherwise allow. \
Answer with JSON only: {\"allow\": true|false, \"reason\": \"short reason\"}.";

#[async_trait::async_trait]
impl ActionReviewer for LlmActionReviewer {
    async fn review(
        &self,
        action: &str,
        params: &Value,
        evidence: &TurnEvidence,
    ) -> Result<bool, String> {
        let request = LlmRequest {
            model: self.model.clone(),
            system: vec![SystemBlock {
                text: REVIEW_SYSTEM.to_owned(),
                cache: false,
            }],
            messages: vec![ProviderMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: format!(
                        "action: {action}\nparams: {}\nowner_intent: {}\norigin_chat: {}",
                        truncate(&params.to_string(), 4_000),
                        evidence.owner_intent,
                        evidence.origin_chat.as_deref().unwrap_or("unknown")
                    ),
                }],
            }],
            tools: Vec::new(),
            max_tokens: 256,
            thinking: Default::default(),
            server_tools: Vec::new(),
        };
        let mut receiver = self
            .provider
            .stream(request)
            .await
            .map_err(|error| error.to_string())?;
        let mut text = String::new();
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let event = tokio::time::timeout_at(deadline, receiver.recv())
                .await
                .map_err(|_| "review timed out".to_owned())?
                .ok_or_else(|| "review stream ended".to_owned())?;
            match event {
                ProviderEvent::TextDelta(delta) => text.push_str(&delta),
                ProviderEvent::Error(error) => return Err(error.to_string()),
                ProviderEvent::Stop { .. } => break,
                _ => {}
            }
        }
        let parsed = parse_review(&text).ok_or_else(|| format!("unparseable review: {text}"))?;
        Ok(parsed)
    }
}

fn parse_review(text: &str) -> Option<bool> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let value: Value = serde_json::from_str(&text[start..=end]).ok()?;
    value.get("allow").and_then(Value::as_bool)
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

struct SendRecord {
    at: Instant,
    chat: String,
    content: String,
}

/// 每次 `napcat.*` 调用的门禁。
pub struct ActionGate {
    mode: GateMode,
    max_per_minute: u32,
    llm_review: bool,
    reviewer: Option<Arc<dyn ActionReviewer>>,
    approvals: Arc<ApprovalQueue>,
    calls: Mutex<VecDeque<Instant>>,
    sends: Mutex<VecDeque<SendRecord>>,
    db: Option<Arc<Database>>,
}

impl ActionGate {
    pub fn new(config: &GateConfig, approvals: Arc<ApprovalQueue>) -> Self {
        Self {
            mode: config.mode,
            max_per_minute: config.max_per_minute.max(1),
            llm_review: config.llm_review,
            reviewer: None,
            approvals,
            calls: Mutex::new(VecDeque::new()),
            sends: Mutex::new(VecDeque::new()),
            db: None,
        }
    }

    pub fn with_reviewer(mut self, reviewer: Arc<dyn ActionReviewer>) -> Self {
        self.reviewer = Some(reviewer);
        self
    }

    pub fn with_db(mut self, db: Arc<Database>) -> Self {
        self.db = Some(db);
        self
    }

    pub fn approvals(&self) -> &Arc<ApprovalQueue> {
        &self.approvals
    }

    /// 当前模式（`/system/gate` 用）。
    pub fn mode(&self) -> GateMode {
        self.mode
    }

    pub fn llm_review(&self) -> bool {
        self.llm_review
    }

    fn record_audit(&self, action: &str, allowed: bool, reason: &str) {
        if let Some(db) = &self.db
            && let Err(error) = events::record(
                db,
                Checkpoint::Gate,
                allowed,
                0.0,
                "gate",
                &format!("{action}: {reason}"),
            )
        {
            tracing::warn!(%error, "cannot record the gate event");
        }
    }

    /// 速率限制：所有 `napcat.*` 调用共用每分钟预算。
    fn rate_ok(&self) -> bool {
        let mut calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        while calls
            .front()
            .is_some_and(|at| now.duration_since(*at) > RATE_WINDOW)
        {
            calls.pop_front();
        }
        if calls.len() >= self.max_per_minute as usize {
            return false;
        }
        calls.push_back(now);
        true
    }

    /// 发送规则：同 chat 重复、跨群同文、@全体。
    fn send_rule(&self, params: &Value, evidence: &TurnEvidence, chat: &str) -> Result<(), String> {
        if contains_at_all(params) && !evidence.owner_intent {
            return Err("message mentions @all; owner intent is required".to_owned());
        }
        let content = message_content(params);
        if content.trim().is_empty() {
            return Ok(());
        }
        let mut sends = self.sends.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        sends.retain(|record| now.duration_since(record.at) <= MASS_WINDOW);
        if sends
            .iter()
            .any(|record| {
                record.chat == chat
                    && record.content == content
                    && now.duration_since(record.at) <= DUPLICATE_WINDOW
            })
        {
            return Err(
                "the same message was already sent to this chat within 30s; do not repeat it"
                    .to_owned(),
            );
        }
        let mut targets: Vec<&str> = sends
            .iter()
            .filter(|record| record.content == content && record.chat != chat)
            .map(|record| record.chat.as_str())
            .collect();
        targets.push(chat);
        targets.sort_unstable();
        targets.dedup();
        if targets.len() >= MASS_TARGETS && !evidence.owner_intent {
            return Err(format!(
                "the same content is being sent to {} different chats within 2 minutes; \\
                 mass sending requires owner intent",
                targets.len()
            ));
        }
        sends.push_back(SendRecord {
            at: now,
            chat: chat.to_owned(),
            content,
        });
        Ok(())
    }

    /// 执行一次门禁判定。
    pub async fn check(
        &self,
        spec: &ActionSpec,
        params: &Value,
        evidence: &TurnEvidence,
    ) -> ActionDecision {
        if self.mode == GateMode::Trusted || spec.namespace() != "napcat" {
            return ActionDecision::Allow;
        }
        if !self.rate_ok() {
            let reason = format!("rate limit of {} calls/min exceeded", self.max_per_minute);
            self.record_audit(spec.action, false, &reason);
            return ActionDecision::Block(reason);
        }

        let mutation = matches!(
            spec.category,
            Category::Message
                | Category::Request
                | Category::Admin
                | Category::File
                | Category::Profile
        );

        if self.mode == GateMode::Approve && mutation {
            if self.approvals.consume(spec.action, params) {
                self.record_audit(spec.action, true, "approved by the owner");
                return ActionDecision::Allow;
            }
            let id = self.approvals.request(spec.action, spec.summary, params);
            let reason = format!(
                "approval_required: request {id} is pending. The owner can run \
                 `sbot gate approve {id}` (or reject it); then retry this action."
            );
            self.record_audit(spec.action, false, &reason);
            return ActionDecision::Pending {
                id,
                action: spec.action.to_owned(),
            };
        }

        if spec.category == Category::Message
            && let Some(chat) = crate::tools::api::ApiTool::chat_of(spec, params)
            && let Err(reason) = self.send_rule(params, evidence, &chat.to_string())
        {
            self.record_audit(spec.action, false, &reason);
            return ActionDecision::Block(reason);
        }

        if self.llm_review && mutation {
            let Some(reviewer) = &self.reviewer else {
                let reason = "llm_review is enabled but no reviewer is configured".to_owned();
                self.record_audit(spec.action, false, &reason);
                return ActionDecision::Block(reason);
            };
            match reviewer.review(spec.action, params, evidence).await {
                Ok(true) => {}
                Ok(false) => {
                    let reason = format!("the LLM reviewer blocked {}", spec.action);
                    self.record_audit(spec.action, false, &reason);
                    return ActionDecision::Block(reason);
                }
                Err(error) => {
                    let reason = format!("the LLM reviewer failed: {error}");
                    self.record_audit(spec.action, false, &reason);
                    return ActionDecision::Block(reason);
                }
            }
        }

        self.record_audit(spec.action, true, "allowed");
        ActionDecision::Allow
    }
}

fn contains_at_all(params: &Value) -> bool {
    params
        .get("message")
        .and_then(Value::as_array)
        .is_some_and(|segments| {
            segments.iter().any(|segment| {
                segment.get("type").and_then(Value::as_str) == Some("at")
                    && segment
                        .get("data")
                        .and_then(|data| data.get("qq"))
                        .and_then(Value::as_str)
                        == Some("all")
            })
        })
}

fn message_content(params: &Value) -> String {
    match params.get("message") {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::actions::specs;

    fn spec(action: &str) -> &'static ActionSpec {
        specs()
            .iter()
            .find(|spec| spec.action == action)
            .expect("action exists")
    }

    fn auto_gate() -> ActionGate {
        ActionGate::new(
            &GateConfig {
                mode: GateMode::Auto,
                max_per_minute: 60,
                llm_review: false,
            },
            ApprovalQueue::new(),
        )
    }

    #[tokio::test]
    async fn trusted_allows_everything() {
        let gate = ActionGate::new(
            &GateConfig {
                mode: GateMode::Trusted,
                max_per_minute: 1,
                llm_review: true,
            },
            ApprovalQueue::new(),
        );
        let params = json!({"group_id": 1, "message": "hi"});
        for _ in 0..5 {
            assert!(
                gate.check(spec("napcat.send_group_msg"), &params, &TurnEvidence::default())
                    .await
                    .is_allowed()
            );
        }
    }

    #[tokio::test]
    async fn rate_limit_blocks_after_the_budget() {
        let gate = ActionGate::new(
            &GateConfig {
                mode: GateMode::Auto,
                max_per_minute: 2,
                llm_review: false,
            },
            ApprovalQueue::new(),
        );
        let query = json!({"group_id": 1});
        assert!(
            gate.check(spec("napcat.get_group_list"), &query, &TurnEvidence::default())
                .await
                .is_allowed()
        );
        assert!(
            gate.check(spec("napcat.get_group_list"), &query, &TurnEvidence::default())
                .await
                .is_allowed()
        );
        assert!(matches!(
            gate.check(spec("napcat.get_group_list"), &query, &TurnEvidence::default())
                .await,
            ActionDecision::Block(_)
        ));
    }

    #[tokio::test]
    async fn duplicate_and_mass_sends_are_blocked_without_owner_intent() {
        let gate = auto_gate();
        let evidence = TurnEvidence::default();
        let message = json!({"group_id": 1, "message": "same long content for three groups"});
        assert!(gate.check(spec("napcat.send_group_msg"), &message, &evidence).await.is_allowed());

        let duplicate = json!({"group_id": 1, "message": "same long content for three groups"});
        assert!(matches!(
            gate.check(spec("napcat.send_group_msg"), &duplicate, &evidence).await,
            ActionDecision::Block(_)
        ));

        for group in [2, 3] {
            let params = json!({"group_id": group, "message": "broadcast text across groups"});
            let _ = gate.check(spec("napcat.send_group_msg"), &params, &evidence).await;
        }
        let params = json!({"group_id": 4, "message": "broadcast text across groups"});
        assert!(matches!(
            gate.check(spec("napcat.send_group_msg"), &params, &evidence).await,
            ActionDecision::Block(reason) if reason.contains("mass sending")
        ));
    }

    #[tokio::test]
    async fn at_all_requires_owner_intent() {
        let gate = auto_gate();
        let params = json!({
            "group_id": 1,
            "message": [{"type": "at", "data": {"qq": "all"}}, {"type": "text", "data": {"text": " hi"}}]
        });
        assert!(matches!(
            gate.check(spec("napcat.send_group_msg"), &params, &TurnEvidence::default()).await,
            ActionDecision::Block(reason) if reason.contains("@all")
        ));
        assert!(
            gate.check(spec("napcat.send_group_msg"), &params, &TurnEvidence::owner("control"))
                .await
                .is_allowed()
        );
    }

    #[tokio::test]
    async fn approve_mode_requires_a_one_shot_confirmation() {
        let approvals = ApprovalQueue::new();
        let gate = ActionGate::new(
            &GateConfig {
                mode: GateMode::Approve,
                max_per_minute: 60,
                llm_review: false,
            },
            Arc::clone(&approvals),
        );
        let params = json!({"group_id": 1, "message": "hello"});
        let evidence = TurnEvidence::owner("control");
        let ActionDecision::Pending { id, .. } = gate
            .check(spec("napcat.send_group_msg"), &params, &evidence)
            .await
        else {
            panic!("expected pending");
        };
        assert!(matches!(
            gate.check(spec("napcat.send_group_msg"), &params, &evidence).await,
            ActionDecision::Pending { .. }
        ));
        approvals.approve(&id).unwrap();
        assert!(
            gate.check(spec("napcat.send_group_msg"), &params, &evidence)
                .await
                .is_allowed()
        );
        // 一次性：同一批准不能再次放行。
        assert!(matches!(
            gate.check(spec("napcat.send_group_msg"), &params, &evidence).await,
            ActionDecision::Pending { .. }
        ));
    }

    struct Rejecting;

    #[async_trait::async_trait]
    impl ActionReviewer for Rejecting {
        async fn review(
            &self,
            _action: &str,
            _params: &Value,
            _evidence: &TurnEvidence,
        ) -> Result<bool, String> {
            Ok(false)
        }
    }

    #[tokio::test]
    async fn llm_review_blocks_and_missing_reviewer_fails_closed() {
        let config = GateConfig {
            mode: GateMode::Auto,
            max_per_minute: 60,
            llm_review: true,
        };
        let gate = ActionGate::new(&config, ApprovalQueue::new());
        let params = json!({"group_id": 1, "message": "hi"});
        assert!(matches!(
            gate.check(spec("napcat.send_group_msg"), &params, &TurnEvidence::default())
                .await,
            ActionDecision::Block(reason) if reason.contains("no reviewer")
        ));

        let gate = ActionGate::new(&config, ApprovalQueue::new()).with_reviewer(Arc::new(Rejecting));
        assert!(matches!(
            gate.check(spec("napcat.send_group_msg"), &params, &TurnEvidence::default())
                .await,
            ActionDecision::Block(reason) if reason.contains("reviewer blocked")
        ));
    }
}
