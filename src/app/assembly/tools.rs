//! 工具表装配与内置唤醒预设。

use std::sync::Arc;

use tracing::{info, warn};

use crate::agent::runner::RuntimeOps;
use crate::config::{AuthFile, SbotConfig};
use crate::mcp::{McpManager, McpTool};
use crate::monitor::{MonitorManager, MonitorOrigin, MonitorSpec};
use crate::providers::ServerToolKind;
use crate::qq::QqClient;
use crate::security::action_gate::{ActionGate, ApprovalQueue, LlmActionReviewer};
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

/// [`assemble_tools`] 的产物。
pub(super) struct ToolSet {
    /// 主代理的工具表（含给子代理用的只读 `API` 副本）。
    pub(super) tools: Arc<ToolRegistry>,
    /// provider 侧声明的服务端工具，随每个请求发送。
    pub(super) server_tools: Vec<ServerToolKind>,
    /// 子代理 hub 的句柄；hub 需要 runner，runner 建好后由 `attach_swarm` 填入。
    pub(super) hub_handle: Arc<HubHandle>,
    /// `[gate] mode = "approve"` 的待批队列；`sbot approve` 经运行时访问它。
    pub(super) approvals: Arc<ApprovalQueue>,
}

/// 注册全部内置工具，返回工具表与 provider 侧声明的服务端工具。
///
/// 工具表按名字排序（`BTreeMap`），注册顺序不影响请求里的 schema 顺序；这里只要求
/// 依赖先就绪：`Agent` 拿的是空 hub 句柄，`API` 要先建好 action gate。
pub(super) async fn assemble_tools(assembly: ToolAssembly<'_>) -> ToolSet {
    let config = assembly.config;
    let vfs = &assembly.vfs;
    let db = &assembly.db;
    let mut tools = ToolRegistry::new(Arc::clone(vfs), Some(Arc::clone(&assembly.views)))
        .with_gate(Arc::clone(&assembly.gate))
        .with_max_result_bytes(config.tools.max_result_bytes)
        .with_max_schema_tokens(config.tools.max_schema_tokens);
    crate::tools::fs::register(&mut tools);
    tools.register(Arc::new(
        SkillTool::new(Arc::clone(vfs)).with_db(Arc::clone(db)),
    ));
    // Agent 工具先注册；hub 需要 runner，稍后注入。
    let hub_handle = Arc::new(HubHandle::new());
    if config.swarm.enabled {
        tools.register(Arc::new(AgentTool::new(Arc::clone(&hub_handle), 0)));
    }
    if config.mcp.enabled {
        register_mcp(&mut tools, &config.mcp).await;
    }
    tools.register(Arc::new(MonitorTool::new(Arc::clone(&assembly.monitors))));
    if config.memory.enabled {
        let memory = &config.memory;
        tools.register(Arc::new(
            MemoryTool::new(
                Arc::clone(vfs),
                memory.max_entry_chars,
                memory.max_index_lines,
                memory.max_index_bytes,
                memory.user_max_chars,
            )
            .with_db(Arc::clone(db)),
        ));
    }
    // 下载与 WebFetch 共用同一套域名 / 体积限制。
    let download_policy = NetPolicy {
        allowed_domains: config.tools.download.allowed_domains.clone(),
        blocked_domains: config.tools.download.blocked_domains.clone(),
        max_bytes: config.tools.download.max_bytes,
        ..NetPolicy::default()
    };
    match DownloadTool::new(Arc::clone(vfs), download_policy.clone()) {
        Ok(tool) => tools.register(Arc::new(tool)),
        Err(error) => warn!(%error, "cannot register Download"),
    }
    let server_tools = register_web_tools(
        &mut tools,
        config,
        assembly.auth,
        assembly.model_ref,
        download_policy,
    );

    let approvals = ApprovalQueue::with_db(Arc::clone(db));
    let action_gate = build_action_gate(&assembly, &approvals);
    let tools = register_api(tools, &assembly, action_gate);
    ToolSet {
        tools,
        server_tools,
        hub_handle,
        approvals,
    }
}

/// 连接 `[mcp]` 里的服务端，把它们的工具挂在一个 `MCP` 工具下。
///
/// 连接失败的服务端在 `McpManager::connect` 里各自记日志并跳过；全部失败时工具仍然注册
/// （列表为空），这里只多记一条警告，不阻止启动。
async fn register_mcp(tools: &mut ToolRegistry, config: &crate::config::McpConfig) {
    let manager = Arc::new(McpManager::connect(config).await);
    let count = manager.tools().await.len();
    if count == 0 {
        warn!("MCP is enabled but no server exposed any tool");
    } else {
        info!(tools = count, "MCP tools registered");
    }
    tools.register(Arc::new(McpTool::new(manager)));
}

