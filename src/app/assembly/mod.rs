//! Agent 装配：存储、provider、工具表、runner、队列与后台任务。
//!
//! 装配顺序（hub → MCP → monitor → API → runner → 队列）是这一层的核心；
//! 每一步都只依赖前一步已经就绪的句柄。
//!
//! - `stores`：数据库、密钥与三套存储，启动对账；
//! - `providers`：模型清单、主模型 / 回退链 / 各辅助模型的计量 provider；
//! - `tools`：工具表与内置唤醒预设；
//! - `qq`：NapCat 连接与 owner / master 解析；
//! - 本文件：[`build_agent`] 按顺序把它们接起来。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::agent::queue::ChatQueue;
use crate::agent::runner::{Runner, RunnerConfig, RuntimeOps};
use crate::config::{AuthFile, LoadedConfig, ModelsFile, SbotConfig};
use crate::monitor::{MonitorManager, MonitorSink};
use crate::providers::{ServerToolKind, ThinkingOptions};
use crate::qq::QqClient;
use crate::qq::filter::MessageFilter;
use crate::security::gate::Gate;
use crate::session::SessionStore;
use crate::session::turn_buffer::TurnBufferStore;
use crate::storage::Database;
use crate::storage::crypto::MasterKey;
use crate::storage::messages::MessageStore;
use crate::tools::agent::HubHandle;
use crate::tools::views::{ViewContext, Views};
use crate::vfs::sqlite::VfsStore;

use super::AgentRuntime;
use super::events::{EventAudit, RunnerSink};

mod providers;
mod qq;
mod stores;
mod tools;

pub(super) use providers::load_models;
pub(super) use qq::{connect_qq, resolve_principals};
pub(super) use stores::open_stores;

use providers::{
    Providers, build_providers, compaction_model, install_llm_judge, memory_review_model,
    security_reviewer,
};
use tools::{ToolAssembly, assemble_tools, install_wake_presets};

/// `build_agent` 的全部输入；参数太多，打包成结构体。
///
/// 三个引用（`loaded` / `models` / `auth`）只借用调用方的数据，其余句柄都是
/// `Arc` 克隆：装配过程中会分发到 runner、工具表与后台任务，调用方仍需持有。
pub(super) struct AgentInputs<'a> {
    /// 已校验的配置（含配置目录）。
    pub loaded: &'a LoadedConfig,
    /// `/config` 热加载替换用的配置句柄。
    pub config_handle: Arc<RwLock<LoadedConfig>>,
    /// 模型清单（内置 + 用户覆盖）。
    pub models: &'a ModelsFile,
    /// 凭据。
    pub auth: &'a AuthFile,
    /// SQLite。
    pub db: Arc<Database>,
    /// 主密钥。
    pub key: Arc<MasterKey>,
    /// 虚拟文件系统。
    pub vfs: Arc<VfsStore>,
    /// 消息归档。
    pub messages: Arc<MessageStore>,
    /// transcript。
    pub sessions: Arc<SessionStore>,
    /// 未完成回合的缓冲。
    pub turn_buffer: Arc<TurnBufferStore>,
    /// 安全闸。
    pub gate: Arc<Gate>,
    /// NapCat 连接；未连接时为 `None`。
    pub qq: Option<Arc<QqClient>>,
    /// 登录账号。
    pub self_id: u64,
    /// 主人 QQ。
    pub owner_qq: u64,
    /// 最高权限 QQ。
    pub master_qq: u64,
    /// LLM 调用开关。
    pub ai_enabled: Arc<AtomicBool>,
    /// 急停文件。
    pub estop_file: PathBuf,
    /// 自定义子代理定义目录。
    pub agents_dir: PathBuf,
}

