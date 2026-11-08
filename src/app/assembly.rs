//! Agent 装配：存储、provider、工具表、runner、队列与后台任务。
//!
//! 装配顺序（hub → MCP → monitor → API → runner → 队列）是这一层的核心；
//! 每一步都只依赖前一步已经就绪的句柄。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::agent::queue::ChatQueue;
use crate::agent::runner::{Runner, RunnerConfig, RuntimeOps};
use crate::config::{AgentMode, AuthFile, EncryptionMode, LoadedConfig, ModelsFile, SbotConfig};
use crate::mcp::{McpManager, McpTool};
use crate::monitor::{MonitorManager, MonitorSink, MonitorSpec};
use crate::paths::SbotPaths;
use crate::providers::metered::UsageSink;
use crate::providers::{Provider, ServerToolKind, ThinkingOptions, build_metered};
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
use crate::vfs::sqlite::VfsStore;

use super::events::{EventAudit, RunnerSink};
use super::{AgentRuntime, Stores};

/// 内置模型清单：随二进制发布，用户 `models.json` 覆盖/追加（设计 §9.3）。
const EMBEDDED_MODELS: &str = include_str!("../../assets/models.default.json");

// ---------------------------------------------------------------------------
// Agent 装配
// ---------------------------------------------------------------------------