/// `[gate]`：所有 `napcat.*` 调用都经过它；`llm_review` 打开时用主 provider 复核。
fn build_action_gate(
    assembly: &ToolAssembly<'_>,
    approvals: &Arc<ApprovalQueue>,
) -> Arc<ActionGate> {
    let config = assembly.config;
    let mut action_gate = ActionGate::new(&config.gate, Arc::clone(approvals))
        .with_db(Arc::clone(&assembly.db))
        .with_chat_send_limit(assembly.chat_sends_per_hour);
    if config.gate.llm_review {
        action_gate = action_gate.with_reviewer(Arc::new(LlmActionReviewer::new(
            Arc::clone(&assembly.provider),
            assembly.api_model.clone(),
            std::time::Duration::from_secs(10),
        )));
    }
    Arc::new(action_gate)
}

/// 注册主代理的 `API`，并给工具表挂上子代理用的只读副本，返回最终的工具表。
///
/// 只读副本给 `docs` 与没开 `allow_send` 的自定义子代理：同一个工具名，但只允许
/// `api.list` / `api.help`，也没有发送范围。
fn register_api(
    mut tools: ToolRegistry,
    assembly: &ToolAssembly<'_>,
    action_gate: Arc<ActionGate>,
) -> Arc<ToolRegistry> {
    let config = assembly.config;
    let gateway: Option<Arc<dyn NapcatGateway>> = assembly
        .qq
        .as_ref()
        .map(|client| Arc::clone(client) as Arc<dyn NapcatGateway>);
    // 两个副本共用的构造参数；只有发送范围不同（只读副本为空）。
    let api = |send_scopes: Vec<String>| {
        ApiTool::new(
            gateway.clone(),
            Some(Arc::clone(&assembly.ops) as Arc<dyn AgentOps>),
            assembly.owner_qq,
            assembly.master_qq,
            Some(Arc::clone(&assembly.gate)),
            send_scopes,
            Some(Arc::clone(&assembly.messages)),
        )
    };
    tools.register(Arc::new(
        api(config.agent.send_scopes.clone())
            .with_account(assembly.account_qq)
            .with_action_gate(action_gate)
            .with_receive_scopes(config.agent.receive_scopes.clone())
            .with_event_hooks(config.hooks.events.clone()),
    ));
    let read_only_api = Arc::new(api(Vec::new()).read_only());
    Arc::new(tools.with_read_only_api(read_only_api))
}

/// Web 搜索 / 抓取。服务端工具由 provider 声明（Chat 协议没有），返回给 runner 放进请求；
/// 本地工具后端只在用户配置了 `base_url` 与 key 时注册，不假定官方地址；`web_fetch.local`
/// 直接抓取，不需要后端（服务端抓取同时可用时两者并存）。
fn register_web_tools(
    tools: &mut ToolRegistry,
    config: &SbotConfig,
    auth: &AuthFile,
    model_ref: crate::config::models::ModelRef<'_>,
    download_policy: NetPolicy,
) -> Vec<ServerToolKind> {
    // 配置里的 `backend` 枚举与工具层的 `WebBackend` 结构体同名，这里换个名字区分。
    use crate::config::WebBackend as Mode;
    use crate::config::models::Protocol;

    let protocol = model_ref.provider.api;
    let search = &config.tools.web_search;
    let fetch = &config.tools.web_fetch;
    // 交给 provider 执行：协议有服务端工具（不是 Chat）、模型声明了该能力、`backend` 允许。
    let on_server = |capability: &str, backend: Mode| {
        protocol != Protocol::Chat
            && model_ref.model.supports(capability)
            && matches!(backend, Mode::Auto | Mode::Server)
    };
    // 走本地工具后端：显式 `tool`，或 `auto` 且协议本身没有服务端工具。
    let on_tool_backend = |backend: Mode| {
        backend == Mode::Tool || (backend == Mode::Auto && protocol == Protocol::Chat)
    };
    let mut server_tools: Vec<ServerToolKind> = Vec::new();

    if on_server("server_tools.web_search", search.backend) {
        server_tools.push(ServerToolKind::WebSearch);
    } else if on_tool_backend(search.backend)
        && let Some(backend) = tool_backend(
            "WebSearch",
            &search.base_url,
            &auth.tools.web_search.api_key,
        )
    {
        tools.register(Arc::new(WebSearchTool::new(backend, search.count)));
    }

    let fetch_on_server = on_server("server_tools.web_fetch", fetch.backend);
    if fetch_on_server {
        server_tools.push(ServerToolKind::WebFetch);
    }
    if fetch.local && fetch.backend != Mode::Off {
        // 本地抓取不需要 base_url/key；dummy 后端只用于复用结构体。
        match WebBackend::new("http://127.0.0.1:1", "") {
            Ok(backend) => tools.register(Arc::new(
                WebFetchTool::new(Arc::new(backend), fetch.max_content_tokens, download_policy)
                    .with_local(true),
            )),
            Err(error) => warn!(%error, "cannot register local WebFetch"),
        }
    } else if !fetch_on_server
        && on_tool_backend(fetch.backend)
        && let Some(backend) =
            tool_backend("WebFetch", &fetch.base_url, &auth.tools.web_fetch.api_key)
    {
        tools.register(Arc::new(WebFetchTool::new(
            backend,
            fetch.max_content_tokens,
            download_policy,
        )));
    }
    server_tools
}

