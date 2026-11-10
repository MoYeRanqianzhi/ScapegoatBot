//! 运行时装配与生命周期。
//!
//! 设计依据：设计 §2.1 / §2.3 / §4 / §10。
//!
//! 启动闭环：
//! 配置与模型加载 → 单实例锁 → 主密钥与数据库 → VFS / 消息 / transcript →
//! 安全闸 → IPC → NapCat（一直等到连上并登录）→ 工具 → 回合执行器。
//!
//! 控制通道先于 NapCat：等 NapCat 的那段时间里 `sbot status` / `sbot stop` 必须能用。
//! agent 在 NapCat 连上之后才建——登录的是哪个账号决定了 owner 与 master，建不了一半；
//! 这个 agent 无人值守，“NapCat 当时没起来就永远不连”等于永久失联。
//!
//! 本模块只做**装配**；业务在 `agent`、`tools`、`qq` 等模块。

mod assembly;
mod bootstrap;
mod events;
mod maintenance;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use serde_json::json;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::agent::queue::ChatQueue;
use crate::agent::runner::{Runner, RuntimeOps};
use crate::config::{AuthFile, LoadedConfig, ModelsFile, SbotConfig};
use crate::ipc::{self, AiAction, Handler, Request, RequestBody, Response};
use crate::monitor::MonitorManager;
use crate::paths::SbotPaths;
use crate::providers::ProviderSlot;
use crate::providers::{ThinkingOptions, build_metered, build_provider};
use crate::qq::QqClient;
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

use self::assembly::{
    AgentInputs, build_agent, load_models, napcat_client, open_stores, resolve_principals,
    wait_for_login,
};
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

/// agent 走到了哪一步。
///
/// agent 要等 NapCat 连上并登录之后才能建（登录的是哪个账号决定了 owner 与 master），
/// 所以守护进程活着不等于 agent 在跑。`sbot status` 的 `agent.state` 就是这个值。
enum AgentPhase {
    /// 还没连上 NapCat，或 NapCat 里还没有账号登录；一直在重试。
    WaitingForNapcat,
    /// NapCat 已就绪，正在建 agent。
    Starting,
    /// 在跑。
    Running(Arc<AgentRuntime>),
    /// 主模型用不了（不在模型清单里，或缺凭据）；改好配置后重启。
    NoProvider,
}

impl AgentPhase {
    /// `sbot status` 里 `agent.state` 的取值。
    fn name(&self) -> &'static str {
        match self {
            Self::WaitingForNapcat => "waiting_for_napcat",
            Self::Starting => "starting",
            Self::Running(_) => "running",
            Self::NoProvider => "no_provider",
        }
    }

    /// 在跑的 agent；没在跑时返回可以直接回给控制端的解释。
    fn running(&self) -> Result<Arc<AgentRuntime>, &'static str> {
        match self {
            Self::Running(agent) => Ok(Arc::clone(agent)),
            Self::WaitingForNapcat => Err(
                "the agent has not started: the daemon is still waiting for NapCat to be \
                 reachable and logged in. `sbot status` shows napcat.connected; the daemon log \
                 has the reason for each failed attempt.",
            ),
            Self::Starting => Err("the agent is still starting; try again in a moment."),
            Self::NoProvider => Err(
                "the agent cannot run: the primary model is unusable (not defined in \
                 models.json, or its credentials are missing). Fix the configuration and \
                 restart the daemon; `sbot doctor` lists what is missing.",
            ),
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
    /// agent 走到了哪一步；跑起来之后里面是回合执行器与队列。
    agent: RwLock<AgentPhase>,
    /// NapCat 客户端；开始等 NapCat 时放进来，之后一直是这一个。
    qq: std::sync::OnceLock<Arc<QqClient>>,
    /// 启动时间用于 uptime。
    started_at: chrono::DateTime<chrono::Utc>,
    /// 关闭信号。
    shutdown: watch::Sender<bool>,
    /// 单实例锁；字段本身只为保持文件句柄存活。
    _instance_lock: InstanceLock,
}

/// 守护进程为什么停下。
enum Stop {
    /// 有人要求：`sbot stop`、Ctrl-C 或终止信号。
    Requested,
    /// 控制通道建不起来或中途垮了。
    IpcFailed(anyhow::Error),
    /// 启动走不下去（见 [`Runtime::attach`]）。
    StartFailed(anyhow::Error),
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
        let Stores {
            db,
            key,
            vfs,
            messages,
            sessions,
            turn_buffer,
        } = open_stores(&paths, &loaded)?;

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