pub(super) async fn build_agent(input: AgentInputs<'_>) -> Result<Option<Arc<AgentRuntime>>> {
    let AgentInputs {
        loaded,
        config_handle,
        models,
        auth,
        db,
        key,
        vfs,
        messages,
        sessions,
        turn_buffer,
        gate,
        qq,
        self_id,
        owner_qq,
        master_qq,
        ai_enabled,
        estop_file,
        agents_dir,
    } = input;
    let config = &loaded.config;
    let Some(model_ref) = models.find(&config.agent.model) else {
        warn!(model = %config.agent.model, "the primary model is not defined in models.json");
        return Ok(None);
    };
    // 预算先于任何 provider：所有模型请求都经 `build_metered` 记进这里。
    let Some(Providers {
        provider,
        fallbacks,
        thinking,
        budget,
        meter,
    }) = build_providers(config, models, auth, &db, model_ref)
    else {
        return Ok(None);
    };
    install_llm_judge(config, &gate, &provider, model_ref);

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
            qq.clone()
                .map(|client| client as Arc<dyn crate::tools::api::NapcatGateway>),
        )
        .with_context(ViewContext {
            config: Some(Arc::clone(&config_handle)),
            models: Some(models.clone()),
            gate: Some(Arc::clone(&gate)),
            ai_enabled: Some(Arc::clone(&ai_enabled)),
            estop_file: Some(estop_file.clone()),
            started_at: Some(chrono::Utc::now()),
            db: Some(Arc::clone(&db)),
            key: Some(Arc::clone(&key)),
            receive_scopes: config.agent.receive_scopes.clone(),
            self_id,
        }),
    );
    let (sink, monitors, internal) = start_monitors(&db, &vfs, &gate, &budget, self_id, owner_qq);
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
        account_qq: self_id,
        owner_qq,
        master_qq,
        provider: Arc::clone(&provider),
        api_model: model_ref.model.id.clone(),
        chat_sends_per_hour: budget.chat_sends_per_hour(),
    })
    .await;

    let security_review =
        security_reviewer(&config.security, models, auth, &provider, model_ref, &meter);
    let last_activity = Arc::new(std::sync::atomic::AtomicI64::new(
        chrono::Utc::now().timestamp(),
    ));
    let memory_model = memory_review_model(&config.memory, models, auth, &meter);
    let memory_flush = config.compaction.memory_flush.then(|| {
        Arc::new(crate::memory::MemoryFlusher::new(
            memory_model.clone(),
            Arc::clone(&vfs),
            Some(Arc::clone(&db)),
            config.memory.clone(),
        ))
    });
    let compaction = compaction_model(config, models, auth, &meter);
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
        runner_config(
            config,
            model_ref,
            RunnerParts {
                account_qq: self_id,
                owner_qq,
                master_qq,
                thinking,
                server_tools,
                security_review,
                memory_flush,
                compaction,
                last_activity: Arc::clone(&last_activity),
                budget: Arc::clone(&budget),
            },
        ),
        Arc::clone(&history),
        Arc::clone(&ai_enabled),
        estop_file,
    )
    .await?;

    // runner 就绪后才能投递 monitor、执行手动压缩。
    let _ = sink.runner.set(Arc::clone(&runner));
    let _ = ops.runner.set(Arc::clone(&runner));
    if config.swarm.enabled {
        attach_swarm(
            config,
            &runner,
            &vfs,
            agents_dir,
            &hub_handle,
            &sink,
            &internal,
        );
    }
    crate::monitor::load_config_monitors(&monitors, &config.monitor, Some(&messages)).await?;
    install_wake_presets(&monitors, &db, owner_qq).await;

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
        review_model: memory_model,
        history: Arc::clone(&history),
        vfs: Arc::clone(&vfs),
        db: Some(Arc::clone(&db)),
        queue: Arc::clone(&queue) as Arc<dyn crate::memory::QueueActivity>,
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
        owner_qq,
        master_qq,
        audit: EventAudit {
            db: Arc::clone(&db),
            key: Arc::clone(&key),
        },
    });

    install_hot_reload(&views, &runtime);

    Ok(Some(runtime))
}

/// 建 monitor 管理器与内部事件总线，并接上会发事件的组件（安全闸、预算、存储巡检）。
///
/// 返回的 sink 在 runner 就绪后才填上 runner / hub / 队列：在那之前到达的投递只记警告。
fn start_monitors(
    db: &Arc<Database>,
    vfs: &Arc<VfsStore>,
    gate: &Arc<Gate>,
    budget: &Arc<crate::budget::Budget>,
    self_id: u64,
    owner_qq: u64,
) -> (
    Arc<RunnerSink>,
    Arc<MonitorManager>,
    crate::monitor::InternalBus,
) {
    let sink = Arc::new(RunnerSink::default());
    let monitors = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
    monitors.set_db(Arc::clone(db));
    monitors.set_self_id(self_id);
    monitors.set_owner_qq(owner_qq);
    // 内部事件：安全闸、预算、子代理与存储巡检的状态变化交给 `internal:*` monitor。
    let internal = crate::monitor::InternalBus::new();
    monitors.listen(&internal);
    gate.set_events(internal.clone());
    budget.set_events(internal.clone());
    crate::app::maintenance::spawn_storage_watch(Arc::clone(vfs), internal.clone());

    (sink, monitors, internal)
}

/// 子代理 hub 以 runner 为执行器；Agent 工具、monitor 投递与 runner 都通过句柄拿到它。
fn attach_swarm(
    config: &SbotConfig,
    runner: &Arc<Runner>,
    vfs: &Arc<VfsStore>,
    agents_dir: PathBuf,
    hub_handle: &Arc<HubHandle>,
    sink: &Arc<RunnerSink>,
    internal: &crate::monitor::InternalBus,
) {
    let hub = crate::swarm::AgentHub::new(
        Arc::clone(runner) as Arc<dyn crate::swarm::SubagentRunner>,
        config.swarm.clone(),
        Arc::clone(vfs),
        agents_dir,
    );
    if hub_handle.set(hub).is_err() {
        warn!("subagent hub was already initialized");
    }
    if let Some(hub) = hub_handle.get() {
        hub.set_events(internal.clone());
        runner.set_hub(Arc::clone(hub));
        let _ = sink.hub.set(Arc::clone(hub));
    }
}