/// 本地 Web 工具的 HTTP 后端。`base_url` 为空或 key 未设置（含 `$ENV` 未定义）时返回
/// `None`，工具不注册；建 HTTP 客户端失败时记警告并返回 `None`。
fn tool_backend(
    tool: &str,
    base_url: &str,
    key: &crate::config::secret::Secret,
) -> Option<Arc<WebBackend>> {
    if base_url.trim().is_empty() {
        return None;
    }
    let key = key.value()?;
    match WebBackend::new(base_url.to_owned(), key) {
        Ok(backend) => Some(Arc::new(backend)),
        Err(error) => {
            warn!(%error, tool, "cannot register a web tool");
            None
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelsFile;

    /// 只为注册工具建一张表；VFS 不会被读写。
    fn registry() -> ToolRegistry {
        let vfs = VfsStore::open(
            Arc::new(Database::open_in_memory().unwrap()),
            Arc::new(crate::storage::crypto::MasterKey::from_bytes([3u8; 32])),
            tempfile::tempdir().unwrap().keep(),
            crate::vfs::quota::Limits::default(),
        )
        .unwrap();
        ToolRegistry::new(Arc::new(vfs), None)
    }

    /// 在 `model`（`provider/id`）上注册 Web 工具，返回（服务端工具，本地注册的 Web 工具名）。
    fn web_tools(
        model: &str,
        config: &SbotConfig,
        auth: &AuthFile,
    ) -> (Vec<ServerToolKind>, Vec<&'static str>) {
        let models = ModelsFile::embedded().unwrap();
        let model_ref = models.find(model).unwrap();
        let mut tools = registry();
        let server = register_web_tools(&mut tools, config, auth, model_ref, NetPolicy::default());
        let local = tools
            .names()
            .into_iter()
            .filter(|name| name.starts_with("Web"))
            .collect();
        (server, local)
    }

    fn keyed_auth() -> AuthFile {
        AuthFile::parse(
            r#"{"tools": {"web_search": {"api_key": "k1"}, "web_fetch": {"api_key": "k2"}}}"#,
        )
        .unwrap()
    }

    fn with_base_urls() -> SbotConfig {
        let mut config = SbotConfig::default();
        config.tools.web_search.base_url = "https://search.example".to_owned();
        config.tools.web_fetch.base_url = "https://fetch.example".to_owned();
        config
    }

    /// 声明了服务端能力的模型 + `auto`：交给 provider，不注册本地工具（即使配了 key）。
    #[test]
    fn capable_models_use_server_tools() {
        let (server, local) = web_tools(
            "anthropic/claude-opus-5-5",
            &with_base_urls(),
            &keyed_auth(),
        );
        assert_eq!(
            server,
            vec![ServerToolKind::WebSearch, ServerToolKind::WebFetch]
        );
        assert!(local.is_empty(), "{local:?}");
    }

    /// Chat 协议没有服务端工具：`auto` 走本地后端，但 `base_url` 与 key 缺一不可。
    #[test]
    fn chat_models_need_base_url_and_key() {
        let (server, local) = web_tools("deepseek/deepseek-chat", &with_base_urls(), &keyed_auth());
        assert!(server.is_empty());
        assert_eq!(local, vec!["WebFetch", "WebSearch"]);

        let (_, local) = web_tools(
            "deepseek/deepseek-chat",
            &SbotConfig::default(),
            &keyed_auth(),
        );
        assert!(local.is_empty(), "no base_url: {local:?}");

        // `$ENV` 引用的变量不存在时视为未设置 key。
        let unset = AuthFile::parse(
            r#"{"tools": {"web_search": {"api_key": "$SBOT_TEST_UNSET_WEB_KEY_7F3A"}}}"#,
        )
        .unwrap();
        let (_, local) = web_tools("deepseek/deepseek-chat", &with_base_urls(), &unset);
        assert!(local.is_empty(), "unset key: {local:?}");
    }

    /// `web_fetch.local` 不需要后端，且与服务端抓取并存；`backend = "off"` 两边都关。
    #[test]
    fn local_fetch_and_off_backend() {
        let mut config = SbotConfig::default();
        config.tools.web_fetch.local = true;
        let (server, local) = web_tools("anthropic/claude-opus-5-5", &config, &AuthFile::default());
        assert_eq!(
            server,
            vec![ServerToolKind::WebSearch, ServerToolKind::WebFetch]
        );
        assert_eq!(local, vec!["WebFetch"]);

        config.tools.web_search.backend = crate::config::WebBackend::Off;
        config.tools.web_fetch.backend = crate::config::WebBackend::Off;
        let (server, local) = web_tools("anthropic/claude-opus-5-5", &config, &keyed_auth());
        assert!(server.is_empty(), "{server:?}");
        assert!(local.is_empty(), "{local:?}");
    }
}