        let (shutdown, shutdown_rx) = watch::channel(false);
        let runtime = Arc::new(Self {
            paths: paths.clone(),
            config: Arc::new(RwLock::new(loaded)),
            models,
            auth,
            db,
            key,
            vfs,
            messages,
            sessions,
            gate,
            ai_enabled: Arc::new(AtomicBool::new(true)),
            agent: RwLock::new(AgentPhase::WaitingForNapcat),
            qq: std::sync::OnceLock::new(),
            started_at: chrono::Utc::now(),
            shutdown,
            _instance_lock: instance_lock,
        });

        let endpoint = {
            let config = runtime.config();
            ipc::Endpoint::resolve(&paths, &config.config.ipc)
        };
        info!(endpoint = %endpoint.display(), "starting IPC server");
        let mut server = tokio::spawn({
            let handler: Arc<dyn Handler> = runtime.clone();
            let endpoint = endpoint.clone();
            let shutdown_rx = shutdown_rx.clone();
            async move { ipc::serve(&endpoint, handler, shutdown_rx).await }
        });

        let stop = {
            let mut wait_shutdown = runtime.shutdown.subscribe();
            // 停下来的原因：有人要求（IPC、Ctrl-C、终止信号），或控制通道自己垮了。
            // `ipc::serve` 只在收到关闭信号时正常返回；在那之前返回说明控制通道建不起来
            // （套接字目录不安全、绑定失败、管道被占用）。没有控制通道的守护进程既查不了
            // 状态也停不下来，所以报错退出，而不是悄悄继续跑。
            let stop = async {
                tokio::select! {
                    _ = wait_shutdown.changed() => Stop::Requested,
                    _ = tokio::signal::ctrl_c() => {
                        info!("received Ctrl-C, shutting down");
                        Stop::Requested
                    }
                    _ = terminate_signal() => {
                        info!("received termination signal, shutting down");
                        Stop::Requested
                    }
                    result = &mut server => {
                        let error = match result {
                            Ok(Ok(())) => anyhow::anyhow!("the IPC server stopped unexpectedly"),
                            Ok(Err(error)) => error.context("the IPC server failed"),
                            Err(error) => anyhow::anyhow!("the IPC server task failed: {error}"),
                        };
                        Stop::IpcFailed(error)
                    }
                }
            };
            tokio::pin!(stop);
            // 第一段：等 NapCat、建 agent，随时可以被停下。第二段：一直跑到被停下。
            let attached = tokio::select! {
                stop = &mut stop => Err(stop),
                result = runtime.attach(turn_buffer, shutdown_rx) => Ok(result),
            };
            match attached {
                Err(stop) => stop,
                Ok(Ok(())) => stop.await,
                Ok(Err(error)) => Stop::StartFailed(error),
            }
        };
        if let Stop::IpcFailed(error) | Stop::StartFailed(error) = &stop {
            tracing::error!(error = %format!("{error:#}"), "shutting down after a fatal error");
        }

