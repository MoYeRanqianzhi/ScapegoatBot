use super::*;

use crate::storage::Database;
use crate::vfs::Vfs;

/// `agent.*` 操作的运行时实现。
pub struct RuntimeOps {
    /// 压缩与上下文操作需要 runner；构造顺序决定了这里只能后置注入。
    pub runner: std::sync::OnceLock<Arc<Runner>>,
    pub gate: Arc<Gate>,
    pub vfs: Arc<VfsStore>,
    pub sessions: Arc<SessionStore>,
    pub messages: Arc<MessageStore>,
    pub db: Option<Arc<Database>>,
    pub history: Arc<Mutex<Vec<AgentMessage>>>,
    /// 当前模型引用；热切换时由 runtime 更新。
    pub model: Arc<std::sync::RwLock<String>>,
    pub owner_qq: u64,
    pub master_qq: u64,
    pub receive_scopes: Vec<String>,
    pub send_scopes: Vec<String>,
    pub estop_file: std::path::PathBuf,
}

#[async_trait::async_trait]
impl crate::tools::api::AgentOps for RuntimeOps {
    async fn status(&self) -> Result<serde_json::Value, String> {
        let usage = self.vfs.usage().map_err(|error| error.to_string())?;
        let events = self
            .db
            .as_ref()
            .and_then(|db| crate::security::events::recent(db, 5).ok())
            .unwrap_or_default();
        Ok(serde_json::json!({
            "hold": self.gate.hold_active(),
            "estop": self.estop_file.exists(),
            "model": *self.model.read().expect("model lock poisoned"),
            "owner_qq": self.owner_qq,
            "master_qq": self.master_qq,
            "receive_scopes": self.receive_scopes,
            "send_scopes": self.send_scopes,
            "vfs": {
                "active_bytes": usage.active_bytes,
                "trash_bytes": usage.trash_bytes,
                "nodes": usage.nodes,
            },
            "recent_security_events": events,
        }))
    }

    async fn resume(&self) -> Result<serde_json::Value, String> {
        if !self.gate.hold_active() {
            return Ok(serde_json::json!({"hold": false, "note": "not on hold"}));
        }
        // 复核真实内容：最近一次被判危险的增量 + 当前历史尾部 + hold 原因。
        // 旧实现只拿固定串 "resume request" 过闸，规则层看不到任何东西，
        // 于是任何 hold 都能被一次调用清掉。
        let mut judged = self.gate.flagged_content().unwrap_or_default();
        {
            let history = self.history.lock().await;
            let mut tail = String::new();
            for message in history.iter().rev() {
                let rendered = format!("{message:?}");
                if tail.len() + rendered.len() > 48_000 {
                    break;
                }
                tail.insert_str(0, &rendered);
                tail.insert(0, '\n');
            }
            judged.push_str(&tail);
        }
        if judged.trim().is_empty() {
            return Err(
                "The gate has no judged content to re-check; the hold stays in place.".to_owned(),
            );
        }
        let reason = self
            .gate
            .hold_reason()
            .unwrap_or_else(|| "security hold".to_owned());
        let verdict = self.gate.review(&reason, &judged).await;
        if verdict.dangerous {
            return Err(format!(
                "The gate still blocks this context: {}",
                verdict.reason
            ));
        }
        self.gate
            .clear_hold()
            .map_err(|error| format!("cannot clear the hold file: {error}"))?;
        Ok(serde_json::json!({
            "hold": false,
            "reason": verdict.reason,
        }))
    }

    async fn quarantine(&self, params: &serde_json::Value) -> Result<serde_json::Value, String> {
        let text = params
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "agent.quarantine requires \"text\"".to_owned())?;
        let reason = params
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("flagged by the agent");
        let name = format!(
            "/quarantine/{}-{}.md",
            chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ"),
            std::process::id()
        );
        let path = crate::vfs::VfsPath::parse(&name).map_err(|error| error.to_string())?;
        let body = format!("# Quarantined content\n\nreason: {reason}\n\n---\n\n{text}\n");
        self.vfs
            .write_text(&path, &body, None)
            .map_err(|error| error.to_string())?;
        Ok(serde_json::json!({"quarantined": path.to_string()}))
    }

    async fn compact(&self, chat: Option<ChatId>) -> Result<serde_json::Value, String> {
        let runner = self
            .runner
            .get()
            .ok_or_else(|| "the runner is not ready yet".to_owned())?;
        let chat = chat.unwrap_or(ChatId::Private(self.owner_qq));
        runner
            .compact_chat("manual", chat)
            .await
            .map_err(|error| error.to_string())
    }

    async fn clear_context(&self) -> Result<serde_json::Value, String> {
        let runner = self
            .runner
            .get()
            .ok_or_else(|| "the runner is not ready yet".to_owned())?;
        runner
            .clear_all_context()
            .await
            .map_err(|error| error.to_string())
    }
}
