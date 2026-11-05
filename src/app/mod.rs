//! 运行时装配与生命周期。
//!
//! 设计依据：设计 §2.1 / §2.3 / §4 / §10。
//!
//! 启动闭环：
//! 配置与模型加载 → 单实例锁 → 主密钥与数据库 → VFS / 消息 / transcript →
//! 安全闸 → NapCat → 工具 → 回合执行器 → IPC。
//!
//! 本模块只做**装配**；业务在 `agent`、`tools`、`qq` 等模块。

mod assembly;
mod bootstrap;
mod events;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use serde_json::json;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::agent::queue::ChatQueue;
use crate::agent::runner::{ProviderSlot, Runner, RuntimeOps};
use crate::config::{AuthFile, LoadedConfig, ModelsFile, SbotConfig};
use crate::ipc::{self, AiAction, Handler, Request, RequestBody, Response};
use crate::monitor::MonitorManager;
use crate::paths::SbotPaths;
use crate::providers::{ThinkingOptions, build_provider};
use crate::qq::filter::MessageFilter;
use crate::security::gate::Gate;
use crate::session::SessionStore;
use crate::session::turn_buffer::TurnBufferStore;
use crate::storage::Database;
use crate::storage::crypto::MasterKey;
use crate::storage::messages::MessageStore;
use crate::tools::api::AgentOps;
use crate::vfs::Vfs;
use crate::vfs::sqlite::VfsStore;

use self::assembly::{build_agent, connect_qq, load_models, open_stores, resolve_principals};
use self::bootstrap::{
    InstanceLock, ensure_default_config, persist_agent_setting, remove_sentinel,
    report_missing_credentials, terminate_signal, write_sentinel,
};
use self::events::{EventAudit, spawn_qq_pipeline};

/// 回合执行所需的组件。
struct AgentRuntime {
    runner: Arc<Runner>,
    queue: Arc<ChatQueue>,
    ops: Arc<RuntimeOps>,
    monitors: Arc<MonitorManager>,
    self_id: u64,
    /// 准入过滤器；`/config/filter` 写入后原地替换。
    filter: Arc<RwLock<MessageFilter>>,
    filtered: AtomicU64,
    /// `[gate]` 的待确认队列；IPC 与主人私聊 nonce 都能批准/拒绝。
    approvals: Arc<crate::security::action_gate::ApprovalQueue>,
    owner_qq: u64,
    master_qq: u64,
    audit: EventAudit,
}

/// 守护进程运行时。
pub struct Runtime {
    /// 路径布局。
    pub paths: SbotPaths,
    /// 已校验的配置；`SetModel` / `SetEffort` 会就地更新。
    config: Arc<RwLock<LoadedConfig>>,
    /// 模型清单（内置 + 用户覆盖）。
    models: ModelsFile,
    /// 凭据（只读，不挂载 VFS）；`sbot doctor` 会读取。
    #[allow(dead_code)]
    auth: AuthFile,
    /// SQLite。
    db: Arc<Database>,
    /// 主密钥。
    key: Arc<MasterKey>,
    /// 虚拟文件系统。
    vfs: Arc<VfsStore>,
    /// 消息归档。
    messages: Arc<MessageStore>,
    /// transcript。
    sessions: Arc<SessionStore>,
    /// 安全闸。
    gate: Arc<Gate>,
    /// LLM 调用开关（`sbot ai stop` 只停 LLM，不收消息）。
    ai_enabled: Arc<AtomicBool>,
    /// 回合执行器与队列；provider 未配置时为 `None`。
    agent: RwLock<Option<Arc<AgentRuntime>>>,
    /// 启动时间用于 uptime。
    started_at: chrono::DateTime<chrono::Utc>,
    /// 关闭信号。
    shutdown: watch::Sender<bool>,
    /// 单实例锁；字段本身只为保持文件句柄存活。
    _instance_lock: InstanceLock,
}

/// 启动时要打开的全部存储句柄。
struct Stores {
    db: Arc<Database>,
    key: Arc<MasterKey>,
    vfs: Arc<VfsStore>,
    messages: Arc<MessageStore>,
    sessions: Arc<SessionStore>,
    turn_buffer: Arc<TurnBufferStore>,
}