        // SessionEnd hook：在关闭 IPC 与 QQ 管线之前触发，`notify` 还能发出去；最多等 5 秒。
        if let Some(agent) = runtime.agent() {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                agent.runner.end_session(),
            )
            .await;
        }
        let _ = runtime.shutdown.send(true);
        // IPC 任务已经结束（并且被取走了结果）时不能再等它一次。
        if !matches!(stop, Stop::IpcFailed(_)) {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
        }
        info!("sbot stopped");
        match stop {
            Stop::Requested => Ok(()),
            Stop::IpcFailed(error) | Stop::StartFailed(error) => Err(error),
        }
    }

    /// 等 NapCat 连上并登录，然后建 agent、接上 QQ 事件。
    ///
    /// 等多久都等（[`wait_for_login`]）；调用方用 `select!` 让它可以被停止信号打断。
    /// 返回 `Err` 的都是重试没有意义的错误：NapCat 地址建不出客户端、`companion` 模式的
    /// `owner_qq` 与登录账号冲突、transcript 恢复失败。
    async fn attach(
        self: &Arc<Self>,
        turn_buffer: Arc<TurnBufferStore>,
        shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        // 配置读锁不能跨 await：要用的先取出来。
        let (client, url) = {
            let loaded = self.config();
            (
                napcat_client(&loaded.config, &self.auth)?,
                loaded.config.napcat.url.clone(),
            )
        };
        // 先放进去：等待期间 `sbot status` 就能看到连接状态。
        let _ = self.qq.set(Arc::clone(&client));
        let login = wait_for_login(&client, &url).await;
        info!(qq = login.user_id, nickname = %login.nickname, "connected to NapCat");
        // 从这里起断线由客户端自己恢复（SDK 的重连有次数上限，之后由它接手）。
        client.keep_connected(shutdown.clone());
        self.set_phase(AgentPhase::Starting);

        // 等待期间 `sbot model set` / `sbot effort set` 可能改过配置：建 agent 用现在的。
        let loaded = self.config().clone();
        let (owner_qq, master_qq) = resolve_principals(&loaded.config, login.user_id)?;
        let agent = build_agent(AgentInputs {
            loaded: &loaded,
            config_handle: Arc::clone(&self.config),
            models: &self.models,
            auth: &self.auth,
            db: Arc::clone(&self.db),
            key: Arc::clone(&self.key),
            vfs: Arc::clone(&self.vfs),
            messages: Arc::clone(&self.messages),
            sessions: Arc::clone(&self.sessions),
            turn_buffer,
            gate: Arc::clone(&self.gate),
            qq: Arc::clone(&client),
            self_id: login.user_id,
            owner_qq,
            master_qq,
            ai_enabled: Arc::clone(&self.ai_enabled),
            estop_file: self.paths.estop_file(),
            agents_dir: self
                .paths
                .resolve(&loaded.config.swarm.agents_dir, &loaded.dir),
        })
        .await?;
        let Some(agent) = agent else {
            // 原因 `build_agent` 已经记过；重试没有用，改好配置后重启。
            warn!("the primary model is unusable: the daemon stays up but the agent cannot run");
            self.set_phase(AgentPhase::NoProvider);
            return Ok(());
        };
        self.set_phase(AgentPhase::Running(Arc::clone(&agent)));
        self.catch_up_model_settings(&loaded.config);
        info!("agent runtime ready");
        spawn_qq_pipeline(agent, client, login, Arc::clone(&self.messages), shutdown);
        Ok(())
    }

    /// 把建 agent 期间错过的 `sbot model set` / `sbot effort set` 补上。
    ///
    /// 这两个命令在 agent 还没跑起来时只能改配置（没有 runner 可以热切换）。建 agent 用的是
    /// 开始时的配置快照 `built_with`，期间的改动要在 agent 就位之后再切一遍；就位之后的
    /// 改动由命令自己热切换。必须在阶段变成 [`AgentPhase::Running`] 之后调用，这样任何一次
    /// 改动要么被这里比对出来，要么自己看得到 agent，不会两头落空。
    fn catch_up_model_settings(&self, built_with: &SbotConfig) {
        let (model, level) = {
            let loaded = self.config();
            (
                loaded.config.agent.model.clone(),
                loaded.config.agent.thinking_level.clone(),
            )
        };
        if model != built_with.agent.model
            && let Err(error) = self.set_model(&model)
        {
            warn!(%error, %model, "cannot switch to the model chosen while the agent was starting");
        }
        if level != built_with.agent.thinking_level
            && let Err(error) = self.set_effort(&level)
        {
            warn!(%error, %level, "cannot apply the thinking level chosen while the agent was starting");
        }
    }

    /// 在跑的 agent；没在跑时为 `None`。
    fn agent(&self) -> Option<Arc<AgentRuntime>> {
        self.running_agent().ok()
    }

    /// 在跑的 agent；没在跑时返回可以直接回给控制端的解释。
    fn running_agent(&self) -> Result<Arc<AgentRuntime>, &'static str> {
        self.agent.read().expect("agent lock poisoned").running()
    }

    fn set_phase(&self, phase: AgentPhase) {
        *self.agent.write().expect("agent lock poisoned") = phase;
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
        // 阶段名与 agent 取自同一次读锁，两者不会对不上。
        let (state, agent) = {
            let phase = self.agent.read().expect("agent lock poisoned");
            (phase.name(), phase.running().ok())
        };
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
            // WebSocket 此刻是否连着。`false`：启动时还没连上，或连接断了；两种情况运行时
            // 都在一直重试。
            "napcat": {
                "connected": self.qq.get().is_some_and(|client| client.is_connected()),
            },
            "agent": {
                // `waiting_for_napcat` / `starting` / `running` / `no_provider`。
                "state": state,
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
        let agent = self.agent();
        // 先构造新 provider：失败时保持旧配置与旧 provider 不变。新 provider 接到同一个
        // 预算计量上，按新模型自己的价格记账（见 `providers::metered`）。
        let provider = match agent.as_ref().and_then(|agent| agent.runner.budget()) {
            Some(budget) => {
                let sink: Arc<dyn crate::providers::metered::UsageSink> = budget;
                build_metered(&self.models, &self.auth, model, &sink)?
            }
            None => build_provider(&self.models, &self.auth, model)?,
        };
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
        // 热切换：下一回合使用新 provider；在飞回合继续用旧快照。窗口与图片能力随模型
        // 一起换，压缩阈值与入站图片注入立即按新模型计算。
        if let Some(agent) = agent.as_ref() {
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
                context_window: reference.model.context_window,
                supports_images: reference.model.accepts_images(),
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

    /// `sbot model list`：白名单里的每个模型，是否在 models.json 里定义，以及关键能力。
    fn model_list(&self) -> serde_json::Value {
        let loaded = self.config();
        let config = &loaded.config;
        let models: Vec<serde_json::Value> = config
            .agent
            .available_models
            .iter()
            .map(|name| match self.models.find(name) {
                Some(reference) => json!({
                    "model": name,
                    "defined": true,
                    "context_window": reference.model.context_window,
                    "images": reference.model.input.iter().any(|kind| kind == "image"),
                    "thinking_levels": reference.model.thinking_levels,
                }),
                // 白名单里有、清单里没有：切换会失败，列出来让用户看到。
                None => json!({"model": name, "defined": false}),
            })
            .collect();
        json!({"current": config.agent.model, "available": models})
    }

    /// `sbot effort list`：当前模型支持的 effort 级别与当前值。
    fn effort_list(&self) -> Result<serde_json::Value> {
        let loaded = self.config();
        let config = &loaded.config;
        let model_name = &config.agent.model;
        let Some(reference) = self.models.find(model_name) else {
            anyhow::bail!("current model {model_name:?} is not defined in models.json");
        };
        Ok(json!({
            "model": model_name,
            "current": config.agent.thinking_level,
            "reasoning": reference.model.reasoning,
            // 空表示该模型不区分级别，任何值都会被接受（不推理的模型不发送 effort）。
            "levels": reference.model.thinking_levels,
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
        if let Some(agent) = self.agent()
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
            RequestBody::ModelList => Response::ok(id, self.model_list()),
            RequestBody::EffortList => match self.effort_list() {
                Ok(payload) => Response::ok(id, payload),
                Err(error) => Response::error(id, "invalid_model", error.to_string()),
            },
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
            },
            RequestBody::Chat { message, scope } => {
                if message.trim().is_empty() {
                    return Response::error(id, "bad_request", "message must not be empty");
                }
                // `--scope` 在落库与启动回合之前校验，写错了直接拒绝，不会悄悄忽略。
                let scope = match scope
                    .as_deref()
                    .map(str::trim)
                    .filter(|raw| !raw.is_empty())
                {
                    None => None,
                    Some(raw) => match crate::qq::event::ChatId::parse(raw) {
                        Some(chat) => Some(chat),
                        None => {
                            return Response::error(
                                id,
                                "bad_request",
                                format!(
                                    "scope {raw:?} must be \"group:<gid>\" or \"private:<qq>\""
                                ),
                            );
                        }
                    },
                };
                // agent 没在跑就直接拒绝，什么都不存：存下来的输入不会在之后补跑。
                let agent = match self.running_agent() {
                    Ok(agent) => agent,
                    Err(reason) => return Response::error(id, "not_ready", reason),
                };
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
                let runner = Arc::clone(&agent.runner);
                tokio::spawn(async move {
                    match runner.run_control(&message, control_id, scope).await {
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
                    json!({
                        "accepted": true,
                        "control_id": control_id,
                        "scope": scope.map(|chat| chat.to_string()),
                    }),
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
                let agent = match self.running_agent() {
                    Ok(agent) => agent,
                    Err(reason) => return Response::error(id, "not_ready", reason),
                };
                match agent.ops.resume().await {
                    Ok(payload) => Response::ok(id, payload),
                    Err(error) => Response::error(id, "hold", error),
                }
            }
            RequestBody::GatePending => {
                let agent = match self.running_agent() {
                    Ok(agent) => agent,
                    Err(reason) => return Response::error(id, "not_ready", reason),
                };
                Response::ok(id, json!({ "pending": agent.approvals.pending() }))
            }
            RequestBody::GateApprove { approval } => {
                let agent = match self.running_agent() {
                    Ok(agent) => agent,
                    Err(reason) => return Response::error(id, "not_ready", reason),
                };
                match agent.approvals.approve(&approval) {
                    Ok(payload) => Response::ok(id, payload),
                    Err(error) => Response::error(id, "gate", error),
                }
            }
            RequestBody::GateReject { approval } => {
                let agent = match self.running_agent() {
                    Ok(agent) => agent,
                    Err(reason) => return Response::error(id, "not_ready", reason),
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
    /// 准入过滤（消息、notice、request）；拒收的事件计入 `filtered`。
    fn admit(&self, event: &crate::qq::event::QqEvent) -> bool {
        let accept = self
            .filter
            .read()
            .expect("filter lock poisoned")
            .check_event(event, self.self_id)
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
