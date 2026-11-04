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
use crate::config::{GateConfig, GateMode};
use crate::providers::{
    ContentBlock, LlmRequest, Provider, ProviderEvent, ProviderMessage, Role, SystemBlock,
};
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
/// 跨 chat 内容重叠的字符 n-gram 大小。
const NGRAM_SIZE: usize = 5;
/// 跨 chat 内容重叠的时间窗口。
const NGRAM_WINDOW: Duration = Duration::from_secs(600);
/// 只有足够长的内容才做 n-gram 比对，避免短句误伤。
const NGRAM_MIN_CHARS: usize = 24;
/// 共享 n-gram 数达到该值即视为搬运。
const NGRAM_SHARED: usize = 5;

/// 一次门禁结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionDecision {
    Allow,
    Block(String),
    /// `approve` 模式：已登记请求，等待主人确认。
    Pending {
        id: String,
        action: String,
    },
}

impl ActionDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// 主人在控制通道或主人私聊里对一次请求的确认记录。
#[derive(Debug, Clone)]
struct Approval {
    id: String,
    action: String,
    summary: String,
    /// 仅进程内保留，用于展示；不写数据库。
    params: String,
    params_hash: String,
    nonce: String,
    created_at: i64,
    approved_until: Option<i64>,
}

/// 待确认队列；可选数据库持久化（只存参数哈希与 nonce）。
#[derive(Default)]
pub struct ApprovalQueue {
    items: Mutex<HashMap<String, Approval>>,
    counter: AtomicU64,
    db: Option<Arc<Database>>,
}

