//! 运行时装配与生命周期。
//!
//! 设计依据：设计 §2.1 / §2.3 / §4 / §10。
//!
//! 启动闭环：
//! 配置与模型加载 → 单实例锁 → 主密钥与数据库 → VFS / 消息 / transcript →
//! 安全闸 → NapCat → 工具 → 回合执行器 → IPC。
//!
//! 本模块只做**装配**；业务在 `agent`、`tools`、`qq` 等模块。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use fs2::FileExt;
use serde_json::json;
use tokio::sync::{Mutex, watch};
use tracing::{debug, info, warn};

use crate::agent::queue::ChatQueue;
use crate::agent::runner::{ProviderSlot, Runner, RunnerConfig, RuntimeOps};
use crate::config::{
    AgentMode, AuthFile, EncryptionMode, LoadedConfig, ModelsFile, SbotConfig, SessionScope,
};
use crate::ipc::{self, AiAction, Handler, Request, RequestBody, Response};
use crate::mcp::{McpManager, McpTool};
use crate::monitor::{MonitorDelivery, MonitorManager, MonitorSink};
use crate::paths::SbotPaths;
use crate::providers::{Provider, ServerToolKind, ThinkingOptions, build_provider};
use crate::qq::event::QqEvent;
use crate::qq::filter::MessageFilter;
use crate::qq::{LoginInfo, QqClient};
use crate::security::gate::Gate;
use crate::session::SessionStore;
use crate::session::turn_buffer::TurnBufferStore;
use crate::storage::Database;
use crate::storage::crypto::MasterKey;
use crate::storage::messages::MessageStore;
use crate::tools::ToolRegistry;
use crate::tools::agent::{AgentTool, HubHandle};
use crate::tools::api::{AgentOps, ApiTool, NapcatGateway};
use crate::tools::download::DownloadTool;
use crate::tools::memory::MemoryTool;
use crate::tools::monitor::MonitorTool;
use crate::tools::net::NetPolicy;
use crate::tools::skill::SkillTool;
use crate::tools::views::{ViewContext, Views};
use crate::tools::web::{WebBackend, WebFetchTool, WebSearchTool};
use crate::vfs::Vfs;
use crate::vfs::sqlite::VfsStore;

/// 内置模型清单：随二进制发布，用户 `models.json` 覆盖/追加（设计 §9.3）。
const EMBEDDED_MODELS: &str = include_str!("../assets/models.default.json");

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
    /// `[gate]` 的待确认队列；IPC 用它批准/拒绝。
    approvals: Arc<crate::security::action_gate::ApprovalQueue>,
}

/// monitor 的投递端：把事件变成一次注入回合。
#[derive(Default)]
struct RunnerSink {
    runner: std::sync::OnceLock<Arc<Runner>>,
    /// 用于 `idle_only`：有活跃回合时不投递。
    queue: std::sync::OnceLock<Arc<ChatQueue>>,
    /// 绑定了子代理的 monitor 走这里。
    hub: std::sync::OnceLock<Arc<crate::swarm::AgentHub>>,
}