impl Runtime {
    /// 启动运行时。
    pub async fn start(config_path: Option<PathBuf>) -> Result<()> {
        let paths = SbotPaths::discover()?;
        paths.ensure_layout()?;

        let config_path = config_path.unwrap_or_else(|| paths.config_file());
        ensure_default_config(&config_path)?;

        let loaded = SbotConfig::load(&config_path)?;
        let models = load_models(&paths, &loaded)?;
        loaded
            .config
            .validate_with_models(&models)
            .map_err(|errors| anyhow::anyhow!("config validation failed:\n{errors}"))?;

        let auth_path = paths.resolve(&loaded.config.auth_file, &loaded.dir);
        let auth = AuthFile::load_optional(&auth_path)?;
        report_missing_credentials(&loaded.config, &auth);

        let instance_lock = InstanceLock::acquire(&paths.instance_lock())?;
        let stores = open_stores(&paths, &loaded)?;
        let db = Arc::clone(&stores.db);
        let key = Arc::clone(&stores.key);
        let vfs = Arc::clone(&stores.vfs);
        let messages = Arc::clone(&stores.messages);
        let sessions = Arc::clone(&stores.sessions);
        let turn_buffer = Arc::clone(&stores.turn_buffer);

        let gate = Arc::new(Gate::from_config(
            &loaded.config.security.guard,
            auth.guard_key().and_then(|secret| secret.value()),
            Some(Arc::clone(&db)),
            paths.security_hold_file(),
        ));
        gate.restore_hold();
        if gate.hold_active() {
            warn!("starting while SECURITY_HOLD is active; outbound chat stays disabled");
        }
        // enforce 但没有可用分类器时，闸会把每条消息都按住；启动时先讲清楚，
        // 免得主人以为是 NapCat 或模型坏了。
        if loaded.config.security.guard.enabled
            && loaded.config.security.guard.mode == crate::config::GuardMode::Enforce
            && !gate.classifier_available()
            && loaded.config.security.guard.backend == crate::config::GuardBackend::Jev
        {
            warn!(
                "security.guard.mode = enforce but no classifier is configured: every new turn \
                 will be held. Set security.guard.base_url plus auth.json security.guard.api_key, \
                 or set security.guard.backend = \"off\" for rules-only enforce"
            );
        }

        let ai_enabled = Arc::new(AtomicBool::new(true));
        let qq = connect_qq(&loaded.config, &auth).await;
        let self_id = qq.as_ref().map(|(_, info)| info.user_id).unwrap_or(0);
        if let Some((client, info)) = &qq {
            info!(qq = info.user_id, nickname = %info.nickname, "connected to NapCat");
            if let Err(error) = client.version_info().await {
                warn!(%error, "cannot read the NapCat version");
            }
        } else {
            warn!("NapCat is not connected; QQ tools will fail until it is");
        }

        let (owner_qq, master_qq) = resolve_principals(&loaded.config, self_id)?;
        let config_handle = Arc::new(RwLock::new(loaded.clone()));
        let agent = build_agent(
            &loaded,
            Arc::clone(&config_handle),
            &models,
            &auth,
            Arc::clone(&db),
            Arc::clone(&key),
            Arc::clone(&vfs),
            Arc::clone(&messages),
            Arc::clone(&sessions),
            turn_buffer,
            Arc::clone(&gate),
            qq.as_ref().map(|(client, _)| Arc::clone(client)),
            self_id,
            owner_qq,
            master_qq,
            Arc::clone(&ai_enabled),
            paths.estop_file(),
            paths.resolve(&loaded.config.swarm.agents_dir, &loaded.dir),
        )
        .await?;

        if agent.is_some() {
            info!("agent runtime ready");
        } else {
            warn!("no usable provider; the daemon runs but cannot answer");
        }

        let (shutdown, shutdown_rx) = watch::channel(false);
        let runtime = Arc::new(Self {
            paths: paths.clone(),
            config: Arc::clone(&config_handle),
            models,
            auth,
            db,
            key,
            vfs,
            messages,
            sessions,
            gate,
            ai_enabled,
            agent: RwLock::new(agent.clone()),
            started_at: chrono::Utc::now(),
            shutdown,
            _instance_lock: instance_lock,
        });

        // NapCat 事件循环。
        if let (Some(agent), Some((client, info))) = (agent, qq) {
            spawn_qq_pipeline(
                agent,
                client,
                info,
                Arc::clone(&runtime.messages),
                shutdown_rx.clone(),
            );
        }

        let endpoint = {
            let config = runtime.config();
            ipc::Endpoint::resolve(&paths, &config.config.ipc)
        };
        info!(endpoint = %endpoint.display(), "starting IPC server");
        let server = tokio::spawn({
            let handler: Arc<dyn Handler> = runtime.clone();
            let endpoint = endpoint.clone();
            async move { ipc::serve(&endpoint, handler, shutdown_rx).await }
        });

        let mut wait_shutdown = runtime.shutdown.subscribe();
        tokio::select! {
            _ = wait_shutdown.changed() => {}
            _ = tokio::signal::ctrl_c() => info!("received Ctrl-C, shutting down"),
            _ = terminate_signal() => info!("received termination signal, shutting down"),
        }
        let _ = runtime.shutdown.send(true);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
        info!("sbot stopped");
        Ok(())
    }