impl ApprovalQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 带持久化：重启后未过期的 pending/approved 记录仍然可用。
    pub fn with_db(db: Arc<Database>) -> Arc<Self> {
        let queue = Arc::new(Self {
            db: Some(Arc::clone(&db)),
            ..Self::default()
        });
        queue.load_from_db();
        queue
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    fn load_from_db(&self) {
        let Some(db) = &self.db else {
            return;
        };
        let rows = match db.gate_approvals() {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "cannot load gate approvals");
                return;
            }
        };
        let now = Self::now();
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        for row in rows {
            let approved =
                row.status == "approved" && row.approved_until.is_some_and(|until| until > now);
            let pending =
                row.status == "pending" && now - row.created_at <= APPROVAL_TTL.as_secs() as i64;
            if approved || pending {
                items.insert(
                    row.id.clone(),
                    Approval {
                        id: row.id,
                        action: row.action,
                        summary: row.summary,
                        params: String::new(),
                        params_hash: row.params_hash,
                        nonce: row.nonce,
                        created_at: row.created_at,
                        approved_until: if approved { row.approved_until } else { None },
                    },
                );
            } else {
                let _ = db.delete_gate_approval(&row.id);
            }
        }
    }

    fn prune(&self, items: &mut HashMap<String, Approval>) {
        let now = Self::now();
        let ttl = APPROVAL_TTL.as_secs() as i64;
        let mut expired = Vec::new();
        items.retain(|id, approval| {
            let keep = match approval.approved_until {
                Some(until) => until > now,
                None => now - approval.created_at <= ttl,
            };
            if !keep {
                expired.push(id.clone());
            }
            keep
        });
        if let Some(db) = &self.db {
            for id in expired {
                let _ = db.delete_gate_approval(&id);
            }
        }
    }

    fn request(&self, action: &str, summary: &str, params: &Value) -> String {
        // 进程计数器重启会归零；带上毫秒时间戳，避免覆盖数据库里仍有效的旧请求。
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos())
            .unwrap_or(0);
        let id = format!(
            "g{}-{nanos}-{}",
            chrono::Utc::now().timestamp_millis(),
            self.counter.fetch_add(1, Ordering::Relaxed) + 1
        );
        let nonce = new_nonce(&id);
        let params_text = params.to_string();
        let params_hash = params_hash(&params_text);
        let created_at = Self::now();
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut items);
        let approval = Approval {
            id: id.clone(),
            action: action.to_owned(),
            summary: summary.to_owned(),
            params: params_text,
            params_hash: params_hash.clone(),
            nonce: nonce.clone(),
            created_at,
            approved_until: None,
        };
        if let Some(db) = &self.db {
            let row = crate::storage::GateApprovalRow {
                id: id.clone(),
                action: action.to_owned(),
                summary: summary.to_owned(),
                params_hash,
                nonce,
                status: "pending".to_owned(),
                created_at,
                approved_until: None,
            };
            if let Err(error) = db.save_gate_approval(&row) {
                tracing::warn!(%error, "cannot persist a gate approval request");
            }
        }
        items.insert(id.clone(), approval);
        id
    }

    fn consume(&self, action: &str, params: &Value) -> bool {
        let hash = params_hash(&params.to_string());
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut items);
        let now = Self::now();
        let found = items.values().find(|approval| {
            approval.action == action
                && approval.params_hash == hash
                && approval.approved_until.is_some_and(|until| until > now)
        });
        let Some(found) = found.map(|approval| approval.id.clone()) else {
            return false;
        };
        items.remove(&found);
        if let Some(db) = &self.db {
            let _ = db.delete_gate_approval(&found);
        }
        true
    }

    /// 某个待确认请求的 nonce（API 用它通知主人）。
    pub fn nonce_for(&self, id: &str) -> Option<String> {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut items);
        items
            .get(id)
            .filter(|approval| approval.approved_until.is_none())
            .map(|approval| approval.nonce.clone())
    }

    /// 主人私聊回复 nonce：批准对应请求并返回 id/action。
    pub fn approve_by_nonce(&self, text: &str) -> Option<Value> {
        let nonce = text.trim();
        if nonce.is_empty() {
            return None;
        }
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut items);
        let id = items
            .values()
            .find(|approval| approval.approved_until.is_none() && approval.nonce == nonce)
            .map(|approval| approval.id.clone())?;
        let approval = items.get_mut(&id)?;
        let until = Self::now() + APPROVED_WINDOW.as_secs() as i64;
        approval.approved_until = Some(until);
        if let Some(db) = &self.db
            && let Err(error) = db.save_gate_approval(&crate::storage::GateApprovalRow {
                id: approval.id.clone(),
                action: approval.action.clone(),
                summary: approval.summary.clone(),
                params_hash: approval.params_hash.clone(),
                nonce: approval.nonce.clone(),
                status: "approved".to_owned(),
                created_at: approval.created_at,
                approved_until: Some(until),
            })
        {
            tracing::warn!(%error, "cannot persist an approved gate request");
        }
        Some(json!({
            "approved": id,
            "action": approval.action,
            "window_secs": APPROVED_WINDOW.as_secs(),
        }))
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
                    "nonce": approval.nonce,
                    "wait_secs": (Self::now() - approval.created_at).max(0),
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
        let until = Self::now() + APPROVED_WINDOW.as_secs() as i64;
        approval.approved_until = Some(until);
        if let Some(db) = &self.db
            && let Err(error) = db.save_gate_approval(&crate::storage::GateApprovalRow {
                id: approval.id.clone(),
                action: approval.action.clone(),
                summary: approval.summary.clone(),
                params_hash: approval.params_hash.clone(),
                nonce: approval.nonce.clone(),
                status: "approved".to_owned(),
                created_at: approval.created_at,
                approved_until: Some(until),
            })
        {
            tracing::warn!(%error, "cannot persist an approved gate request");
        }
        Ok(json!({
            "approved": id,
            "action": approval.action,
            "window_secs": APPROVED_WINDOW.as_secs()
        }))
    }

    /// 拒绝一个请求。
    pub fn reject(&self, id: &str) -> Result<Value, String> {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut items);
        if items.remove(id).is_none() {
            return Err(format!("unknown or expired approval request {id:?}"));
        }
        if let Some(db) = &self.db {
            let _ = db.delete_gate_approval(id);
        }
        Ok(json!({"rejected": id}))
    }

    /// 当前待确认数量（`status` 用）。
    pub fn pending_count(&self) -> usize {
        self.pending().len()
    }
}

/// 参数哈希：只用于重启后匹配同一次调用，不落原文。
fn params_hash(params: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(params.as_bytes());
    hex::encode(digest)
}

/// 6 位 nonce；主人只在私聊里回复它。不是密码学令牌，但需要不可预测到
/// 足以防止同群成员猜中；输入包含时间与进程信息。
fn new_nonce(id: &str) -> String {
    use sha2::{Digest, Sha256};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let digest = Sha256::digest(format!("{id}:{nanos}:{}", std::process::id()).as_bytes());
    let value = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
    format!("{value:06}")
}