/// 打开数据库、密钥与三套存储，并做启动对账。
pub(super) fn open_stores(paths: &SbotPaths, loaded: &LoadedConfig) -> Result<Stores> {
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
    // 保留期或大小上限任一生效就在启动时清理一次；大小上限在每次写入时也会检查（`make_room`）。
    if (loaded.config.storage.message_retention_days > 0
        || loaded.config.storage.max_message_bytes != 0)
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
    // 保留期或大小上限任一生效就在启动时清理一次（默认只有 2 GiB 的大小上限）。
    if sessions.limits_retention()
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
    /// 本账号 QQ（NapCat 登录信息）；0 = 未连接。
    account_qq: u64,
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
        account_qq,
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
    // 后台维护：技能 curator（可关）与回收站清理（总是开启）。
    if config.skills.curator {
        super::maintenance::spawn_skill_curator(
            Arc::clone(&vfs),
            Arc::clone(&db),
            config.skills.clone(),
        );
    }
    super::maintenance::spawn_trash_purge(Arc::clone(&vfs));
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
    if config.tools.web_fetch.local
        && config.tools.web_fetch.backend != crate::config::WebBackend::Off
    {
        // 本地抓取不需要 base_url/key；dummy 后端只用于复用结构体。
        match WebBackend::new("http://127.0.0.1:1", "") {
            Ok(backend) => tools.register(Arc::new(
                WebFetchTool::new(
                    Arc::new(backend),
                    config.tools.web_fetch.max_content_tokens,
                    download_policy.clone(),
                )
                .with_local(true),
            )),
            Err(error) => warn!(%error, "cannot register local WebFetch"),
        }
    } else if !fetch_server
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
    let approvals = crate::security::action_gate::ApprovalQueue::with_db(Arc::clone(&db));
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
        .with_account(account_qq)
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

/// 内置唤醒预设：对 `@` 与主人的消息唤醒；删除后会记录到 meta，重启不再恢复。
///
/// 这只是默认值，不是特权通道：所有非 agent 发出的消息（包括 stand_in 下主人在
/// 本账号上手打的）都进 inbox，唤醒与否完全由 monitor 决定。agent 可以改写或删除
/// 这些预设——哪怕是不想看主人的消息，也是它自己的选择。
///
/// 预设命中时并不使用这里的 prompt：`RunnerSink` 会把原始消息交给 ChatQueue，
/// 按普通 chat 回合处理（见 `app::events`）。prompt 只在消息缺失时兜底。
async fn install_wake_presets(monitors: &Arc<MonitorManager>, db: &Arc<Database>, owner_qq: u64) {
    let presets = [
        (
            "wake-on-mention",
            format!(
                "event.type == 'message' and event.mentions_self and event.sender != '{owner_qq}'"
            ),
            "You were mentioned in {{event.chat}}: {{event.text}}\n\
             Read /inbox if you need the surrounding conversation. Reply only if appropriate; \
             otherwise end the turn."
                .to_owned(),
        ),
        (
            "wake-on-owner",
            format!("event.type == 'message' and event.sender == '{owner_qq}'"),
            "The owner wrote in {{event.chat}}: {{event.text}}\n\
             Read /inbox if needed, then decide whether to respond."
                .to_owned(),
        ),
    ];
    for (name, when, prompt) in presets {
        if name == "wake-on-owner" && owner_qq == 0 {
            continue;
        }
        let key = format!("monitor.removed.{name}");
        if db.meta_get(&key).ok().flatten().as_deref() == Some(b"1") {
            continue;
        }
        if let Err(error) = monitors
            .add(MonitorSpec {
                name: name.to_owned(),
                source: "qq".to_owned(),
                timer: None,
                when: Some(when),
                prompt: Some(prompt),
                as_prompt: true,
                max_events: 1_000_000,
                expires: None,
                one_shot: false,
                min_interval: std::time::Duration::ZERO,
                idle_only: false,
                batch_window: std::time::Duration::ZERO,
                target: None,
                catch_up: crate::config::CatchUp::Coalesce,
                cc: false,
                on_missing: crate::config::OnMissing::Queue,
            })
            .await
        {
            warn!(%error, preset = name, "cannot install a wake preset");
        }
    }
}

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
    let budget = crate::budget::Budget::new(Arc::clone(&db), &config.budget, guard_cost);
    let meter: Arc<dyn UsageSink> = budget.clone();
    let provider: Arc<dyn Provider> = match build_metered(models, auth, &config.agent.model, &meter)
    {
        Ok(provider) => provider,
        Err(error) => {
            warn!(%error, model = %config.agent.model, "cannot build a provider");
            return Ok(None);
        }
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
            match build_metered(models, auth, reference, &meter) {
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
        effort: if model_ref.model.reasoning {
            thinking_effort(&config.agent.thinking_level, model_ref.model)
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
    let sink = Arc::new(RunnerSink::default());
    let monitors = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
    monitors.set_db(Arc::clone(&db));
    monitors.set_self_id(self_id);
    monitors.set_owner_qq(owner_qq);
    // 内部事件：安全闸、预算、子代理与存储巡检的状态变化交给 `internal:*` monitor。
    let internal = crate::monitor::InternalBus::new();
    monitors.listen(&internal);
    gate.set_events(internal.clone());
    budget.set_events(internal.clone());
    super::maintenance::spawn_storage_watch(Arc::clone(&vfs), internal.clone());

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
    let memory_flush = if config.compaction.memory_flush {
        Some(Arc::new(crate::memory::MemoryFlusher::new(
            memory_model.clone(),
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
        match build_metered(models, auth, config.agent.compaction_model.trim(), &meter) {
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
            receive_scopes: config.agent.receive_scopes.clone(),
            send_scopes: config.agent.send_scopes.clone(),
            gate_mode: config.gate.mode,
            max_tokens: config.agent.max_output_tokens,
            supports_images: model_ref.model.accepts_images(),
            thinking,
            max_steps: config.agent.max_steps,
            retry: crate::agent::agent_loop::RetryPolicy {
                attempts: config.agent.retry_attempts,
                base_ms: config.agent.retry_base_ms,
                max_ms: config.agent.retry_max_ms,
                mode: config.agent.retry_mode,
            },
            chat_history_limit: 20,
            context_budget: crate::prompt::context::ContextBudget::from_config(config),
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
            max_state_chars: config.security.guard.max_state_tokens as usize * 4,
            event_hooks: config.hooks.events.clone(),
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
            hub.set_events(internal.clone());
            runner.set_hub(Arc::clone(hub));
            let _ = sink.hub.set(Arc::clone(hub));
        }
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

    // `/config/filter` 写入后立即替换准入过滤器（弱引用避免循环）。
    let weak = Arc::downgrade(&runtime);
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

    Ok(Some(runtime))
}

/// 连接 NapCat；失败不阻止守护进程启动。
pub(super) async fn connect_qq(
    config: &SbotConfig,
    auth: &AuthFile,
) -> Option<(Arc<QqClient>, LoginInfo)> {
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
pub(super) fn resolve_principals(config: &SbotConfig, self_id: u64) -> Result<(u64, u64)> {
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

/// 按 `[security]` 构造周期检查器；`review_model` 为空时用主模型。
fn security_reviewer(
    security: &crate::config::SecurityConfig,
    models: &ModelsFile,
    auth: &AuthFile,
    provider: &Arc<dyn Provider>,
    model_ref: crate::config::models::ModelRef<'_>,
    meter: &Arc<dyn UsageSink>,
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
        match build_metered(models, auth, reference, meter) {
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

/// `[memory].review_model` → 独立的计量 provider；不设或构造失败时返回 `None`
/// （复盘与压缩前记忆提取跟随主模型，构造失败记警告）。
fn memory_review_model(
    memory: &crate::config::MemoryConfig,
    models: &ModelsFile,
    auth: &AuthFile,
    meter: &Arc<dyn UsageSink>,
) -> Option<crate::memory::ReviewModel> {
    let reference = memory.review_model.trim();
    if reference.is_empty() {
        return None;
    }
    // 存在性已由 `validate_with_models` 保证；这里仍防御一次。
    let found = models.find(reference)?;
    match build_metered(models, auth, reference, meter) {
        Ok(provider) => Some(crate::memory::ReviewModel {
            provider,
            model: found.model.id.clone(),
        }),
        Err(error) => {
            warn!(%error, model = %reference, "cannot build the memory review provider; using the main model");
            None
        }
    }
}

/// 显式配置优先；否则用模型声明的默认 effort；都没有则关闭 thinking。
fn thinking_effort(configured: &str, model: &crate::config::models::ModelDef) -> Option<String> {
    if !configured.is_empty() {
        Some(configured.to_owned())
    } else if !model.default_thinking_level.is_empty() {
        Some(model.default_thinking_level.clone())
    } else {
        None
    }
}

pub(super) fn load_models(paths: &SbotPaths, loaded: &LoadedConfig) -> Result<ModelsFile> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_effort_prefers_explicit_then_model_default() {
        let mut model = crate::config::models::ModelDef {
            reasoning: true,
            ..crate::config::models::ModelDef::default()
        };
        model.default_thinking_level = "high".to_owned();
        assert_eq!(thinking_effort("", &model), Some("high".to_owned()));
        assert_eq!(thinking_effort("low", &model), Some("low".to_owned()));
        model.default_thinking_level.clear();
        assert_eq!(thinking_effort("", &model), None);
    }
}