    fn config(&self) -> std::sync::RwLockReadGuard<'_, LoadedConfig> {
        self.config.read().expect("config lock poisoned")
    }

    fn estop_active(&self) -> bool {
        self.paths.estop_file().exists()
    }

    fn status_payload(&self) -> serde_json::Value {
        let loaded = self.config();
        let config = &loaded.config;
        let agent = self.agent.read().expect("agent lock poisoned").clone();
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "started_at": self.started_at.to_rfc3339(),
            "uptime_secs": (chrono::Utc::now() - self.started_at).num_seconds().max(0),
            "home": self.paths.home().display().to_string(),
            "config_path": loaded.path.display().to_string(),
            "schema_version": self.db.schema_version().ok(),
            "mode": config.agent.mode,
            "model": config.agent.model,
            "thinking_level": config.agent.thinking_level,
            "available_models": config.agent.available_models,
            "ai_enabled": self.ai_enabled.load(Ordering::Relaxed),
            "estop": self.estop_active(),
            "hold": self.gate.hold_active(),
            "session_scope": config.session.scope,
            "agent": {
                "available": agent.is_some(),
                "self_id": agent.as_ref().map(|agent| agent.self_id),
                "filtered_messages": agent.as_ref().map(|agent| agent.filtered.load(Ordering::Relaxed)),
            },
            "vfs": self.vfs.usage().ok().map(|usage| json!({
                "active_bytes": usage.active_bytes,
                "trash_bytes": usage.trash_bytes,
                "nodes": usage.nodes,
                "trash_nodes": usage.trash_nodes,
            })),
            "messages": self.messages.usage().ok().map(|(bytes, count)| json!({
                "bytes": bytes,
                "count": count,
            })),
            "unconfirmed_sends": self.messages.unconfirmed_sends().map(|rows| rows.len()).unwrap_or(0),
            "sessions": self.sessions.usage().ok().map(|(bytes, count)| json!({
                "bytes": bytes,
                "count": count,
            })),
            "guard": {
                "enabled": config.security.guard.enabled,
                "mode": config.security.guard.mode,
                "backend": config.security.guard.backend,
            },
            "gate": {
                "mode": config.gate.mode,
                "max_per_minute": config.gate.max_per_minute,
                "llm_review": config.gate.llm_review,
                "pending_approvals": agent.as_ref().map_or(0, |agent| agent.approvals.pending_count()),
            },
        })
    }

    fn set_model(&self, model: &str) -> Result<serde_json::Value> {
        let mut loaded = self.config.write().expect("config lock poisoned");
        let config = &mut loaded.config;
        if !config
            .agent
            .available_models
            .iter()
            .any(|item| item == model)
        {
            anyhow::bail!(
                "model {model:?} is not in agent.available_models; available: {:?}",
                config.agent.available_models
            );
        }
        let Some(reference) = self.models.find(model) else {
            anyhow::bail!("model {model:?} is not defined in models.json");
        };
        // 先构造新 provider：失败时保持旧配置与旧 provider 不变。
        let provider = build_provider(&self.models, &self.auth, model)?;
        let api_model = reference.model.id.clone();
        let thinking = ThinkingOptions {
            enabled: reference.model.reasoning,
            effort: if reference.model.reasoning && !config.agent.thinking_level.trim().is_empty() {
                Some(config.agent.thinking_level.clone())
            } else {
                None
            },
        };
        let max_tokens = config.agent.max_output_tokens;

        let previous = std::mem::replace(&mut config.agent.model, model.to_owned());
        if let Err(error) = persist_agent_setting(&loaded.path, "model", model) {
            warn!(%error, "cannot persist agent.model; change is runtime-only");
        }
        // 热切换：下一回合使用新 provider；在飞回合继续用旧快照。
        if let Some(agent) = self.agent.read().expect("agent lock poisoned").as_ref() {
            *agent
                .runner
                .provider_slot()
                .write()
                .expect("provider slot lock poisoned") = ProviderSlot {
                reference: model.to_owned(),
                api_model,
                provider,
                max_tokens,
                thinking,
            };
            *agent.ops.model.write().expect("model lock poisoned") = model.to_owned();
        }
        info!(from = %previous, to = model, "primary model switched");
        Ok(json!({
            "model": model,
            "context_window": reference.model.context_window,
            "thinking_levels": reference.model.thinking_levels,
            "note": "takes effect on the next request",
        }))
    }

    fn set_effort(&self, level: &str) -> Result<serde_json::Value> {
        let mut loaded = self.config.write().expect("config lock poisoned");
        let config = &mut loaded.config;
        let model_name = config.agent.model.clone();
        let Some(reference) = self.models.find(&model_name) else {
            anyhow::bail!("current model {model_name:?} is not defined in models.json");
        };
        if !reference.model.thinking_levels.is_empty()
            && !reference
                .model
                .thinking_levels
                .iter()
                .any(|item| item == level)
        {
            anyhow::bail!(
                "thinking level {level:?} is not supported by {model_name}; supported: {:?}",
                reference.model.thinking_levels
            );
        }
        config.agent.thinking_level = level.to_owned();
        if let Err(error) = persist_agent_setting(&loaded.path, "thinking_level", level) {
            warn!(%error, "cannot persist agent.thinking_level; change is runtime-only");
        }
        if let Some(agent) = self.agent.read().expect("agent lock poisoned").as_ref()
            && let Ok(mut slot) = agent.runner.provider_slot().write()
        {
            slot.thinking.effort = if reference.model.reasoning {
                Some(level.to_owned())
            } else {
                None
            };
        }
        Ok(json!({ "thinking_level": level }))
    }

    async fn dispatch(&self, request: Request) -> Response {
        let id = request.id.clone();
        match request.body {
            RequestBody::Status => Response::ok(id, self.status_payload()),
            RequestBody::Shutdown => {
                let shutdown = self.shutdown.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let _ = shutdown.send(true);
                });
                Response::ok(id, json!({ "stopping": true }))
            }
            RequestBody::Ai { action } => match action {
                AiAction::Start => {
                    self.ai_enabled.store(true, Ordering::Relaxed);
                    Response::ok(id, json!({ "ai_enabled": true }))
                }
                AiAction::Stop => {
                    self.ai_enabled.store(false, Ordering::Relaxed);
                    Response::ok(id, json!({ "ai_enabled": false }))
                }
                AiAction::Restart => {
                    self.ai_enabled.store(true, Ordering::Relaxed);
                    Response::ok(id, json!({ "ai_enabled": true, "restarted": true }))
                }
            },
            RequestBody::Chat { message, scope } => {
                if message.trim().is_empty() {
                    return Response::error(id, "bad_request", "message must not be empty");
                }
                let sealed =
                    match self
                        .key
                        .seal("control", b"control_inputs.text_enc", message.as_bytes())
                    {
                        Ok(sealed) => sealed,
                        Err(error) => {
                            return Response::error(
                                id,
                                "internal",
                                format!("cannot encrypt input: {error}"),
                            );
                        }
                    };
                let control_id = match self.db.insert_control_input(&sealed, "cli") {
                    Ok(control_id) => control_id,
                    Err(error) => {
                        return Response::error(
                            id,
                            "internal",
                            format!("cannot store input: {error}"),
                        );
                    }
                };
                let agent = self.agent.read().expect("agent lock poisoned").clone();
                let Some(agent) = agent else {
                    return Response::error(
                        id,
                        "not_ready",
                        "no provider is configured; control input was stored but not run",
                    );
                };
                let runner = Arc::clone(&agent.runner);
                tokio::spawn(async move {
                    match runner.run_control(&message, control_id).await {
                        Ok(outcome) => info!(
                            stop = ?outcome.stop_reason,
                            turns = outcome.turns,
                            "control turn finished"
                        ),
                        Err(error) => warn!(%error, "control turn failed"),
                    }
                });
                Response::ok(
                    id,
                    json!({ "accepted": true, "control_id": control_id, "scope": scope }),
                )
            }
            RequestBody::SetModel { model } => match self.set_model(&model) {
                Ok(payload) => Response::ok(id, payload),
                Err(error) => Response::error(id, "invalid_model", error.to_string()),
            },
            RequestBody::SetEffort { level } => match self.set_effort(&level) {
                Ok(payload) => Response::ok(id, payload),
                Err(error) => Response::error(id, "invalid_effort", error.to_string()),
            },
            RequestBody::Pause => match write_sentinel(&self.paths.estop_file()) {
                Ok(()) => Response::ok(id, json!({ "paused": true })),
                Err(error) => Response::error(id, "internal", error.to_string()),
            },
            RequestBody::Resume => match remove_sentinel(&self.paths.estop_file()) {
                Ok(()) => Response::ok(id, json!({ "paused": false })),
                Err(error) => Response::error(id, "internal", error.to_string()),
            },
            RequestBody::SecurityStatus => {
                let events = crate::security::events::recent(&self.db, 20).unwrap_or_default();
                Response::ok(
                    id,
                    json!({
                        "hold": self.gate.hold_active(),
                        "estop": self.estop_active(),
                        "events": events,
                    }),
                )
            }
            RequestBody::SecurityResume => {
                if !self.gate.hold_active() {
                    return Response::ok(id, json!({ "hold": false }));
                }
                let agent = self.agent.read().expect("agent lock poisoned").clone();
                let Some(agent) = agent else {
                    return Response::error(
                        id,
                        "not_ready",
                        "no agent runtime is available to re-check the context",
                    );
                };
                match agent.ops.resume().await {
                    Ok(payload) => Response::ok(id, payload),
                    Err(error) => Response::error(id, "hold", error),
                }
            }
            RequestBody::GatePending => {
                let agent = self.agent.read().expect("agent lock poisoned").clone();
                let Some(agent) = agent else {
                    return Response::error(id, "not_ready", "no agent runtime is available");
                };
                Response::ok(id, json!({ "pending": agent.approvals.pending() }))
            }
            RequestBody::GateApprove { approval } => {
                let agent = self.agent.read().expect("agent lock poisoned").clone();
                let Some(agent) = agent else {
                    return Response::error(id, "not_ready", "no agent runtime is available");
                };
                match agent.approvals.approve(&approval) {
                    Ok(payload) => Response::ok(id, payload),
                    Err(error) => Response::error(id, "gate", error),
                }
            }
            RequestBody::GateReject { approval } => {
                let agent = self.agent.read().expect("agent lock poisoned").clone();
                let Some(agent) = agent else {
                    return Response::error(id, "not_ready", "no agent runtime is available");
                };
                match agent.approvals.reject(&approval) {
                    Ok(payload) => Response::ok(id, payload),
                    Err(error) => Response::error(id, "gate", error),
                }
            }
        }
    }
}