/// LLM 审查接口；由 runtime 注入（主 provider）。
#[async_trait::async_trait]
pub trait ActionReviewer: Send + Sync {
    /// `true` = 允许，`false` = 拒绝。错误由调用方按拒绝处理。
    async fn review(
        &self,
        action: &str,
        params: &Value,
        evidence: &TurnEvidence,
    ) -> Result<bool, String>;
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
            if tokio::time::Instant::now() >= deadline {
                return Err("review timed out".to_owned());
            }
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

/// 发送参数里的媒体引用（图片/文件/语音/视频）。
fn outgoing_media(params: &Value) -> Vec<String> {
    let Some(segments) = params.get("message").and_then(Value::as_array) else {
        return Vec::new();
    };
    segments
        .iter()
        .filter_map(|segment| {
            let kind = segment.get("type")?.as_str()?;
            if !matches!(kind, "image" | "file" | "record" | "video") {
                return None;
            }
            let data = segment.get("data")?;
            let origin = data
                .get("url")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .or_else(|| {
                    data.get("file")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                })?;
            Some(origin.to_owned())
        })
        .collect()
}

/// 归一化字符 n-gram（小写、空白折叠），用于跨 chat 内容重叠检测。
fn ngrams(text: &str) -> std::collections::HashSet<String> {
    let normalized: String = text
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let chars: Vec<char> = normalized.chars().collect();
    if chars.len() < NGRAM_SIZE {
        return std::collections::HashSet::new();
    }
    chars
        .windows(NGRAM_SIZE)
        .map(|window| window.iter().collect())
        .collect()
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
    /// 每 chat 每小时成功发送上限；0 = 不限制。
    chat_send_limit: u32,
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
            chat_send_limit: 0,
        }
    }

    /// 设置每 chat 每小时发送上限（`[budget].chat_sends_per_hour`）。
    /// 某个待确认请求的 nonce；API 用它通知主人。
    pub fn nonce_for(&self, id: &str) -> Option<String> {
        self.approvals.nonce_for(id)
    }

    pub fn with_chat_send_limit(mut self, limit: u32) -> Self {
        self.chat_send_limit = limit;
        self
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
        self.media_rule(params, evidence, chat)?;
        let content = message_content(params);
        if content.trim().is_empty() {
            return Ok(());
        }
        // 短应答（“好的”“哈哈”）重复是正常对话，不按重复处理。
        let content_short = content.chars().count() < 12;
        let mut sends = self.sends.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        sends.retain(|record| now.duration_since(record.at) <= Duration::from_secs(3600));
        if self.chat_send_limit > 0 {
            let count = sends
                .iter()
                .filter(|record| {
                    record.chat == chat
                        && now.duration_since(record.at) <= Duration::from_secs(3600)
                })
                .count();
            if count >= self.chat_send_limit as usize {
                return Err(format!(
                    "chat send limit of {} per hour reached for {chat}",
                    self.chat_send_limit
                ));
            }
        }
        if !content_short
            && sends.iter().any(|record| {
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
            .filter(|record| {
                record.content == content
                    && record.chat != chat
                    && now.duration_since(record.at) <= MASS_WINDOW
            })
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
        // 不是逐字相同但高度重叠（搬运/转发）时也要求 owner intent。
        if !evidence.owner_intent && content.chars().count() >= NGRAM_MIN_CHARS {
            let current = ngrams(&content);
            let overlapping = sends.iter().any(|record| {
                record.chat != chat
                    // 逐字相同交给 mass 规则（阈值 3 个 chat）；这里只抓“改写后搬运”。
                    && record.content != content
                    && now.duration_since(record.at) <= NGRAM_WINDOW
                    && record.content.chars().count() >= NGRAM_MIN_CHARS
                    && current.intersection(&ngrams(&record.content)).count() >= NGRAM_SHARED
            });
            if overlapping {
                return Err(
                    "content overlaps a message sent to another chat within 10 minutes;                      forwarding requires owner intent"
                        .to_owned(),
                );
            }
        }
        Ok(())
    }

    /// 媒体来源校验：第一次出现在别的 chat 的图片/文件不能无证据转发。
    fn media_rule(
        &self,
        params: &Value,
        evidence: &TurnEvidence,
        chat: &str,
    ) -> Result<(), String> {
        if evidence.owner_intent {
            return Ok(());
        }
        let Some(db) = &self.db else {
            return Ok(());
        };
        for origin in outgoing_media(params) {
            if let Ok(Some(source)) = db.media_origin(&origin)
                && source != chat
            {
                return Err(format!(
                    "media was first seen in {source}; forwarding it to {chat} requires                      owner intent"
                ));
            }
        }
        Ok(())
    }

    /// 记录一次**成功**的发送；失败/超时不应触发重复与跨群规则，
    /// 否则一次临时错误会让重试被自己的记录挡住。
    pub fn note_send(&self, params: &Value, chat: &str) {
        let content = message_content(params);
        if content.trim().is_empty() {
            return;
        }
        let mut sends = self.sends.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        sends.retain(|record| now.duration_since(record.at) <= MASS_WINDOW);
        sends.push_back(SendRecord {
            at: now,
            chat: chat.to_owned(),
            content,
        });
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
                gate.check(
                    spec("napcat.send_group_msg"),
                    &params,
                    &TurnEvidence::default()
                )
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
            gate.check(
                spec("napcat.get_group_list"),
                &query,
                &TurnEvidence::default()
            )
            .await
            .is_allowed()
        );
        assert!(
            gate.check(
                spec("napcat.get_group_list"),
                &query,
                &TurnEvidence::default()
            )
            .await
            .is_allowed()
        );
        assert!(matches!(
            gate.check(
                spec("napcat.get_group_list"),
                &query,
                &TurnEvidence::default()
            )
            .await,
            ActionDecision::Block(_)
        ));
    }

    #[tokio::test]
    async fn duplicate_and_mass_sends_are_blocked_without_owner_intent() {
        let gate = auto_gate();
        let evidence = TurnEvidence::default();
        let message = json!({"group_id": 1, "message": "same long content for three groups"});
        assert!(
            gate.check(spec("napcat.send_group_msg"), &message, &evidence)
                .await
                .is_allowed()
        );
        gate.note_send(&message, "group:1");

        let duplicate = json!({"group_id": 1, "message": "same long content for three groups"});
        assert!(matches!(
            gate.check(spec("napcat.send_group_msg"), &duplicate, &evidence)
                .await,
            ActionDecision::Block(_)
        ));

        // 短应答重复不拦。
        let short = json!({"group_id": 1, "message": "好的"});
        assert!(
            gate.check(spec("napcat.send_group_msg"), &short, &evidence)
                .await
                .is_allowed()
        );
        gate.note_send(&short, "group:1");
        assert!(
            gate.check(spec("napcat.send_group_msg"), &short, &evidence)
                .await
                .is_allowed()
        );

        for group in [2, 3] {
            let params = json!({"group_id": group, "message": "broadcast text across groups"});
            assert!(
                gate.check(spec("napcat.send_group_msg"), &params, &evidence)
                    .await
                    .is_allowed()
            );
            gate.note_send(&params, &format!("group:{group}"));
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
            gate.check(
                spec("napcat.send_group_msg"),
                &params,
                &TurnEvidence::owner("control")
            )
            .await
            .is_allowed()
        );
    }

    #[tokio::test]
    async fn approvals_persist_and_match_by_nonce() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let config = GateConfig {
            mode: GateMode::Approve,
            max_per_minute: 60,
            llm_review: false,
        };
        let first = ApprovalQueue::with_db(Arc::clone(&db));
        let gate = ActionGate::new(&config, Arc::clone(&first));
        let params = json!({"group_id": 1, "message": "hello"});
        let decision = gate
            .check(
                spec("napcat.send_group_msg"),
                &params,
                &TurnEvidence::default(),
            )
            .await;
        let ActionDecision::Pending { id, .. } = decision else {
            panic!("expected pending");
        };
        let nonce = first.pending()[0]["nonce"].as_str().unwrap().to_owned();
        assert!(first.approve_by_nonce(&nonce).is_some());

        // 重启：只有参数哈希与 nonce 落库，已批准记录仍可消费一次。
        drop(gate);
        drop(first);
        let reloaded = ApprovalQueue::with_db(Arc::clone(&db));
        let gate = ActionGate::new(&config, Arc::clone(&reloaded));
        assert!(matches!(
            gate.check(
                spec("napcat.send_group_msg"),
                &params,
                &TurnEvidence::default()
            )
            .await,
            ActionDecision::Allow
        ));
        // 第二次消费失败，重新回到 pending。
        assert!(matches!(
            gate.check(
                spec("napcat.send_group_msg"),
                &params,
                &TurnEvidence::default()
            )
            .await,
            ActionDecision::Pending { .. }
        ));
        assert_eq!(reloaded.pending().len(), 1);
        assert_ne!(reloaded.pending()[0]["id"], id);
    }

    #[tokio::test]
    async fn per_chat_hourly_send_limit_blocks() {
        let gate = auto_gate().with_chat_send_limit(2);
        let evidence = TurnEvidence::default();
        for text in ["first long message", "second long message"] {
            let params = json!({"group_id": 1, "message": text});
            assert!(
                gate.check(spec("napcat.send_group_msg"), &params, &evidence)
                    .await
                    .is_allowed()
            );
            gate.note_send(&params, "group:1");
        }
        let params = json!({"group_id": 1, "message": "third long message"});
        assert!(matches!(
            gate.check(spec("napcat.send_group_msg"), &params, &evidence)
                .await,
            ActionDecision::Block(reason) if reason.contains("send limit")
        ));
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
            gate.check(spec("napcat.send_group_msg"), &params, &evidence)
                .await,
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
            gate.check(spec("napcat.send_group_msg"), &params, &evidence)
                .await,
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

        let gate =
            ActionGate::new(&config, ApprovalQueue::new()).with_reviewer(Arc::new(Rejecting));
        assert!(matches!(
            gate.check(spec("napcat.send_group_msg"), &params, &TurnEvidence::default())
                .await,
            ActionDecision::Block(reason) if reason.contains("reviewer blocked")
        ));
    }

    #[tokio::test]
    async fn overlapping_content_across_chats_requires_owner_intent() {
        let gate = auto_gate();
        let original = "今天下午三点在会议室讨论一下新版本的上线计划和回滚方案，记得带上文档";
        let params = json!({"group_id": 1, "message": original});
        assert!(
            gate.check(
                spec("napcat.send_group_msg"),
                &params,
                &TurnEvidence::default()
            )
            .await
            .is_allowed()
        );
        gate.note_send(&params, "group:1");

        // 共享多个 5-gram、但不是逐字相同。
        let forwarded = "今天下午三点在会议室讨论一下新版本的上线计划，以及回滚方案，记得带上文档";
        let params = json!({"group_id": 2, "message": forwarded});
        assert!(matches!(
            gate.check(spec("napcat.send_group_msg"), &params, &TurnEvidence::default())
                .await,
            ActionDecision::Block(reason) if reason.contains("overlaps")
        ));

        let owner = TurnEvidence::owner("control");
        assert!(
            gate.check(spec("napcat.send_group_msg"), &params, &owner)
                .await
                .is_allowed()
        );
    }

    #[tokio::test]
    async fn forwarding_media_from_another_chat_requires_owner_intent() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        db.record_media_origin("https://example.invalid/a.png", "group:1")
            .unwrap();
        let gate = auto_gate().with_db(db);
        let params = json!({
            "group_id": 2,
            "message": [{"type": "image", "data": {"url": "https://example.invalid/a.png"}}]
        });
        assert!(matches!(
            gate.check(spec("napcat.send_group_msg"), &params, &TurnEvidence::default())
                .await,
            ActionDecision::Block(reason) if reason.contains("first seen")
        ));
        assert!(
            gate.check(
                spec("napcat.send_group_msg"),
                &params,
                &TurnEvidence::owner("control")
            )
            .await
            .is_allowed()
        );
        // 发回来源 chat 不需要额外证据。
        let same = json!({
            "group_id": 1,
            "message": [{"type": "image", "data": {"url": "https://example.invalid/a.png"}}]
        });
        assert!(
            gate.check(
                spec("napcat.send_group_msg"),
                &same,
                &TurnEvidence::default()
            )
            .await
            .is_allowed()
        );
    }
}
