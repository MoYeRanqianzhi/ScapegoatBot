//! 回合执行器：把队列输入接到 agent loop、transcript 与工具上。
//!
//! 设计依据：设计 §3.4 / §4 / §8.1。
//!
//! 并发模型（共享 transcript）：
//! - 入站消息由回合开始时立即追加到 transcript，并进入共享历史；
//! - 每个回合在**本地副本**上运行，避免别的回合的未提交块插进工具调用之间；
//! - 回合结束把新块写入 `turn_buffer`，再一次性提交到 transcript，最后并入共享历史。
//!
//! runtime context 每回合单独注入，不写进 transcript；它承载时间、origin_chat、
//! `current_session`、收件箱摘要等易变字段。

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, warn};

use crate::agent::queue::{TurnHandler, TurnInput};
use crate::agent::{AgentEvent, AgentLoop, AgentMessage, LoopConfig, LoopOutcome, SteeringQueue};
use crate::config::{AgentMode, GateMode};
use crate::prompt::{self, RuntimeContext};
use crate::providers::{Provider, SystemBlock, ThinkingOptions};
use crate::qq::event::ChatId;
use crate::qq::{QqClient, format};
use crate::security::gate::Gate;
use crate::session::SessionStore;
use crate::session::turn_buffer::{TurnBufferStore, new_turn_id};
use crate::storage::Database;
use crate::storage::messages::MessageStore;
use crate::tools::ToolRegistry;
use crate::vfs::Vfs;
use crate::vfs::sqlite::VfsStore;

/// 运行配置。
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub model: String,
    pub mode: AgentMode,
    pub account_qq: u64,
    pub owner_qq: u64,
    pub master_qq: u64,
    pub owner_name: String,
    pub session_scope: String,
    pub receive_scopes: Vec<String>,
    pub send_scopes: Vec<String>,
    pub gate_mode: GateMode,
    pub max_tokens: u32,
    pub thinking: ThinkingOptions,
    pub max_steps: u32,
    pub retry: crate::agent::agent_loop::RetryPolicy,
    /// 注入“上次回复以来的群消息”时最多取多少条。
    pub chat_history_limit: u32,
    /// context hooks（AGENTS/USER/MEMORY/STYLE 或自定义）。
    pub hooks: Vec<crate::config::ContextHookConfig>,
}

/// 回合执行器。
pub struct Runner {
    provider: Arc<dyn Provider>,
    /// `sbot ai stop` 的开关；关闭时不发起新的模型请求。
    ai_enabled: Arc<std::sync::atomic::AtomicBool>,
    /// `ESTOP` 哨兵路径；存在时暂停新回合。
    estop_file: std::path::PathBuf,
    tools: Arc<ToolRegistry>,
    gate: Arc<Gate>,
    vfs: Arc<VfsStore>,
    messages: Arc<MessageStore>,
    sessions: Arc<SessionStore>,
    turn_buffer: Arc<TurnBufferStore>,
    qq: Option<Arc<QqClient>>,
    config: RunnerConfig,
    /// 已提交的全局 transcript（内存镜像）。
    history: Arc<Mutex<Vec<AgentMessage>>>,
    system: Vec<SystemBlock>,
}