impl AgentRuntime {
    fn filter_and_count(&self, message: &crate::qq::event::QqMessage) -> bool {
        let accept = self
            .filter
            .read()
            .expect("filter lock poisoned")
            .check(message, self.self_id)
            .is_accept();
        if !accept {
            self.filtered.fetch_add(1, Ordering::Relaxed);
        }
        accept
    }
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl Handler for Runtime {
    async fn handle(&self, request: Request) -> Response {
        let idempotency_key = if request.wants_idempotency() {
            request.idempotency_key.clone()
        } else {
            None
        };
        if let Some(key) = idempotency_key.as_deref() {
            match self.db.idempotency_get(key) {
                Ok(Some(stored)) => {
                    return serde_json::from_str(&stored).unwrap_or_else(|_| {
                        Response::error(
                            request.id,
                            "internal",
                            "stored idempotent response is corrupt",
                        )
                    });
                }
                Ok(None) => {}
                Err(error) => warn!(%error, "idempotency lookup failed"),
            }
        }
        let response = self.dispatch(request).await;
        if response.ok
            && let Some(key) = idempotency_key
            && let Ok(serialized) = serde_json::to_string(&response)
            && let Err(error) = self.db.idempotency_put(&key, &serialized)
        {
            warn!(%error, "cannot store idempotent response");
        }
        response
    }
}