/// `/config` 写入后立即替换准入过滤器与队列参数，并触发 `config_change` hook。
///
/// 回调只持有运行时的弱引用，避免 views ↔ runtime 循环引用。
fn install_hot_reload(views: &Arc<Views>, runtime: &Arc<AgentRuntime>) {
    let weak = Arc::downgrade(runtime);
    views.set_hot_reload(Arc::new(move |config: &SbotConfig| {
        if let Some(agent) = weak.upgrade() {
            *agent.filter.write().expect("filter lock poisoned") =
                MessageFilter::from_config(&config.filter);
            agent.queue.reload_config(config.queue.clone());
            info!("filter and queue settings reloaded from /config");
        }
        // ConfigChange 只允许 `log`（设计 §7.5）；配置已经写入，没有可阻止或注入的东西。
        let _ = crate::agent::hooks::fire(
            &config.hooks.events,
            crate::config::HookEvent::ConfigChange,
            None,
        );
    }));
}

/// `runner_config` 里不是直接从配置读出来的那部分：运行时身份与已经建好的组件。
struct RunnerParts {
    /// 登录账号 QQ。
    account_qq: u64,
    /// 已解析的主人 QQ。
    owner_qq: u64,
    /// 已解析的通知目标 QQ。
    master_qq: u64,
    /// 主模型的 thinking / effort。
    thinking: ThinkingOptions,
    /// provider 侧声明的服务端工具。
    server_tools: Vec<ServerToolKind>,
    /// 周期安全审查；`None` = 关闭。
    security_review: Option<Arc<dyn crate::security::review::SecurityCheck>>,
    /// 压缩前的记忆提取；`None` = 关闭。
    memory_flush: Option<Arc<crate::memory::MemoryFlusher>>,
    /// 独立的压缩摘要模型；`None` = 主模型。
    compaction: Option<Arc<crate::agent::runner::CompactionModel>>,
    /// 最近一次回合活动的时间戳（后台复盘据此判断空闲）。
    last_activity: Arc<std::sync::atomic::AtomicI64>,
    /// 每日 / 每回合预算。
    budget: Arc<crate::budget::Budget>,
}

/// 由配置与已建好的组件组装 [`RunnerConfig`]：字段一一对应，不含逻辑。
fn runner_config(
    config: &SbotConfig,
    model_ref: crate::config::models::ModelRef<'_>,
    parts: RunnerParts,
) -> RunnerConfig {
    RunnerConfig {
        model: config.agent.model.clone(),
        mode: config.agent.mode,
        account_qq: parts.account_qq,
        owner_qq: parts.owner_qq,
        master_qq: parts.master_qq,
        owner_name: config.agent.owner_name.clone(),
        receive_scopes: config.agent.receive_scopes.clone(),
        send_scopes: config.agent.send_scopes.clone(),
        gate_mode: config.gate.mode,
        max_tokens: config.agent.max_output_tokens,
        supports_images: model_ref.model.accepts_images(),
        thinking: parts.thinking,
        max_steps: config.agent.max_steps,
        retry: crate::agent::agent_loop::RetryPolicy {
            attempts: config.agent.retry_attempts,
            base_ms: config.agent.retry_base_ms,
            max_ms: config.agent.retry_max_ms,
            mode: config.agent.retry_mode,
        },
        chat_history_limit: 20,
        context_budget: crate::prompt::context::ContextBudget::from_config(config),
        server_tools: parts.server_tools,
        turn_context_budget: config.compaction.turn_context_budget,
        keep_recent_tokens: config.compaction.keep_recent_tokens,
        compaction_max_failures: config.compaction.max_failures,
        security_review: parts.security_review,
        check_every_tokens: config.security.check_every_tokens,
        check_overlap_tokens: config.security.check_overlap_tokens,
        check_on_compaction: config.security.check_on_compaction,
        context_window: model_ref.model.context_window,
        reserve_tokens: config.compaction.reserve_tokens,
        prune: config.compaction.prune,
        prune_soft_limit: config.compaction.prune_soft_limit,
        prune_keep_turns: config.compaction.prune_keep_turns,
        memory_flush: parts.memory_flush,
        post_compact_recall: config.compaction.post_compact_recall,
        compaction: parts.compaction,
        last_activity: parts.last_activity,
        budget: Some(parts.budget),
        max_state_chars: config.security.guard.max_state_tokens as usize * 4,
        event_hooks: config.hooks.events.clone(),
    }
}