impl Runner {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<ToolRegistry>,
        gate: Arc<Gate>,
        vfs: Arc<VfsStore>,
        messages: Arc<MessageStore>,
        sessions: Arc<SessionStore>,
        turn_buffer: Arc<TurnBufferStore>,
        qq: Option<Arc<QqClient>>,
        config: RunnerConfig,
        history: Arc<Mutex<Vec<AgentMessage>>>,
        ai_enabled: Arc<std::sync::atomic::AtomicBool>,
        estop_file: std::path::PathBuf,
    ) -> Result<Arc<Self>> {
        // 恢复崩溃前未提交的回合，再载入当前分段。
        let recovered = turn_buffer.recover_open(&sessions).await?;
        if recovered > 0 {
            warn!(
                recovered,
                "recovered interrupted turns from the turn buffer"
            );
        }
        let current = sessions.current()?;
        let recovered_history = sessions.read_segment(&current.file)?;
        {
            let mut shared = history.lock().await;
            // 只在此处初始化一次；重复构造会重置内存历史。
            if shared.is_empty() {
                *shared = recovered_history;
            }
        }

        let dynamic = prompt::load_hooks(&vfs, &config.hooks);

        let mut system = vec![SystemBlock {
            text: prompt::render_static(config.mode),
            cache: true,
        }];
        for block in dynamic.render() {
            system.push(SystemBlock {
                text: block,
                cache: false,
            });
        }

        Ok(Arc::new(Self {
            provider,
            ai_enabled,
            estop_file,
            tools,
            gate,
            vfs,
            messages,
            sessions,
            turn_buffer,
            qq,
            config,
            history,
            system,
        }))
    }

    /// 共享历史句柄（`agent.*` 操作使用）。
    pub fn history(&self) -> Arc<Mutex<Vec<AgentMessage>>> {
        Arc::clone(&self.history)
    }

    /// 当前 system 块快照。
    pub fn system(&self) -> &[SystemBlock] {
        &self.system
    }

    /// 组装运行上下文。
    fn runtime_context(&self, chat: ChatId) -> RuntimeContext {
        let inbox = self
            .messages
            .unread_counts()
            .map(|counts| {
                counts
                    .iter()
                    .map(|(chat, count)| format!("{chat}: {count}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let session = self
            .sessions
            .current_path()
            .unwrap_or_else(|_| "/sessions/unknown.jsonl".to_owned());
        RuntimeContext {
            mode: match self.config.mode {
                AgentMode::StandIn => "stand_in".to_owned(),
                AgentMode::Companion => "companion".to_owned(),
            },
            session_scope: self.config.session_scope.clone(),
            account_qq: self.config.account_qq,
            owner_name: self.config.owner_name.clone(),
            origin_chat: chat.to_string(),
            owner_qq: self.config.owner_qq,
            master_qq: self.config.master_qq,
            current_time: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            model: self.config.model.clone(),
            thinking_level: self.config.thinking.clone().effort.unwrap_or_default(),
            current_session: session,
            receive_scopes: self.config.receive_scopes.clone(),
            send_scopes: self.config.send_scopes.clone(),
            group_roles: String::new(),
            gate_mode: match self.config.gate_mode {
                GateMode::Trusted => "trusted",
                GateMode::Auto => "auto",
                GateMode::Approve => "approve",
            }
            .to_owned(),
            inbox_summary: if inbox.is_empty() {
                "nothing unread".to_owned()
            } else {
                inbox
            },
        }
    }

    /// 控制通道输入：`sbot chat` 注入的一句话。
    ///
    /// 作为 `<owner_context source="control:<id>">` 注入并立即跑一个回合；
    /// 不依赖任何 QQ chat，因此 `origin_chat` 标为 `control`。
    pub async fn run_control(&self, text: &str, control_id: i64) -> Result<LoopOutcome> {
        if !self.ai_enabled.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("AI calls are stopped; run `sbot ai start` first");
        }
        let message = AgentMessage::user(format!(
            "<owner_context source=\"control:{control_id}\">
{text}
</owner_context>"
        ));
        {
            let mut history = self.history.lock().await;
            history.push(message.clone());
        }
        self.sessions.append(&[message])?;

        let mut working = { self.history.lock().await.clone() };
        let base = working.len();
        let runtime = RuntimeContext {
            origin_chat: "control".to_owned(),
            ..self.runtime_context(ChatId::Private(self.config.owner_qq))
        };
        let prompts = vec![AgentMessage::user(runtime.render())];

        let loop_ = AgentLoop::new(
            Arc::clone(&self.provider),
            Arc::clone(&self.tools) as Arc<dyn crate::agent::ToolExecutor>,
            Arc::clone(&self.gate),
            LoopConfig {
                model: self.config.model.clone(),
                system: self.system.clone(),
                max_tokens: self.config.max_tokens,
                thinking: self.config.thinking.clone(),
                server_tools: Vec::new(),
                max_steps: self.config.max_steps,
                retry: self.config.retry,
                max_state_chars: 32_000,
            },
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

        let new_blocks: Vec<AgentMessage> = working[base..].to_vec();
        if !new_blocks.is_empty() {
            let turn_id = new_turn_id();
            for (seq, message) in new_blocks.iter().enumerate() {
                self.turn_buffer.append(&turn_id, seq as u64, message)?;
            }
            self.turn_buffer.commit(&self.sessions, &turn_id).await?;
            self.history.lock().await.extend(new_blocks);
        }
        Ok(outcome)
    }

    /// “上次回复以来的群消息”：提供上下文，但不触发回合。
    fn pending_history(&self, chat: ChatId, first_batch_id: Option<i64>) -> Option<String> {
        let recent = self
            .messages
            .recent(chat, self.config.chat_history_limit)
            .ok()?;
        let last_self = recent.iter().rposition(|message| message.is_self);
        let start = last_self.map(|index| index + 1).unwrap_or(0);
        let mut lines = Vec::new();
        for message in recent.iter().skip(start) {
            if message.is_self {
                continue;
            }
            if first_batch_id.is_some_and(|first| message.message_id >= first) {
                continue;
            }
            lines.push(format::render_inbound(message));
        }
        if lines.is_empty() {
            None
        } else {
            Some(format!(
                "[chat history since your last reply]\n{}",
                lines.join("\n")
            ))
        }
    }

    /// 跑一个完整回合。
    async fn execute(
        &self,
        input: TurnInput,
        mut steering_rx: mpsc::UnboundedReceiver<AgentMessage>,
    ) -> Result<()> {
        // `sbot ai stop` 与 `ESTOP` 都只暂停新回合；消息已经入库，下一回合可见。
        if !self.ai_enabled.load(std::sync::atomic::Ordering::Relaxed) {
            debug!(chat = %input.chat, "ai is stopped; skipping this turn");
            return Ok(());
        }
        if self.estop_file.exists() {
            debug!(chat = %input.chat, "ESTOP is active; skipping this turn");
            return Ok(());
        }
        let chat = input.chat;
        let turn_id = new_turn_id();
        let first_batch_id = input.messages.first().map(|message| message.message_id);

        // 1) 入站消息立即进入 transcript 与共享历史。
        let mut inbound = Vec::with_capacity(input.messages.len());
        for message in &input.messages {
            let text = format::render_inbound(message);
            let agent_message = AgentMessage::user(text);
            inbound.push(agent_message.clone());
            self.sessions.append(&[agent_message])?;
            if let Err(error) =
                self.messages
                    .set_cursor(chat, message.message_id, &message.time.to_rfc3339())
            {
                warn!(%error, "cannot update chat cursor");
            }
        }

        // 2) 取历史副本；后续新块只写本地副本。
        let mut working = {
            let mut history = self.history.lock().await;
            for message in &inbound {
                history.push(message.clone());
            }
            history.clone()
        };
        let base = working.len();

        // 3) 组装本回合的即时提示。
        let mut prompts = vec![AgentMessage::user(self.runtime_context(chat).render())];
        if let Some(pending) = self.pending_history(chat, first_batch_id) {
            prompts.push(AgentMessage::user(pending));
        }
        if input.dropped > 0 {
            prompts.push(AgentMessage::user(format!(
                "Note: {} earlier message(s) from this chat were dropped because the queue \
                 exceeded its limits. They were not read and cannot be recovered from context.",
                input.dropped
            )));
        }

        // 4) 运行循环。
        let loop_ = AgentLoop::new(
            Arc::clone(&self.provider),
            Arc::clone(&self.tools) as Arc<dyn crate::agent::ToolExecutor>,
            Arc::clone(&self.gate),
            LoopConfig {
                model: self.config.model.clone(),
                system: self.system.clone(),
                max_tokens: self.config.max_tokens,
                thinking: self.config.thinking.clone(),
                server_tools: Vec::new(),
                max_steps: self.config.max_steps,
                retry: self.config.retry,
                max_state_chars: 32_000,
            },
        );
        let (events_tx, mut events_rx) = mpsc::channel(256);
        let logger = tokio::spawn(async move {
            while let Some(event) = events_rx.recv().await {
                match event {
                    AgentEvent::Security {
                        checkpoint,
                        allowed: false,
                        reason,
                    } => warn!(?checkpoint, %reason, "security gate blocked this turn"),
                    AgentEvent::Error { message } => warn!(%message, "agent error"),
                    _ => {}
                }
            }
        });
        let mut steering = SteeringQueue::default();
        let outcome = loop_
            .run_with_steering(
                &mut working,
                prompts,
                &mut steering,
                Some(&mut steering_rx),
                &events_tx,
            )
            .await;
        drop(events_tx);
        let _ = logger.await;

        // 5) 提交新块：buffer -> transcript -> 共享历史。
        let new_blocks: Vec<AgentMessage> = working[base..].to_vec();
        if !new_blocks.is_empty() {
            for (seq, message) in new_blocks.iter().enumerate() {
                self.turn_buffer.append(&turn_id, seq as u64, message)?;
            }
            self.turn_buffer.commit(&self.sessions, &turn_id).await?;
            let mut history = self.history.lock().await;
            history.extend(new_blocks);
        }

        // 6) 已读标记（模拟真人；失败不影响回合结果）。
        if let Some(qq) = &self.qq {
            let _ = qq.mark_read(chat).await;
        }

        debug!(
            chat = %chat,
            stop = ?outcome.stop_reason,
            turns = outcome.turns,
            "turn completed"
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl TurnHandler for Runner {
    async fn handle(
        &self,
        input: TurnInput,
        steering: mpsc::UnboundedReceiver<AgentMessage>,
    ) -> Result<()> {
        self.execute(input, steering).await
    }
}

/// 回合结果摘要（供 CLI/日志使用）。
#[allow(dead_code)]
pub fn summarize(outcome: &LoopOutcome) -> String {
    format!(
        "stop={:?} turns={} tokens={}",
        outcome.stop_reason,
        outcome.turns,
        outcome.usage.input_tokens + outcome.usage.output_tokens
    )
}

/// `agent.*` 操作的运行时实现。
pub struct RuntimeOps {
    pub gate: Arc<Gate>,
    pub vfs: Arc<VfsStore>,
    pub sessions: Arc<SessionStore>,
    pub messages: Arc<MessageStore>,
    pub db: Option<Arc<Database>>,
    pub history: Arc<Mutex<Vec<AgentMessage>>>,
    pub model: String,
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
            "model": self.model,
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
        let state = {
            let history = self.history.lock().await;
            history
                .iter()
                .rev()
                .take(12)
                .map(|message| format!("{message:?}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let decision = self
            .gate
            .check(crate::security::Checkpoint::Input, &state, "resume request")
            .await;
        if decision.allowed {
            self.gate
                .clear_hold()
                .map_err(|error| format!("cannot clear the hold file: {error}"))?;
            Ok(serde_json::json!({"hold": false, "reason": decision.reason}))
        } else {
            Err(format!(
                "The gate still blocks this context: {}",
                decision.reason
            ))
        }
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

    async fn compact(&self) -> Result<serde_json::Value, String> {
        Err("compaction is not available in this build".to_owned())
    }

    async fn clear_context(&self) -> Result<serde_json::Value, String> {
        let current = self.sessions.current().map_err(|error| error.to_string())?;
        let next = self.sessions.rotate().map_err(|error| error.to_string())?;
        self.history.lock().await.clear();
        Ok(serde_json::json!({
            "cleared": true,
            "previous_session": current.file,
            "current_session": next.file,
        }))
    }
}

/// 从消息存储构造待注入的运行上下文（供其他模块复用）。
#[allow(dead_code)]
pub fn inbox_summary(messages: &MessageStore) -> String {
    messages
        .unread_counts()
        .map(|counts| {
            if counts.is_empty() {
                "nothing unread".to_owned()
            } else {
                counts
                    .iter()
                    .map(|(chat, count)| format!("{chat}: {count}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        })
        .unwrap_or_else(|_| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicBool;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::agent::{StopReason, Usage};
    use crate::config::{
        AgentMode, EncryptionMode, GateMode, GuardBackend, GuardConfig, GuardMode,
    };
    use crate::providers::{EventReceiver, LlmRequest, Provider, ProviderError, ProviderEvent};
    use crate::qq::event::{Segment, Sender};
    use crate::storage::crypto::MasterKey;
    use crate::tools::api::{AgentOps, ApiTool, NapcatGateway};
    use crate::vfs::quota::Limits;

    struct FakeProvider {
        script: StdMutex<VecDeque<Vec<ProviderEvent>>>,
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn protocol(&self) -> crate::config::models::Protocol {
            crate::config::models::Protocol::Chat
        }

        fn model_id(&self) -> &str {
            "fake"
        }

        async fn stream(&self, _request: LlmRequest) -> Result<EventReceiver, ProviderError> {
            let events = self.script.lock().unwrap().pop_front().unwrap_or_else(|| {
                vec![ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                    refusal: None,
                }]
            });
            let (tx, rx) = mpsc::channel(64);
            tokio::spawn(async move {
                for event in events {
                    let _ = tx.send(event).await;
                }
            });
            Ok(rx)
        }
    }

    #[derive(Default)]
    struct FakeGateway {
        calls: StdMutex<Vec<(String, serde_json::Value)>>,
    }

    #[async_trait]
    impl NapcatGateway for FakeGateway {
        async fn call(
            &self,
            action: &str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.calls.lock().unwrap().push((action.to_owned(), params));
            Ok(json!({"status":"ok","retcode":0,"data":{"message_id":777}}))
        }
    }

    struct NoOps;

    #[async_trait]
    impl AgentOps for NoOps {
        async fn status(&self) -> Result<serde_json::Value, String> {
            Ok(json!({}))
        }
        async fn resume(&self) -> Result<serde_json::Value, String> {
            Ok(json!({}))
        }
        async fn quarantine(
            &self,
            _params: &serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({}))
        }
        async fn compact(&self) -> Result<serde_json::Value, String> {
            Ok(json!({}))
        }
        async fn clear_context(&self) -> Result<serde_json::Value, String> {
            Ok(json!({}))
        }
    }

    fn inbound(chat: ChatId, id: i64, text: &str) -> crate::qq::event::QqMessage {
        crate::qq::event::QqMessage {
            message_id: id,
            chat,
            sender: Sender {
                qq: Some(10001),
                nickname: "Alice".to_owned(),
                card: String::new(),
                role: "member".to_owned(),
            },
            time: chrono::Utc::now(),
            segments: vec![Segment::Text {
                text: text.to_owned(),
            }],
            reply_to: None,
            is_self: false,
            backfilled: false,
            raw: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn turn_runs_tool_and_records_self_message() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([8u8; 32]));
        let vfs = Arc::new(
            VfsStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                Limits::default(),
            )
            .unwrap(),
        );
        let messages = Arc::new(
            MessageStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let sessions = Arc::new(
            SessionStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let turn_buffer = Arc::new(TurnBufferStore::new(
            Arc::clone(&db),
            Arc::clone(&key),
            EncryptionMode::XChaCha20Poly1305,
        ));
        let gate = Arc::new(Gate::from_config(
            &GuardConfig {
                enabled: true,
                mode: GuardMode::Shadow,
                backend: GuardBackend::Off,
                threshold: 0.9,
                ..GuardConfig::default()
            },
            None,
            None,
            crate::security::gate::hold_path(dir.path()),
        ));

        let gateway = Arc::new(FakeGateway::default());
        let mut registry = crate::tools::ToolRegistry::new(Arc::clone(&vfs), None);
        crate::tools::fs::register(&mut registry);
        registry.register(Arc::new(ApiTool::new(
            Some(Arc::clone(&gateway) as Arc<dyn NapcatGateway>),
            Some(Arc::new(NoOps)),
            999,
            999,
            Some(Arc::clone(&gate)),
            vec!["*".to_owned()],
            Some(Arc::clone(&messages)),
        )));
        let tools = Arc::new(registry);

        let script = vec![
            vec![
                ProviderEvent::ToolCallStart {
                    index: 0,
                    id: "c1".to_owned(),
                    name: "API".to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    index: 0,
                    delta: json!({
                        "action": "napcat.send_group_msg",
                        "params": {"group_id": 1, "message": "hello"}
                    })
                    .to_string(),
                },
                ProviderEvent::Stop {
                    reason: StopReason::ToolUse,
                    refusal: None,
                },
            ],
            vec![
                ProviderEvent::TextDelta("done".to_owned()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                    refusal: None,
                },
            ],
        ];
        let provider = Arc::new(FakeProvider {
            script: StdMutex::new(script.into()),
        });

        let history = Arc::new(Mutex::new(Vec::new()));
        let runner = Runner::new(
            provider,
            tools,
            Arc::clone(&gate),
            Arc::clone(&vfs),
            Arc::clone(&messages),
            Arc::clone(&sessions),
            Arc::clone(&turn_buffer),
            None,
            RunnerConfig {
                model: "fake".to_owned(),
                mode: AgentMode::StandIn,
                account_qq: 10000,
                owner_qq: 10000,
                master_qq: 10000,
                owner_name: "Owner".to_owned(),
                session_scope: "shared".to_owned(),
                receive_scopes: vec!["*".to_owned()],
                send_scopes: vec!["*".to_owned()],
                gate_mode: GateMode::Auto,
                max_tokens: 256,
                thinking: ThinkingOptions::default(),
                max_steps: 8,
                retry: crate::agent::agent_loop::RetryPolicy::default(),
                chat_history_limit: 10,
                hooks: crate::config::HooksConfig::default().context,
            },
            Arc::clone(&history),
            Arc::new(AtomicBool::new(true)),
            dir.path().join("ESTOP"),
        )
        .await
        .unwrap();

        let chat = ChatId::Group(1);
        // 生产路径里消息先由事件循环落库；这里复现同一步骤。
        let incoming = inbound(chat, 42, "hi");
        messages.append(&incoming).unwrap();
        let (_tx, rx) = mpsc::unbounded_channel();
        runner
            .handle(
                TurnInput {
                    chat,
                    messages: vec![incoming],
                    dropped: 0,
                },
                rx,
            )
            .await
            .unwrap();

        // 工具真的调用了网关。锁必须在一个块里释放，后面还有 await。
        {
            let calls = gateway.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, "send_group_msg");
        }

        // 发送被记录为 self。
        let recent = messages.recent(chat, 10).unwrap();
        assert!(
            recent
                .iter()
                .any(|message| message.is_self && message.plain_text() == "hello")
        );
        // 入站消息也在。
        assert!(
            recent
                .iter()
                .any(|message| !message.is_self && message.plain_text() == "hi")
        );

        // transcript 里有 assistant 与 tool 块。
        let current = sessions.current().unwrap();
        let transcript = sessions.read_segment(&current.file).unwrap();
        assert!(
            transcript
                .iter()
                .any(|message| matches!(message, AgentMessage::Assistant(_)))
        );
        assert!(
            transcript
                .iter()
                .any(|message| matches!(message, AgentMessage::Tool(_)))
        );
        // 共享历史与 transcript 一致。
        let history = history.lock().await;
        assert_eq!(history.len(), transcript.len());
    }

    #[tokio::test]
    async fn stopped_ai_skips_the_turn() {
        // 只验证开关分支：没有 provider 调用时不会 panic。
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([8u8; 32]));
        let vfs = Arc::new(
            VfsStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                Limits::default(),
            )
            .unwrap(),
        );
        let messages = Arc::new(
            MessageStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let sessions = Arc::new(
            SessionStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let turn_buffer = Arc::new(TurnBufferStore::new(
            Arc::clone(&db),
            Arc::clone(&key),
            EncryptionMode::XChaCha20Poly1305,
        ));
        let gate = Arc::new(Gate::from_config(
            &GuardConfig {
                enabled: true,
                mode: GuardMode::Shadow,
                backend: GuardBackend::Off,
                threshold: 0.9,
                ..GuardConfig::default()
            },
            None,
            None,
            crate::security::gate::hold_path(dir.path()),
        ));
        let tools = Arc::new(crate::tools::ToolRegistry::new(Arc::clone(&vfs), None));
        let provider = Arc::new(FakeProvider {
            script: StdMutex::new(VecDeque::new()),
        });
        let runner = Runner::new(
            provider,
            tools,
            gate,
            vfs,
            Arc::clone(&messages),
            sessions,
            turn_buffer,
            None,
            RunnerConfig {
                model: "fake".to_owned(),
                mode: AgentMode::StandIn,
                account_qq: 10000,
                owner_qq: 10000,
                master_qq: 10000,
                owner_name: String::new(),
                session_scope: "shared".to_owned(),
                receive_scopes: vec!["*".to_owned()],
                send_scopes: vec!["*".to_owned()],
                gate_mode: GateMode::Auto,
                max_tokens: 256,
                thinking: ThinkingOptions::default(),
                max_steps: 4,
                retry: crate::agent::agent_loop::RetryPolicy::default(),
                chat_history_limit: 10,
                hooks: crate::config::HooksConfig::default().context,
            },
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(AtomicBool::new(false)),
            dir.path().join("ESTOP"),
        )
        .await
        .unwrap();

        let chat = ChatId::Group(1);
        let (_tx, rx) = mpsc::unbounded_channel();
        runner
            .handle(
                TurnInput {
                    chat,
                    messages: vec![inbound(chat, 1, "hi")],
                    dropped: 0,
                },
                rx,
            )
            .await
            .unwrap();
        // 回合被跳过：没有 assistant 块。
        let current = runner.sessions.current().unwrap();
        let transcript = runner.sessions.read_segment(&current.file).unwrap();
        assert!(
            !transcript
                .iter()
                .any(|message| matches!(message, AgentMessage::Assistant(_)))
        );
        let _ = Usage::default();
        let _ = messages.usage().unwrap();
    }
}
