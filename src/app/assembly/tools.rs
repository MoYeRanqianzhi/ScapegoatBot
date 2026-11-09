//! 工具表装配与内置唤醒预设。

use std::sync::Arc;

use tracing::{info, warn};

use crate::agent::runner::RuntimeOps;
use crate::config::{AuthFile, SbotConfig};
use crate::mcp::{McpManager, McpTool};
use crate::monitor::{MonitorManager, MonitorOrigin, MonitorSpec};
use crate::providers::ServerToolKind;
use crate::qq::QqClient;
use crate::security::gate::Gate;
use crate::storage::Database;
use crate::storage::messages::MessageStore;
use crate::tools::ToolRegistry;
use crate::tools::agent::{AgentTool, HubHandle};
use crate::tools::api::{AgentOps, ApiTool, NapcatGateway};
use crate::tools::download::DownloadTool;
use crate::tools::memory::MemoryTool;
use crate::tools::monitor::MonitorTool;
use crate::tools::net::NetPolicy;
use crate::tools::skill::SkillTool;
use crate::tools::views::Views;
use crate::tools::web::{WebBackend, WebFetchTool, WebSearchTool};
use crate::vfs::sqlite::VfsStore;

/// 装配工具表所需的运行时句柄；参数太多，打包成结构体。
pub(super) struct ToolAssembly<'a> {
    pub(super) config: &'a SbotConfig,
    pub(super) auth: &'a AuthFile,
    pub(super) model_ref: crate::config::models::ModelRef<'a>,
    pub(super) db: Arc<Database>,
    pub(super) vfs: Arc<VfsStore>,
    pub(super) messages: Arc<MessageStore>,
    pub(super) gate: Arc<Gate>,
    pub(super) views: Arc<Views>,
    pub(super) monitors: Arc<MonitorManager>,
    pub(super) ops: Arc<RuntimeOps>,
    pub(super) qq: Option<Arc<QqClient>>,
    /// 本账号 QQ（NapCat 登录信息）；0 = 未连接。
    pub(super) account_qq: u64,
    pub(super) owner_qq: u64,
    pub(super) master_qq: u64,
    pub(super) provider: Arc<dyn crate::providers::Provider>,
    pub(super) api_model: String,
    pub(super) chat_sends_per_hour: u32,
}

/// 注册全部内置工具，返回注册表与 provider 侧声明的服务端工具。
///
/// 与 `build_agent` 分开：装配顺序（hub → MCP → monitor → API）是这里的主要
/// 复杂度，放在一个短函数里更容易核对。
pub(super) async fn assemble_tools(
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
        crate::app::maintenance::spawn_skill_curator(
            Arc::clone(&vfs),
            Arc::clone(&db),
            config.skills.clone(),
        );
    }
    crate::app::maintenance::spawn_trash_purge(Arc::clone(&vfs));
    // Agent 工具先注册；hub 需要 runner，稍后注入。
    let hub_handle = Arc::new(HubHandle::new());
    if config.swarm.enabled {
        tools.register(Arc::new(AgentTool::new(Arc::clone(&hub_handle), 0)));
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
/// 预设是 `deliver.as = "chat"`：命中时 `RunnerSink` 把原始消息交给 ChatQueue，按普通
/// 会话回合处理（见 `app::events`）；来源是 `Preset`，删除会被记住。
pub(super) async fn install_wake_presets(
    monitors: &Arc<MonitorManager>,
    db: &Arc<Database>,
    owner_qq: u64,
) {
    let presets = [
        (
            "wake-on-mention",
            format!(
                "event.type == 'message' and event.mentions_self and event.sender != '{owner_qq}'"
            ),
        ),
        (
            "wake-on-owner",
            format!("event.type == 'message' and event.sender == '{owner_qq}'"),
        ),
    ];
    for (name, when) in presets {
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
                prompt: None,
                deliver_as: crate::config::DeliverAs::Chat,
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
                origin: MonitorOrigin::Preset,
            })
            .await
        {
            warn!(%error, preset = name, "cannot install a wake preset");
        }
    }
}