#[async_trait::async_trait]
impl MonitorSink for RunnerSink {
    async fn deliver(&self, delivery: MonitorDelivery) {
        // 绑定目标：交给子代理，不进主上下文。
        if let Some(target) = &delivery.target {
            let Some(hub) = self.hub.get() else {
                warn!(monitor = %delivery.name, target, "subagent monitor delivered before the hub was ready");
                return;
            };
            match delivery.on_missing {
                crate::config::OnMissing::Drop => {
                    if hub.deliver_to(target, delivery.text.clone()).await.is_err() {
                        warn!(monitor = %delivery.name, target, "subagent monitor target is missing; dropped");
                    }
                }
                crate::config::OnMissing::Queue => {
                    for _ in 0..30 {
                        if hub.deliver_to(target, delivery.text.clone()).await.is_ok() {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                    warn!(monitor = %delivery.name, target, "subagent monitor target never appeared; dropped");
                }
                crate::config::OnMissing::Spawn => {
                    if let Err(error) = hub
                        .spawn(
                            Some("general"),
                            None,
                            format!(
                                "[monitor {}]
{}",
                                delivery.name, delivery.text
                            ),
                            0,
                            crate::agent::TurnEvidence::monitor(),
                        )
                        .await
                    {
                        warn!(%error, monitor = %delivery.name, "cannot spawn a target for the monitor");
                    }
                }
            }
            return;
        }
        let Some(runner) = self.runner.get() else {
            warn!(monitor = %delivery.name, "monitor delivered before the runner was ready");
            return;
        };
        if let Err(error) = runner
            .run_event(&delivery.name, &delivery.source, &delivery.text)
            .await
        {
            warn!(%error, monitor = %delivery.name, "monitor delivery failed");
        }
    }

    async fn is_idle(&self) -> bool {
        match self.queue.get() {
            Some(queue) => queue.active_turns().await == 0,
            // 队列尚未装配时按空闲处理，避免启动阶段的 monitor 永久等待。
            None => true,
        }
    }
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

/// 打开数据库、密钥与三套存储，并做启动对账。
fn open_stores(paths: &SbotPaths, loaded: &LoadedConfig) -> Result<Stores> {
    let key = Arc::new(MasterKey::load_or_create(paths.key_file())?);
    let data_dir = paths.resolve(&loaded.config.storage.path, &loaded.dir);
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("cannot create storage path {}", data_dir.display()))?;
    let db = Arc::new(Database::open(data_dir.join("data.db"))?);
    db.ensure_key(&key)?;

    if loaded.config.storage.encryption == EncryptionMode::None {
        warn!("storage.encryption = \"none\": data is stored in plaintext (debug only)");
    }

    let vfs = Arc::new(VfsStore::from_config(
        Arc::clone(&db),
        Arc::clone(&key),
        data_dir.clone(),
        &loaded.config.storage,
    )?);
    let messages = Arc::new(
        MessageStore::open(
            Arc::clone(&db),
            Arc::clone(&key),
            data_dir.clone(),
            loaded.config.storage.max_message_bytes,
            loaded.config.storage.encryption,
        )?
        .with_retention_days(loaded.config.storage.message_retention_days),
    );
    if loaded.config.storage.message_retention_days > 0
        && let Err(error) = messages.enforce_retention(loaded.config.storage.message_retention_days)
    {
        warn!(%error, "cannot enforce message retention at startup");
    }
    // 重启核对发送账本：久悬的 pending 可能已经发出，标记为 unknown。
    match messages.mark_stale_intents(600) {
        Ok(0) => {}
        Ok(stale) => warn!(
            stale,
            "marked stale outbound intents as unknown; check /messages before resending"
        ),
        Err(error) => warn!(%error, "cannot reconcile the outbound intent ledger"),
    }

    // 启动校验：索引文件数与磁盘不一致时重建（索引只是缓存）。
    match messages.index_file_counts() {
        Ok((disk, indexed)) if disk != indexed => {
            warn!(
                disk,
                indexed, "message index file count mismatch; rebuilding"
            );
            match messages.rebuild_index() {
                Ok(repaired) => info!(repaired, "message index rebuilt"),
                Err(error) => warn!(%error, "message index rebuild failed"),
            }
        }
        Ok(_) => {}
        Err(error) => warn!(%error, "cannot compare the message index with the JSONL files"),
    }
    let sessions = Arc::new(
        SessionStore::open(
            Arc::clone(&db),
            Arc::clone(&key),
            data_dir.clone(),
            loaded.config.storage.max_session_bytes,
            loaded.config.storage.encryption,
        )?
        .with_retention_days(loaded.config.storage.session_retention_days),
    );
    if loaded.config.storage.session_retention_days > 0
        && let Err(error) = sessions.enforce_retention(loaded.config.storage.session_retention_days)
    {
        warn!(%error, "cannot enforce transcript retention at startup");
    }
    let turn_buffer = Arc::new(TurnBufferStore::new(
        Arc::clone(&db),
        Arc::clone(&key),
        loaded.config.storage.encryption,
    ));

    Ok(Stores {
        db,
        key,
        vfs,
        messages,
        sessions,
        turn_buffer,
    })
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
// Agent 装配
// ---------------------------------------------------------------------------

/// 装配工具表所需的运行时句柄；参数太多，打包成结构体。
struct ToolAssembly<'a> {
    config: &'a SbotConfig,
    auth: &'a AuthFile,
    model_ref: crate::config::models::ModelRef<'a>,
    db: Arc<Database>,
    vfs: Arc<VfsStore>,
    messages: Arc<MessageStore>,
    gate: Arc<Gate>,
    views: Arc<Views>,
    monitors: Arc<MonitorManager>,
    ops: Arc<RuntimeOps>,
    qq: Option<Arc<QqClient>>,
    owner_qq: u64,
    master_qq: u64,
    provider: Arc<dyn crate::providers::Provider>,
    api_model: String,
    chat_sends_per_hour: u32,
}

/// 注册全部内置工具，返回注册表与 provider 侧声明的服务端工具。
///
/// 与 `build_agent` 分开：装配顺序（hub → MCP → monitor → API）是这里的主要
/// 复杂度，放在一个短函数里更容易核对。
async fn assemble_tools(
    assembly: ToolAssembly<'_>,
) -> (
    Arc<ToolRegistry>,
    Vec<ServerToolKind>,
    Arc<HubHandle>,
    Arc<crate::security::action_gate::ApprovalQueue>,
) {
    let ToolAssembly {
        config,
        auth,
        model_ref,
        db,
        vfs,
        messages,
        gate,
        views,
        monitors,
        ops,
        qq,
        owner_qq,
        master_qq,
        provider,
        api_model,
        chat_sends_per_hour,
    } = assembly;
    let mut tools = ToolRegistry::new(Arc::clone(&vfs), Some(Arc::clone(&views)))
        .with_gate(Arc::clone(&gate))
        .with_max_result_bytes(config.tools.max_result_bytes)
        .with_max_schema_tokens(config.tools.max_schema_tokens);
    crate::tools::fs::register(&mut tools);
    tools.register(Arc::new(
        SkillTool::new(Arc::clone(&vfs)).with_db(Arc::clone(&db)),
    ));
    // 技能 curator：每小时检查一次；只在积累足够新技能时运行，只归档不删除。
    if config.skills.curator {
        let vfs_curator = Arc::clone(&vfs);
        let db_curator = Arc::clone(&db);
        let skills_config = config.skills.clone();
        tokio::spawn(async move {
            loop {
                let vfs = Arc::clone(&vfs_curator);
                let db = Arc::clone(&db_curator);
                let cfg = skills_config.clone();
                match tokio::task::spawn_blocking(move || {
                    crate::tools::skill::curate(&vfs, &db, &cfg)
                })
                .await
                {
                    Ok(Ok(report)) if !report.archived.is_empty() => {
                        info!(archived = ?report.archived, "skill curator archived unused skills");
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => warn!(%error, "skill curator failed"),
                    Err(error) => warn!(%error, "skill curator task failed"),
                }
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    }
    // Agent 工具先注册；hub 需要 runner，稍后注入。
    let hub_handle = Arc::new(HubHandle::new());
    if config.swarm.enabled {
        tools.register(Arc::new(
            AgentTool::new(Arc::clone(&hub_handle), 0).with_monitors(Arc::clone(&monitors)),
        ));
    }
    if config.mcp.enabled {
        let manager = Arc::new(McpManager::connect(&config.mcp).await);
        let count = manager.tools().await.len();
        if count == 0 {
            warn!("MCP is enabled but no server exposed any tool");
        } else {
            info!(tools = count, "MCP tools registered");
        }
        tools.register(Arc::new(McpTool::new(manager)));
    }
    tools.register(Arc::new(MonitorTool::new(Arc::clone(&monitors))));
    if config.memory.enabled {
        tools.register(Arc::new(
            MemoryTool::new(
                Arc::clone(&vfs),
                config.memory.max_entry_chars,
                config.memory.max_index_lines,
                config.memory.max_index_bytes,
                config.memory.user_max_chars,
            )
            .with_db(Arc::clone(&db)),
        ));
    }
    let download_policy = NetPolicy {
        allowed_domains: config.tools.download.allowed_domains.clone(),
        blocked_domains: config.tools.download.blocked_domains.clone(),
        max_bytes: config.tools.download.max_bytes,
        ..NetPolicy::default()
    };
    match DownloadTool::new(Arc::clone(&vfs), download_policy.clone()) {
        Ok(tool) => tools.register(Arc::new(tool)),
        Err(error) => warn!(%error, "cannot register Download"),
    }

    // Web 工具：服务端工具由 provider 声明；本地工具后端只在用户配置了
    // base_url 与 key 时注册，不假定官方地址。
    let protocol = model_ref.provider.api;
    let mut server_tools: Vec<ServerToolKind> = Vec::new();
    let mut add_server =
        |kind: ServerToolKind, capability: &str, backend: crate::config::WebBackend| {
            let server_capable = protocol != crate::config::models::Protocol::Chat
                && model_ref.model.supports(capability);
            if server_capable
                && matches!(
                    backend,
                    crate::config::WebBackend::Auto | crate::config::WebBackend::Server
                )
            {
                server_tools.push(kind);
                true
            } else {
                false
            }
        };
    let search_server = add_server(
        ServerToolKind::WebSearch,
        "server_tools.web_search",
        config.tools.web_search.backend,
    );
    let fetch_server = add_server(
        ServerToolKind::WebFetch,
        "server_tools.web_fetch",
        config.tools.web_fetch.backend,
    );

    let use_tool_backend = |backend: crate::config::WebBackend| {
        matches!(backend, crate::config::WebBackend::Tool)
            || (matches!(backend, crate::config::WebBackend::Auto)
                && protocol == crate::config::models::Protocol::Chat)
    };
    if !search_server
        && use_tool_backend(config.tools.web_search.backend)
        && let Some(key) = auth
            .tools
            .web_search
            .api_key
            .value()
            .filter(|_| !config.tools.web_search.base_url.trim().is_empty())
    {
        match WebBackend::new(config.tools.web_search.base_url.clone(), key) {
            Ok(backend) => tools.register(Arc::new(WebSearchTool::new(
                Arc::new(backend),
                config.tools.web_search.count,
            ))),
            Err(error) => warn!(%error, "cannot register WebSearch"),
        }
    }
    if !fetch_server
        && use_tool_backend(config.tools.web_fetch.backend)
        && let Some(key) = auth
            .tools
            .web_fetch
            .api_key
            .value()
            .filter(|_| !config.tools.web_fetch.base_url.trim().is_empty())
    {
        match WebBackend::new(config.tools.web_fetch.base_url.clone(), key) {
            Ok(backend) => tools.register(Arc::new(WebFetchTool::new(
                Arc::new(backend),
                config.tools.web_fetch.max_content_tokens,
                download_policy,
            ))),
            Err(error) => warn!(%error, "cannot register WebFetch"),
        }
    }

    let gateway: Option<Arc<dyn NapcatGateway>> = qq
        .as_ref()
        .map(|client| Arc::clone(client) as Arc<dyn NapcatGateway>);

    // `[gate]`：所有 napcat.* 调用都过这里；llm_review 用主 provider。
    let approvals = crate::security::action_gate::ApprovalQueue::new();
    let mut action_gate =
        crate::security::action_gate::ActionGate::new(&config.gate, Arc::clone(&approvals))
            .with_db(Arc::clone(&db))
            .with_chat_send_limit(chat_sends_per_hour);
    if config.gate.llm_review {
        action_gate = action_gate.with_reviewer(Arc::new(
            crate::security::action_gate::LlmActionReviewer::new(
                Arc::clone(&provider),
                api_model,
                std::time::Duration::from_secs(10),
            ),
        ));
    }
    let action_gate = Arc::new(action_gate);

    tools.register(Arc::new(
        ApiTool::new(
            gateway.clone(),
            Some(ops.clone() as Arc<dyn AgentOps>),
            owner_qq,
            master_qq,
            Some(Arc::clone(&gate)),
            config.agent.send_scopes.clone(),
            Some(Arc::clone(&messages)),
        )
        .with_action_gate(Arc::clone(&action_gate))
        .with_receive_scopes(config.agent.receive_scopes.clone())
        .with_event_hooks(config.hooks.events.clone()),
    ));
    // 子代理（docs、未开 allow_send 的自定义定义）拿到的是只读副本：
    // 同一个工具名，但只允许 api.list / api.help。
    let read_only_api = Arc::new(
        ApiTool::new(
            gateway,
            Some(ops.clone() as Arc<dyn AgentOps>),
            owner_qq,
            master_qq,
            Some(Arc::clone(&gate)),
            Vec::new(),
            Some(Arc::clone(&messages)),
        )
        .read_only(),
    );
    let tools = Arc::new(tools.with_read_only_api(read_only_api));

    (tools, server_tools, hub_handle, approvals)
}

#[allow(clippy::too_many_arguments)]
async fn build_agent(
    loaded: &LoadedConfig,
    config_handle: Arc<RwLock<LoadedConfig>>,
    models: &ModelsFile,
    auth: &AuthFile,
    db: Arc<Database>,
    vfs: Arc<VfsStore>,
    messages: Arc<MessageStore>,
    sessions: Arc<SessionStore>,
    turn_buffer: Arc<TurnBufferStore>,
    gate: Arc<Gate>,
    qq: Option<Arc<QqClient>>,
    self_id: u64,
    owner_qq: u64,
    master_qq: u64,
    ai_enabled: Arc<AtomicBool>,
    estop_file: PathBuf,
    agents_dir: PathBuf,
) -> Result<Option<Arc<AgentRuntime>>> {
    let config = &loaded.config;
    let provider: Arc<dyn Provider> = match build_provider(models, auth, &config.agent.model) {
        Ok(provider) => provider,
        Err(error) => {
            warn!(%error, model = %config.agent.model, "cannot build a provider");
            return Ok(None);
        }
    };
    let Some(model_ref) = models.find(&config.agent.model) else {
        anyhow::bail!("model {} disappeared from models.json", config.agent.model);
    };
    // 回退链：每个模型单独构造 provider（协议/密钥可能不同）。
    let fallbacks: Vec<crate::agent::FallbackProvider> = config
        .agent
        .fallback_models
        .iter()
        .filter_map(|reference| {
            let Some(def) = models.find(reference) else {
                warn!(model = %reference, "fallback model is not defined in models.json");
                return None;
            };
            match build_provider(models, auth, reference) {
                Ok(provider) => Some(crate::agent::FallbackProvider {
                    model: def.model.id.clone(),
                    provider,
                    max_tokens: def.model.max_tokens.min(config.agent.max_output_tokens),
                }),
                Err(error) => {
                    warn!(%error, model = %reference, "cannot build a fallback provider");
                    None
                }
            }
        })
        .collect();
    let thinking = ThinkingOptions {
        enabled: model_ref.model.reasoning,
        effort: if model_ref.model.reasoning && !config.agent.thinking_level.is_empty() {
            Some(config.agent.thinking_level.clone())
        } else {
            None
        },
    };

    // `backend = "llm"` 需要 provider；在这里把它装进闸。
    if config.security.guard.enabled
        && config.security.guard.backend == crate::config::GuardBackend::Llm
    {
        let model = if config.security.guard.model.is_empty() {
            model_ref.model.id.clone()
        } else {
            config.security.guard.model.clone()
        };
        let judge = crate::security::gate::LlmJudge::new(
            Arc::clone(&provider),
            model,
            std::time::Duration::from_millis(config.security.guard.timeout_ms),
        );
        if gate.install_judge(Arc::new(judge)).is_err() {
            warn!("a guard judge was already installed");
        }
    }

    let history = Arc::new(Mutex::new(Vec::new()));
    let ops = Arc::new(RuntimeOps {
        runner: std::sync::OnceLock::new(),
        gate: Arc::clone(&gate),
        vfs: Arc::clone(&vfs),
        sessions: Arc::clone(&sessions),
        messages: Arc::clone(&messages),
        db: Some(Arc::clone(&db)),
        history: Arc::clone(&history),
        model: Arc::new(std::sync::RwLock::new(config.agent.model.clone())),
        owner_qq,
        master_qq,
        receive_scopes: config.agent.receive_scopes.clone(),
        send_scopes: config.agent.send_scopes.clone(),
        estop_file: estop_file.clone(),
    });

    let views = Arc::new(
        Views::new(
            Arc::clone(&vfs),
            Arc::clone(&messages),
            Arc::clone(&sessions),
            qq.clone(),
        )
        .with_context(ViewContext {
            config: Some(Arc::clone(&config_handle)),
            models: Some(models.clone()),
            gate: Some(Arc::clone(&gate)),
            ai_enabled: Some(Arc::clone(&ai_enabled)),
            estop_file: Some(estop_file.clone()),
            started_at: Some(chrono::Utc::now()),
            db: Some(Arc::clone(&db)),
            receive_scopes: config.agent.receive_scopes.clone(),
        }),
    );
    let sink = Arc::new(RunnerSink::default());
    let monitors = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
    monitors.set_db(Arc::clone(&db));

    let guard_cost = match config.security.guard.backend {
        crate::config::GuardBackend::Off => 0.0,
        crate::config::GuardBackend::Jev => crate::budget::JEV_CALL_USD,
        crate::config::GuardBackend::Llm => {
            // 粗略：一次判定约 3k 输入 + 100 输出 token。
            let input = 3_000.0 * model_ref.model.cost.input;
            let output = 100.0 * model_ref.model.cost.output;
            (input + output) / 1_000_000.0
        }
    };
    let budget = crate::budget::Budget::new(
        Arc::clone(&db),
        &config.budget,
        &model_ref.model.cost,
        guard_cost,
    );
    gate.set_budget(Arc::clone(&budget));

    let (tools, server_tools, hub_handle, approvals) = assemble_tools(ToolAssembly {
        config,
        auth,
        model_ref,
        db: Arc::clone(&db),
        vfs: Arc::clone(&vfs),
        messages: Arc::clone(&messages),
        gate: Arc::clone(&gate),
        views: Arc::clone(&views),
        monitors: Arc::clone(&monitors),
        ops: Arc::clone(&ops),
        qq: qq.clone(),
        owner_qq,
        master_qq,
        provider: Arc::clone(&provider),
        api_model: model_ref.model.id.clone(),
        chat_sends_per_hour: budget.chat_sends_per_hour(),
    })
    .await;

    let security_review = security_reviewer(&config.security, models, auth, &provider, model_ref);
    let last_activity = Arc::new(std::sync::atomic::AtomicI64::new(
        chrono::Utc::now().timestamp(),
    ));
    let memory_flush = if config.compaction.memory_flush {
        Some(Arc::new(crate::memory::MemoryFlusher::new(
            Arc::clone(&provider),
            model_ref.model.id.clone(),
            Arc::clone(&vfs),
            Some(Arc::clone(&db)),
            config.memory.clone(),
        )))
    } else {
        None
    };
    let compaction = if config.agent.compaction_model.trim().is_empty() {
        None
    } else {
        match build_provider(models, auth, config.agent.compaction_model.trim()) {
            Ok(provider) => models
                .find(config.agent.compaction_model.trim())
                .map(|found| {
                    Arc::new(crate::agent::runner::CompactionModel {
                        provider,
                        model: found.model.id.clone(),
                    })
                }),
            Err(error) => {
                warn!(%error, model = %config.agent.compaction_model, "cannot build the compaction provider");
                None
            }
        }
    };
    let runner = Runner::new_with_fallbacks(
        provider,
        fallbacks,
        tools,
        Arc::clone(&gate),
        Arc::clone(&vfs),
        Arc::clone(&messages),
        Arc::clone(&sessions),
        turn_buffer,
        qq,
        RunnerConfig {
            model: config.agent.model.clone(),
            mode: config.agent.mode,
            account_qq: self_id,
            owner_qq,
            master_qq,
            owner_name: config.agent.owner_name.clone(),
            session_scope: match config.session.scope {
                SessionScope::Shared => "shared".to_owned(),
                SessionScope::PerChat => "per_chat".to_owned(),
            },
            receive_scopes: config.agent.receive_scopes.clone(),
            send_scopes: config.agent.send_scopes.clone(),
            gate_mode: config.gate.mode,
            max_tokens: config.agent.max_output_tokens,
            thinking,
            max_steps: config.agent.max_steps,
            retry: crate::agent::agent_loop::RetryPolicy {
                attempts: config.agent.retry_attempts,
                base_ms: config.agent.retry_base_ms,
                max_ms: config.agent.retry_max_ms,
            },
            chat_history_limit: 20,
            hooks: config.hooks.context.clone(),
            server_tools,
            turn_context_budget: config.compaction.turn_context_budget,
            keep_recent_tokens: config.compaction.keep_recent_tokens,
            compaction_max_failures: config.compaction.max_failures,
            security_review,
            check_every_tokens: config.security.check_every_tokens,
            check_overlap_tokens: config.security.check_overlap_tokens,
            check_on_compaction: config.security.check_on_compaction,
            context_window: model_ref.model.context_window,
            reserve_tokens: config.compaction.reserve_tokens,
            prune: config.compaction.prune,
            prune_soft_limit: config.compaction.prune_soft_limit,
            prune_keep_turns: config.compaction.prune_keep_turns,
            memory_flush,
            post_compact_recall: config.compaction.post_compact_recall,
            compaction,
            last_activity: Arc::clone(&last_activity),
            budget: Some(Arc::clone(&budget)),
        },
        Arc::clone(&history),
        Arc::clone(&ai_enabled),
        estop_file,
    )
    .await?;

    // runner 就绪后才能投递 monitor、执行手动压缩。
    let _ = sink.runner.set(Arc::clone(&runner));
    let _ = ops.runner.set(Arc::clone(&runner));
    // 子代理 hub 以 runner 为执行器；Agent 工具通过句柄读取。
    if config.swarm.enabled {
        let hub = crate::swarm::AgentHub::new(
            Arc::clone(&runner) as Arc<dyn crate::swarm::SubagentRunner>,
            config.swarm.clone(),
            Arc::clone(&vfs),
            agents_dir,
        );
        if hub_handle.set(hub).is_err() {
            warn!("subagent hub was already initialized");
        }
        if let Some(hub) = hub_handle.get() {
            runner.set_hub(Arc::clone(hub));
            let _ = sink.hub.set(Arc::clone(hub));
        }
    }
    crate::monitor::load_config_monitors(&monitors, &config.monitor, Some(&messages)).await?;

    let filter = MessageFilter::from_config(&config.filter);
    let queue = ChatQueue::new(
        config.queue.clone(),
        Arc::clone(&runner) as Arc<dyn crate::agent::queue::TurnHandler>,
    );
    // `idle_only` monitor 依赖队列的活跃回合数。
    let _ = sink.queue.set(Arc::clone(&queue));

    // 后台复盘：记忆候选 + 风格画像；`review = off` 时立即返回。
    crate::memory::spawn(crate::memory::ReviewerConfig {
        provider: runner.provider_slot(),
        history: Arc::clone(&history),
        vfs: Arc::clone(&vfs),
        db: Some(Arc::clone(&db)),
        queue: Arc::clone(&queue),
        memory: config.memory.clone(),
        style: config.style.clone(),
        ai_enabled: Arc::clone(&ai_enabled),
        last_activity,
    });

    let runtime = Arc::new(AgentRuntime {
        runner,
        queue,
        ops,
        monitors,
        self_id,
        filter: Arc::new(RwLock::new(filter)),
        filtered: AtomicU64::new(0),
        approvals,
    });

    // `/config/filter` 写入后立即替换准入过滤器（弱引用避免循环）。
    let weak = Arc::downgrade(&runtime);
    views.set_hot_reload(Arc::new(move |config: &SbotConfig| {
        if let Some(agent) = weak.upgrade() {
            *agent.filter.write().expect("filter lock poisoned") =
                MessageFilter::from_config(&config.filter);
            agent.queue.reload_config(config.queue.clone());
            info!("filter and queue settings reloaded from /config");
        }
    }));

    Ok(Some(runtime))
}

/// 连接 NapCat；失败不阻止守护进程启动。
async fn connect_qq(config: &SbotConfig, auth: &AuthFile) -> Option<(Arc<QqClient>, LoginInfo)> {
    let token = auth.napcat.access_token.value();
    let client = match QqClient::new(&config.napcat.url, token.as_deref()) {
        Ok(client) => Arc::new(client),
        Err(error) => {
            warn!(%error, "cannot build the NapCat client");
            return None;
        }
    };
    match tokio::time::timeout(std::time::Duration::from_secs(10), client.connect()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            warn!(%error, url = %config.napcat.url, "cannot connect to NapCat");
            return None;
        }
        Err(_) => {
            warn!(url = %config.napcat.url, "NapCat connection timed out");
            return None;
        }
    }
    match tokio::time::timeout(std::time::Duration::from_secs(10), client.login_info()).await {
        Ok(Ok(info)) => Some((client, info)),
        Ok(Err(error)) => {
            warn!(%error, "cannot read NapCat login info");
            None
        }
        Err(_) => {
            warn!("NapCat login info timed out");
            None
        }
    }
}

/// 解析 owner/master。
fn resolve_principals(config: &SbotConfig, self_id: u64) -> Result<(u64, u64)> {
    let agent = &config.agent;
    let owner_qq = match agent.mode {
        AgentMode::StandIn => {
            if agent.owner_qq != 0 {
                agent.owner_qq
            } else {
                self_id
            }
        }
        AgentMode::Companion => {
            if agent.owner_qq == 0 {
                anyhow::bail!("companion mode requires a non-zero owner_qq");
            }
            if self_id != 0 && agent.owner_qq == self_id {
                anyhow::bail!("companion mode requires owner_qq to differ from the account");
            }
            agent.owner_qq
        }
    };
    let master_qq = if agent.master == "self" {
        self_id
    } else {
        agent
            .master
            .parse()
            .map_err(|_| anyhow::anyhow!("agent.master must be \"self\" or a QQ number"))?
    };
    Ok((owner_qq, master_qq))
}

/// NapCat 事件循环。
fn spawn_qq_pipeline(
    agent: Arc<AgentRuntime>,
    client: Arc<QqClient>,
    info: LoginInfo,
    messages: Arc<MessageStore>,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut subscription = client.subscribe();
        info!(qq = info.user_id, "listening for QQ events");
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                event = subscription.recv() => {
                    match event {
                        Some(data) => {
                            handle_qq_event(
                                &agent,
                                &messages,
                                &data.name,
                                &data.data,
                                info.user_id,
                            )
                            .await;
                        }
                        None => {
                            warn!("NapCat event subscription ended");
                            break;
                        }
                    }
                }
            }
        }
    });
}

async fn handle_qq_event(
    agent: &AgentRuntime,
    messages: &MessageStore,
    name: &str,
    data: &serde_json::Value,
    _self_id: u64,
) {
    let event = crate::qq::event::normalize(name, data);
    // 准入过滤必须在 monitor 之前：被拒消息既不应入库，也不应通过 monitor
    // 进入上下文（否则过滤只剩“不写 JSONL”，挡不住注入）。
    if let QqEvent::Message(message) = &event
        && !agent.filter_and_count(message)
    {
        debug!(chat = %message.chat, "message rejected by the admission filter");
        return;
    }
    agent.monitors.on_qq_event(&event).await;
    match event {
        QqEvent::Message(message) => match messages.append(&message) {
            Ok(true) => {
                if !message.is_self && !message.backfilled {
                    agent.queue.ingest(message).await;
                }
            }
            Ok(false) => {}
            Err(error) => warn!(%error, "cannot store an inbound message"),
        },
        QqEvent::Notice(notice) => {
            debug!(kind = %notice.kind, "QQ notice");
        }
        QqEvent::Request(request) => {
            debug!(kind = %request.kind, actor = ?request.actor, "QQ request");
        }
        QqEvent::Meta { kind } => debug!(kind, "meta event"),
        QqEvent::Connection { connected, reason } => {
            debug!(connected, %reason, "connection state");
        }
        QqEvent::Unknown { name, .. } => debug!(name, "unknown QQ event"),
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

// ---------------------------------------------------------------------------
// 启动辅助
// ---------------------------------------------------------------------------

fn ensure_default_config(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(&SbotConfig::default())
        .context("cannot serialize the default configuration")?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(text.as_bytes())
                .with_context(|| format!("cannot write {}", path.display()))?;
            info!(path = %path.display(), "created default configuration");
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(anyhow::anyhow!("cannot create {}: {error}", path.display())),
    }
}

/// 按 `[security]` 构造周期检查器；`review_model` 为空时用主模型。
fn security_reviewer(
    security: &crate::config::SecurityConfig,
    models: &ModelsFile,
    auth: &AuthFile,
    provider: &Arc<dyn Provider>,
    model_ref: crate::config::models::ModelRef<'_>,
) -> Option<Arc<dyn crate::security::review::SecurityCheck>> {
    if security.check_every_tokens == 0 && !security.check_on_compaction {
        return None;
    }
    let (reviewer, model) = if security.review_model.trim().is_empty() {
        (Arc::clone(provider), model_ref.model.id.clone())
    } else {
        let reference = security.review_model.trim();
        let Some(found) = models.find(reference) else {
            warn!(model = %reference, "security.review_model is not defined in models.json");
            return None;
        };
        match build_provider(models, auth, reference) {
            Ok(provider) => (provider, found.model.id.clone()),
            Err(error) => {
                warn!(%error, model = %reference, "cannot build the security review provider");
                return None;
            }
        }
    };
    Some(Arc::new(crate::security::review::SecurityReviewer::new(
        reviewer,
        model,
        std::time::Duration::from_secs(security.check_timeout_secs.max(1)),
    )))
}

fn load_models(paths: &SbotPaths, loaded: &LoadedConfig) -> Result<ModelsFile> {
    let mut models =
        ModelsFile::parse(EMBEDDED_MODELS).context("embedded models.default.json is invalid")?;
    let user_path = paths.resolve(&loaded.config.models_file, &loaded.dir);
    if user_path.exists() {
        models.merge(ModelsFile::load(&user_path)?);
    }
    if let Err(errors) = models.validate() {
        anyhow::bail!(
            "models.json validation failed:\n  - {}",
            errors.join("\n  - ")
        );
    }
    Ok(models)
}

fn report_missing_credentials(config: &SbotConfig, auth: &AuthFile) {
    let referenced = config
        .agent
        .available_models
        .iter()
        .filter_map(|reference| {
            reference
                .split_once('/')
                .map(|(provider, _)| provider.to_owned())
        });
    let missing = auth.missing_providers(referenced);
    if !missing.is_empty() {
        warn!(
            providers = ?missing,
            "missing API keys in auth.json; models from these providers are unavailable"
        );
    }
}

struct InstanceLock {
    file: std::fs::File,
    path: PathBuf,
}

impl InstanceLock {
    fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("cannot open lock file {}", path.display()))?;
        let pid_path = path.with_file_name("instance.pid");

        if let Err(error) = file.try_lock_exclusive() {
            let existing = std::fs::read_to_string(&pid_path).unwrap_or_default();
            anyhow::bail!(
                "another sbot instance is already running (lock {} held{}): {error}",
                path.display(),
                if existing.trim().is_empty() {
                    String::new()
                } else {
                    format!(" by pid {}", existing.trim())
                }
            );
        }
        let _ = write!(file, "{}", std::process::id());
        let _ = file.flush();
        let _ = std::fs::write(&pid_path, std::process::id().to_string());
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_file_name("instance.pid"));
    }
}

fn write_sentinel(path: &Path) -> Result<()> {
    std::fs::write(path, chrono::Utc::now().to_rfc3339())
        .with_context(|| format!("cannot create {}", path.display()))
}

fn remove_sentinel(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(anyhow::anyhow!("cannot remove {}: {error}", path.display())),
    }
}

fn persist_agent_setting(path: &Path, key: &str, value: &str) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("cannot parse {}", path.display()))?;
    let agent = document
        .entry("agent")
        .or_insert(toml_edit::table())
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[agent] in {} is not a table", path.display()))?;
    agent[key] = toml_edit::value(value);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, document.to_string())
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
async fn terminate_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut stream) => {
            stream.recv().await;
        }
        Err(error) => {
            warn!(%error, "cannot install SIGTERM handler");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(unix))]
async fn terminate_signal() {
    std::future::pending::<()>().await
}
