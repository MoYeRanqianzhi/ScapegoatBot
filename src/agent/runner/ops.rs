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
            // 只给原因，不回显被判危险的原文：再把它放进上下文，复核只会再次命中。
            "hold_reason": self.gate.hold_reason(),
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
        // 复核的是“接下来要据以行动的上下文”：已提交历史的尾部。
        //
        // 不把当初被判危险的那段原文本身拿来复核——它永远是危险的，拿它复核等于
        // hold 永远解不开；而这个 agent 无人值守，没有人会来替它解除。
        // - 输出闸命中：危险输出从未进入历史，上下文干净即可恢复；
        // - 输入闸命中：危险输入已在历史里，复核会再次命中，agent 必须先用
        //   `agent.quarantine` 把它移出上下文（或 `agent.clear_context`）。
        let judged = {
            let history = self.history.lock().await;
            let mut tail = String::new();
            for message in history.iter().rev() {
                let rendered = crate::agent::agent_loop::render_message(message);
                if tail.len() + rendered.len() > 48_000 {
                    break;
                }
                tail.insert_str(0, &rendered);
            }
            tail
        };
        if judged.trim().is_empty() {
            // 空上下文没有任何可以继续出问题的东西。
            self.gate
                .clear_hold()
                .map_err(|error| format!("cannot clear the hold file: {error}"))?;
            return Ok(serde_json::json!({"hold": false, "reason": "context is empty"}));
        }
        let reason = self
            .gate
            .hold_reason()
            .unwrap_or_else(|| "security hold".to_owned());
        let verdict = self.gate.review(&reason, &judged).await;
        if verdict.dangerous {
            return Err(format!(
                "The hold stays: your current context still looks unsafe ({}). Find the \
                 offending excerpt, move it out with agent.quarantine (pass its exact text), \
                 or start over with agent.clear_context; then call agent.resume again.",
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
        // 把同一段原文从已提交的上下文里换成占位符：下一回合与 `agent.resume`
        // 看到的就是清理后的上下文。只改内存里的历史；磁盘上的 transcript 分段
        // 保持原样，作为审计记录（重启后会重新读入，必要时需再隔离一次）。
        let placeholder = format!("[quarantined: {path}]");
        let removed: usize = {
            let mut history = self.history.lock().await;
            history
                .iter_mut()
                .map(|message| message.redact(text, &placeholder))
                .sum()
        };
        Ok(serde_json::json!({
            "quarantined": path.to_string(),
            "removed_from_context": removed,
        }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EncryptionMode, GuardBackend, GuardConfig, GuardMode};
    use crate::storage::crypto::MasterKey;
    use crate::tools::api::AgentOps;
    use crate::vfs::quota::Limits;

    /// 只含 resume/quarantine 需要的部件；runner 留空（compact/clear_context 不在这里测）。
    fn ops(dir: &std::path::Path, history: Vec<AgentMessage>) -> RuntimeOps {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([4u8; 32]));
        let data = dir.join("data");
        let vfs = Arc::new(
            VfsStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                data.clone(),
                Limits::default(),
            )
            .unwrap(),
        );
        let messages = Arc::new(
            MessageStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                data.clone(),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let sessions = Arc::new(
            SessionStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                data,
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let gate = Arc::new(Gate::from_config(
            &GuardConfig {
                enabled: true,
                mode: GuardMode::Enforce,
                backend: GuardBackend::Off,
                threshold: 0.9,
                ..GuardConfig::default()
            },
            None,
            None,
            crate::security::gate::hold_path(dir),
        ));
        RuntimeOps {
            runner: std::sync::OnceLock::new(),
            gate,
            vfs,
            sessions,
            messages,
            db: Some(db),
            history: Arc::new(Mutex::new(history)),
            model: Arc::new(std::sync::RwLock::new("fake".to_owned())),
            owner_qq: 10000,
            master_qq: 10000,
            receive_scopes: vec!["*".to_owned()],
            send_scopes: vec!["*".to_owned()],
            estop_file: dir.join("ESTOP"),
        }
    }

    /// 无人值守：输出闸命中后，危险输出从未进入上下文，agent 自己就能解除 hold。
    #[tokio::test]
    async fn the_agent_can_lift_an_output_hold_by_itself() {
        let dir = tempfile::tempdir().unwrap();
        let ops = ops(dir.path(), vec![AgentMessage::user("hi")]);
        assert!(
            !ops.gate
                .check(
                    crate::security::events::Checkpoint::Output,
                    "",
                    "token sk-abcdefghijklmnopqrstuvwx"
                )
                .await
                .allowed
        );
        assert!(ops.gate.hold_active());
        let status = ops.status().await.unwrap();
        assert!(status["hold_reason"].is_string());

        assert_eq!(ops.resume().await.unwrap()["hold"], false);
        assert!(!ops.gate.hold_active());
    }

    /// 输入闸命中：可疑内容已在上下文里，必须先隔离，复核才会通过。
    #[tokio::test]
    async fn a_poisoned_context_must_be_quarantined_before_resuming() {
        let dir = tempfile::tempdir().unwrap();
        let poison = "token sk-abcdefghijklmnopqrstuvwx";
        let ops = ops(
            dir.path(),
            vec![AgentMessage::user(format!("look: {poison}"))],
        );
        ops.gate
            .check(crate::security::events::Checkpoint::Input, "", poison)
            .await;
        assert!(ops.gate.hold_active());

        let refused = ops.resume().await.unwrap_err();
        assert!(refused.contains("agent.quarantine"), "{refused}");
        assert!(ops.gate.hold_active());

        let quarantined = ops
            .quarantine(&serde_json::json!({"text": poison, "reason": "credential"}))
            .await
            .unwrap();
        assert_eq!(quarantined["removed_from_context"], 1);
        let path =
            crate::vfs::VfsPath::parse(quarantined["quarantined"].as_str().unwrap()).unwrap();
        assert!(
            ops.vfs
                .read_text_unrecorded(&path)
                .unwrap()
                .contains(poison)
        );

        assert_eq!(ops.resume().await.unwrap()["hold"], false);
        assert!(!ops.gate.hold_active());
    }
}
