//! `API` 工具：统一动作入口。
//!
//! 设计依据：设计 §5.2 / §5.6 / §5.10。
//!
//! 四个命名空间：
//! - `napcat.*`：转发 OneBot action（全部 QQ 操作），策略链见 `policy` 子模块；
//! - `agent.*`：对 agent 自己的操作（status / resume / quarantine / compact /
//!   clear_context），不产生对外影响；
//! - `image.*`：VFS 图片处理（`image_ops` 子模块），不访问 QQ；
//! - `api.*`：对注册表本身的查询（list / help）。
//!
//! 安全边界：
//! - `hold` 期间 **所有** `napcat.*` 返回 `Disabled during hold`，包括拉黑、禁言、
//!   踢人；`agent.*`、`image.*` 与 `api.*` 照常可用。
//! - 目标为 `owner_qq` / `master_qq` 的 admin/destructive action 在任何模式下都拒绝。
//! - 对外动作受 `[agent].send_scopes` 限制（fail-closed）。
//! - NapCat 只会收到 `base64://` 形式的文件内容（`outbound` 子模块），
//!   宿主机路径与内网 URL 永远不会交给它。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::ExecutionMode;
use crate::qq::event::{QqMessage, Sender};
use crate::qq::{ChatId, QqClient, format};
use crate::security::Gate;
use crate::storage::messages::MessageStore;
use crate::tools::net::NetPolicy;
use crate::transcript::ToolResult;

use super::{Permission, Tool, ToolContext, arg_str, object_schema};
use crate::actions::{ActionSpec, Category, qq_number, specs};

mod image_ops;
mod outbound;
mod policy;

/// NapCat 网关抽象；真实实现是 [`QqClient`]，测试用 fake。
#[async_trait]
pub trait NapcatGateway: Send + Sync {
    async fn call(&self, action: &str, params: Value) -> Result<Value, String>;
}

#[async_trait]
impl NapcatGateway for QqClient {
    async fn call(&self, action: &str, params: Value) -> Result<Value, String> {
        QqClient::call(self, action, params)
            .await
            .map_err(|error| error.to_string())
    }
}

/// 对 agent 自身的操作；由 runtime 实现（压缩、清空上下文等）。
#[async_trait]
pub trait AgentOps: Send + Sync {
    async fn status(&self) -> Result<Value, String>;
    async fn resume(&self) -> Result<Value, String>;
    async fn quarantine(&self, params: &Value) -> Result<Value, String>;
    /// 压缩指定 chat；`None` 时由 runtime 选择默认会话。
    async fn compact(&self, chat: Option<crate::qq::event::ChatId>) -> Result<Value, String>;
    async fn clear_context(&self) -> Result<Value, String>;
}

/// `API` 工具。
pub struct ApiTool {
    gateway: Option<Arc<dyn NapcatGateway>>,
    agent_ops: Option<Arc<dyn AgentOps>>,
    owner_qq: u64,
    master_qq: u64,
    /// 本账号 QQ；0 表示未知（未连接 NapCat 或测试）。用来判断主人能否在
    /// `master_qq` 私聊里回复确认码：通知账号就是本账号时不能。
    account_qq: u64,
    gate: Option<Arc<Gate>>,
    /// 允许产生对外影响的 chat 列表；空表示拒绝一切对外动作。
    send_scopes: Vec<String>,
    /// 发送成功后写入 `sender = self`；没有存储时只返回结果。
    messages: Option<Arc<MessageStore>>,
    /// 发送账本 id 的进程内序号。
    counter: std::sync::atomic::AtomicU64,
    /// 只读模式：只允许 `api.*` 查询。docs 子代理使用。
    read_only: bool,
    /// `[gate]`：每次 napcat.* 调用的门禁；`None` 只用于测试。
    action_gate: Option<Arc<crate::security::action_gate::ActionGate>>,
    /// 允许读取的 chat；`None` = 不限制（测试/旧路径），`["*"]` = 全部。
    receive_scopes: Option<Vec<String>>,
    /// `[hooks.events]` 的 BeforeSend hook。
    event_hooks: Vec<crate::config::EventHookConfig>,
    /// 发送前下载 http(s) 媒体用的网络策略（只连公网）。
    media_net: NetPolicy,
}

impl ApiTool {
    pub fn new(
        gateway: Option<Arc<dyn NapcatGateway>>,
        agent_ops: Option<Arc<dyn AgentOps>>,
        owner_qq: u64,
        master_qq: u64,
        gate: Option<Arc<Gate>>,
        send_scopes: Vec<String>,
        messages: Option<Arc<MessageStore>>,
    ) -> Self {
        Self {
            gateway,
            agent_ops,
            owner_qq,
            master_qq,
            account_qq: 0,
            gate,
            send_scopes,
            messages,
            counter: std::sync::atomic::AtomicU64::new(0),
            read_only: false,
            action_gate: None,
            receive_scopes: None,
            event_hooks: Vec::new(),
            media_net: outbound::media_net_policy(),
        }
    }

    /// 告诉工具本账号 QQ（NapCat 登录信息）。
    pub(crate) fn with_account(mut self, account_qq: u64) -> Self {
        self.account_qq = account_qq;
        self
    }

    /// 测试用：替换媒体下载策略（例如允许连本地测试服务器）。
    #[cfg(test)]
    pub(crate) fn with_media_net(mut self, policy: NetPolicy) -> Self {
        self.media_net = policy;
        self
    }

    /// 接入 `[gate]`；测试不接时按 trusted 处理。
    pub fn with_action_gate(mut self, gate: Arc<crate::security::action_gate::ActionGate>) -> Self {
        self.action_gate = Some(gate);
        self
    }

    /// 限制 `napcat.*` 查询能读到哪些 chat；`["*"]` = 全部。
    pub(crate) fn with_receive_scopes(mut self, scopes: Vec<String>) -> Self {
        self.receive_scopes = Some(scopes);
        self
    }

    /// 接入消息库：`get_msg` 的 chat 反查与发送记账用。
    #[cfg(test)]
    pub(crate) fn with_messages(mut self, messages: Arc<MessageStore>) -> Self {
        self.messages = Some(messages);
        self
    }

    /// 接入 `BeforeSend` 事件 hook。
    pub(crate) fn with_event_hooks(mut self, hooks: Vec<crate::config::EventHookConfig>) -> Self {
        self.event_hooks = hooks;
        self
    }

    /// 只读副本：只暴露 `api.list` / `api.help`，不能发送或执行 QQ 动作。
    pub(crate) fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// 查表。
    fn spec(action: &str) -> Option<&'static ActionSpec> {
        specs().iter().find(|spec| spec.action == action)
    }

    /// 目标 QQ（用于 owner/master 保护）。
    fn target_qq(spec: &ActionSpec, params: &Value) -> Option<u64> {
        spec.target_field
            .and_then(|field| params.get(field))
            .and_then(qq_number)
    }

    fn scope_allowed(&self, chat: ChatId) -> bool {
        if self.send_scopes.is_empty() {
            return false;
        }
        self.send_scopes
            .iter()
            .any(|scope| scope == "*" || scope == &chat.to_string())
    }

    /// 查询类动作的 chat 目标：`group_id` 优先，其次 `user_id`。
    fn query_chat(params: &Value) -> Option<ChatId> {
        if let Some(group) = params.get("group_id").and_then(qq_number) {
            return Some(ChatId::Group(group));
        }
        params
            .get("user_id")
            .and_then(qq_number)
            .map(ChatId::Private)
    }

    fn receive_allows(&self, chat: ChatId) -> bool {
        let Some(scopes) = &self.receive_scopes else {
            return true;
        };
        let chat = chat.to_string();
        scopes.iter().any(|scope| scope == "*" || scope == &chat)
    }

    /// 规范化并清洗发送参数：字符串消息转数组，文本段剥离内部标签。
    fn sanitize_send_params(params: Value) -> Result<Value, String> {
        let mut params = params;
        if let Some(message) = params.get_mut("message") {
            if let Some(text) = message.as_str() {
                *message = json!([{ "type": "text", "data": { "text": text } }]);
            }
            if let Some(segments) = message.as_array_mut() {
                for segment in segments.iter_mut() {
                    if segment.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(text) = segment
                            .pointer("/data/text")
                            .and_then(Value::as_str)
                            .map(format::strip_internal_tags)
                    {
                        segment["data"]["text"] = json!(text);
                    }
                }
            }
        }
        Ok(params)
    }

    fn admin_target_is_principal(&self, target: Option<u64>) -> bool {
        match target {
            Some(qq) => qq != 0 && (qq == self.owner_qq || qq == self.master_qq),
            None => false,
        }
    }
}

#[async_trait]
impl Tool for ApiTool {
    fn name(&self) -> &'static str {
        "API"
    }

    fn description(&self) -> &'static str {
        "Perform a registered action. `params` must match that action's schema. There are three \
         namespaces.\n\n\
         napcat.* — QQ operations. Message segments use the array form, for example \
         [{\"type\":\"reply\",\"data\":{\"id\":98765}},{\"type\":\"text\",\"data\":{\"text\":\"hello\"}}]. \
         Do not write CQ codes. Use api.list to browse actions and api.help for one action's full \
         schema. During a hold every napcat.* call returns \
         `Disabled during hold`.\n\n\
         agent.* — operations on yourself; these never reach QQ and are always available: \
         status, resume, quarantine, compact (optional `chat`), clear_context.\n\n\
         api.* — read-only registry queries: api.list (namespace/category/query/offset/limit) and \
         api.help (action).\n\n\
         Sends return a message_id. Do not retry a send without checking whether it went out."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "action": {"type": "string", "description": "Namespaced action, e.g. napcat.send_group_msg or agent.status."},
                "params": {"type": "object", "description": "Action parameters; see api.help."}
            }),
            &["action"],
        )
    }

    fn permissions(&self) -> &'static [Permission] {
        &[Permission::Send, Permission::Admin]
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let action = match arg_str(&args, "action") {
            Ok(action) => action,
            Err(error) => return error,
        };
        if self.read_only && !action.starts_with("api.") {
            return ToolResult::error(
                "This subagent is read-only: only api.list and api.help are available.",
            );
        }
        let params = args.get("params").cloned().unwrap_or_else(|| json!({}));

        // `api.*` 是元查询，直接返回，不进入 action 分派。
        if action == "api.list" {
            return self.api_list(&params);
        }
        if action == "api.help" {
            return self.api_help(&params);
        }

        let Some(spec) = Self::spec(action) else {
            let candidates = suggestions(action);
            return ToolResult::error(format!(
                "unknown_action: {action:?} is not registered.{}",
                if candidates.is_empty() {
                    " Try api.list.".to_owned()
                } else {
                    format!(" Did you mean: {}?", candidates.join(", "))
                }
            ));
        };

        match spec.namespace() {
            "agent" => self.run_agent(spec, &params, ctx).await,
            "image" => self.run_image(spec, &params, ctx),
            // napcat 的策略链顺序集中在本模块的 `policy` 子模块里。
            "napcat" => self.run_napcat(spec, params, ctx).await,
            _ => ToolResult::error(format!("Unsupported namespace in {}", spec.action)),
        }
    }
}

impl ApiTool {
    fn api_list(&self, params: &Value) -> ToolResult {
        let namespace = params.get("namespace").and_then(Value::as_str);
        let category = params
            .get("category")
            .and_then(Value::as_str)
            .and_then(Category::parse);
        let query = params
            .get("query")
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase);
        let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 100) as usize;

        let mut matching: Vec<&ActionSpec> = specs()
            .iter()
            .filter(|spec| namespace.is_none_or(|ns| spec.namespace() == ns))
            .filter(|spec| category.is_none_or(|kind| spec.category == kind))
            .filter(|spec| {
                query.as_ref().is_none_or(|query| {
                    spec.action.to_ascii_lowercase().contains(query)
                        || spec.summary.to_ascii_lowercase().contains(query)
                })
            })
            .collect();
        matching.sort_by_key(|spec| spec.action);

        let total = matching.len();
        let page: Vec<Value> = matching
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|spec| {
                json!({
                    "action": spec.action,
                    "category": spec.category.as_str(),
                    "summary": spec.summary,
                    "hold_allowed": spec.hold_allowed(),
                })
            })
            .collect();
        let next_offset = offset + page.len();
        ToolResult::ok(
            serde_json::to_string_pretty(&json!({
                "total": total,
                "offset": offset,
                "items": page,
                "next_offset": if next_offset < total { Some(next_offset) } else { None },
            }))
            .unwrap_or_default(),
        )
    }

    fn api_help(&self, params: &Value) -> ToolResult {
        let Some(action) = params.get("action").and_then(Value::as_str) else {
            return ToolResult::error("api.help requires \"action\".");
        };
        let Some(spec) = Self::spec(action) else {
            return ToolResult::error(format!("unknown_action: {action:?}"));
        };
        ToolResult::ok(
            serde_json::to_string_pretty(&json!({
                "action": spec.action,
                "category": spec.category.as_str(),
                "summary": spec.summary,
                "params": spec.params,
                "returns": spec.returns,
                "requires_owner_intent": spec.requires_owner_intent,
                "scope_field": spec.scope_field,
                "hold": if spec.hold_allowed() { "available" } else { crate::tools::HOLD_DISABLED },
                "examples": spec.examples,
            }))
            .unwrap_or_default(),
        )
    }

    async fn run_agent(&self, spec: &ActionSpec, params: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(ops) = &self.agent_ops else {
            return ToolResult::error("Agent operations are not available in this runtime.");
        };
        let result = match spec.action {
            "agent.status" => ops.status().await,
            "agent.resume" => ops.resume().await,
            "agent.quarantine" => ops.quarantine(params).await,
            "agent.compact" => {
                // `chat` 优先；否则用完当前回合的会话；都没有则交给 runtime 的默认值。
                let chat = params
                    .get("chat")
                    .and_then(Value::as_str)
                    .and_then(ChatId::parse)
                    .or_else(|| ctx.evidence.origin_chat.as_deref().and_then(ChatId::parse));
                ops.compact(chat).await
            }
            "agent.clear_context" => ops.clear_context().await,
            other => Err(format!("{other} is not implemented by this runtime")),
        };
        match result {
            Ok(value) => ToolResult::ok(
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
            ),
            Err(error) => ToolResult::error(error),
        }
    }
}

/// 未知 action 的候选。
fn suggestions(action: &str) -> Vec<String> {
    let needle = action
        .rsplit('.')
        .next()
        .unwrap_or(action)
        .to_ascii_lowercase();
    specs()
        .iter()
        .filter(|spec| spec.action.to_ascii_lowercase().contains(&needle))
        .take(5)
        .map(|spec| spec.action.to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// 注册表
// ---------------------------------------------------------------------------

/// 把发送参数还原成一条 `self` 消息，写进消息归档。
///
/// 段的解析与入站消息共用 [`crate::qq::event::parse_segment`]。`message_id` 是
/// NapCat 返回的真实 id；有它时 `message_sent` 回声会按 `(chat, message_id)` 去重，
/// 同一条消息不会存两份。没有时用负数占位 id，避免与 NapCat 的 id 冲突。
fn self_message_from_params(chat: ChatId, params: &Value, message_id: Option<i64>) -> QqMessage {
    let segments: Vec<crate::qq::event::Segment> = match params.get("message") {
        Some(Value::String(text)) => vec![crate::qq::event::Segment::Text { text: text.clone() }],
        Some(Value::Array(items)) => items.iter().map(crate::qq::event::parse_segment).collect(),
        _ => Vec::new(),
    };
    // 回复段决定 `reply_to`，与入站消息的解析规则一致。
    let reply_to = segments.iter().find_map(|segment| match segment {
        crate::qq::event::Segment::Reply { id } => id.parse().ok(),
        _ => None,
    });
    QqMessage {
        message_id: message_id.unwrap_or_else(crate::qq::event::synthetic_event_id),
        chat,
        sender: Sender {
            qq: None,
            nickname: "self".to_owned(),
            card: String::new(),
            role: String::new(),
        },
        time: chrono::Utc::now(),
        segments,
        reply_to,
        is_self: true,
        backfilled: false,
        raw: Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use image::GenericImageView;

    use crate::config::{GuardBackend, GuardConfig, GuardMode};
    use crate::vfs::Vfs;

    #[derive(Default)]
    struct FakeGateway {
        calls: Mutex<Vec<(String, Value)>>,
    }

    #[async_trait]
    impl NapcatGateway for FakeGateway {
        async fn call(&self, action: &str, params: Value) -> Result<Value, String> {
            self.calls.lock().unwrap().push((action.to_owned(), params));
            Ok(json!({"status": "ok", "retcode": 0, "data": {"message_id": 42}}))
        }
    }

    #[derive(Default)]
    struct FakeOps {
        compacted: Mutex<Vec<Option<crate::qq::event::ChatId>>>,
    }

    #[async_trait]
    impl AgentOps for FakeOps {
        async fn status(&self) -> Result<Value, String> {
            Ok(json!({"hold": false}))
        }
        async fn resume(&self) -> Result<Value, String> {
            Ok(json!({"hold": false}))
        }
        async fn quarantine(&self, params: &Value) -> Result<Value, String> {
            Ok(json!({"quarantined": params}))
        }
        async fn compact(&self, chat: Option<crate::qq::event::ChatId>) -> Result<Value, String> {
            self.compacted.lock().unwrap().push(chat);
            Ok(json!({"compacted": true}))
        }
        async fn clear_context(&self) -> Result<Value, String> {
            Ok(json!({"cleared": true}))
        }
    }

    fn temp_gate(mode: GuardMode) -> (Arc<Gate>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(Gate::from_config(
            &GuardConfig {
                enabled: true,
                mode,
                backend: GuardBackend::Off,
                threshold: 0.9,
                ..GuardConfig::default()
            },
            None,
            None,
            crate::security::gate::hold_path(dir.path()),
        ));
        (gate, dir)
    }

    fn tool(gateway: Option<Arc<dyn NapcatGateway>>, gate: Option<Arc<Gate>>) -> ApiTool {
        ApiTool::new(
            gateway,
            Some(Arc::new(FakeOps::default())),
            10000,
            10000,
            gate,
            vec!["*".to_owned()],
            None,
        )
    }

    async fn call(tool: &ApiTool, action: &str, params: Value) -> ToolResult {
        tool.execute(
            &ToolContext::new(
                Arc::new(
                    crate::vfs::sqlite::VfsStore::open(
                        Arc::new(crate::storage::Database::open_in_memory().unwrap()),
                        Arc::new(crate::storage::crypto::MasterKey::from_bytes([1u8; 32])),
                        tempfile::tempdir().unwrap().keep(),
                        crate::vfs::quota::Limits::default(),
                    )
                    .unwrap(),
                ),
                None,
            )
            .with_evidence(crate::transcript::TurnEvidence::owner("test")),
            json!({"action": action, "params": params}),
        )
        .await
    }

    fn media_vfs() -> Arc<crate::vfs::sqlite::VfsStore> {
        Arc::new(
            crate::vfs::sqlite::VfsStore::open(
                Arc::new(crate::storage::Database::open_in_memory().unwrap()),
                Arc::new(crate::storage::crypto::MasterKey::from_bytes([2u8; 32])),
                tempfile::tempdir().unwrap().keep(),
                crate::vfs::quota::Limits::default(),
            )
            .unwrap(),
        )
    }

    /// 造一张带渐变、能看出编码差异的小图。
    fn seed_image(vfs: &crate::vfs::sqlite::VfsStore, path: &str, width: u32, height: u32) {
        let image = image::RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([
                (x * 37 % 256) as u8,
                (y * 53 % 256) as u8,
                ((x + y) * 29 % 256) as u8,
                255,
            ])
        });
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let path = crate::vfs::path::VfsPath::parse(path).unwrap();
        vfs.write_binary(&path, &encoded.into_inner(), crate::vfs::NodeKind::Image)
            .unwrap();
    }

    fn load_output(vfs: &crate::vfs::sqlite::VfsStore, path: &str) -> image::DynamicImage {
        let path = crate::vfs::path::VfsPath::parse(path).unwrap();
        image::load_from_memory(&vfs.read_bytes(&path).unwrap()).unwrap()
    }

    async fn run_image(
        tool: &ApiTool,
        ctx: &ToolContext,
        action: &str,
        params: Value,
    ) -> ToolResult {
        tool.execute(ctx, json!({"action": action, "params": params}))
            .await
    }

    #[tokio::test]
    async fn api_list_filters_and_paginates() {
        let tool = tool(None, None);
        let result = call(
            &tool,
            "api.list",
            json!({"namespace": "napcat", "category": "admin", "limit": 3}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let value: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(value["items"].as_array().unwrap().len(), 3);
        assert!(value["total"].as_u64().unwrap() >= 3);
    }

    #[tokio::test]
    async fn api_help_matches_the_registry() {
        let tool = tool(None, None);
        let result = call(&tool, "api.help", json!({"action": "napcat.set_group_ban"})).await;
        assert!(!result.is_error);
        let value: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(value["action"], "napcat.set_group_ban");
        assert_eq!(value["scope_field"], "group_id");
        assert_eq!(value["params"]["user_id"]["type"], "number");

        let result = call(&tool, "api.help", json!({"action": "agent.compact"})).await;
        assert!(!result.is_error);
        let value: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(value["params"]["chat"]["type"], "string");
    }

    #[tokio::test]
    async fn hold_blocks_every_napcat_action() {
        let (gate, _dir) = temp_gate(GuardMode::Enforce);
        gate.force_hold("test");
        let gateway = Arc::new(FakeGateway::default());
        let tool = tool(Some(gateway.clone()), Some(gate));

        // 连查询与拉黑都被拒绝：设计 §5.10 明确“全部拒绝”。
        for (action, params) in [
            ("napcat.get_login_info", json!({})),
            (
                "napcat.set_group_kick",
                json!({"group_id": 1, "user_id": 2}),
            ),
        ] {
            let result = call(&tool, action, params).await;
            assert!(result.is_error);
            assert!(
                result.content.contains(crate::tools::HOLD_DISABLED),
                "{action}: {}",
                result.content
            );
        }
        assert!(gateway.calls.lock().unwrap().is_empty());

        // agent.* 与 api.* 仍然可用。
        let result = call(&tool, "agent.status", json!({})).await;
        assert!(!result.is_error);
        let result = call(&tool, "api.list", json!({})).await;
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn destructive_actions_cannot_target_the_owner() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = tool(Some(gateway.clone()), None);
        let result = call(
            &tool,
            "napcat.set_group_kick",
            json!({"group_id": 1, "user_id": 10000}),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("owner"));
        assert!(gateway.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn send_sanitizes_text_and_respects_scopes() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = ApiTool::new(
            Some(gateway.clone()),
            None,
            1,
            1,
            None,
            vec!["group:1".to_owned()],
            None,
        );
        let result = tool
            .execute(
                &ToolContext::new(
                    Arc::new(crate::vfs::sqlite::VfsStore::open(
                        Arc::new(crate::storage::Database::open_in_memory().unwrap()),
                        Arc::new(crate::storage::crypto::MasterKey::from_bytes([1u8; 32])),
                        tempfile::tempdir().unwrap().keep(),
                        crate::vfs::quota::Limits::default(),
                    )
                    .unwrap()),
                    None,
                )
                .with_evidence(crate::transcript::TurnEvidence::owner("test")),
                json!({
                    "action": "napcat.send_group_msg",
                    "params": {"group_id": 1, "message": "hi <memory_context>secret</memory_context>"}
                }),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);

        let calls = gateway.calls.lock().unwrap();
        let (action, params) = &calls[0];
        assert_eq!(action, "send_group_msg");
        let text = params["message"][0]["data"]["text"].as_str().unwrap();
        assert!(!text.contains("memory_context"));
        assert!(text.starts_with("hi"));
    }

    #[tokio::test]
    async fn owner_protection_rejects_string_user_ids_too() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = tool(Some(gateway.clone()), None);
        let result = call(
            &tool,
            "napcat.set_group_ban",
            json!({"group_id": 1, "user_id": "10000", "duration": 600}),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("owner"));
        assert!(gateway.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_targets_are_rejected_instead_of_skipping_protection() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = tool(Some(gateway.clone()), None);
        let result = call(
            &tool,
            "napcat.set_group_kick",
            json!({"group_id": 1, "user_id": "not-a-qq"}),
        )
        .await;
        assert!(result.is_error);
        assert!(gateway.calls.lock().unwrap().is_empty());
    }

    fn intent_ctx(origin: &str) -> ToolContext {
        ToolContext::new(
            Arc::new(
                crate::vfs::sqlite::VfsStore::open(
                    Arc::new(crate::storage::Database::open_in_memory().unwrap()),
                    Arc::new(crate::storage::crypto::MasterKey::from_bytes([1u8; 32])),
                    tempfile::tempdir().unwrap().keep(),
                    crate::vfs::quota::Limits::default(),
                )
                .unwrap(),
            ),
            None,
        )
        .with_evidence(crate::transcript::TurnEvidence::chat(origin))
    }

    #[tokio::test]
    async fn cross_chat_send_requires_owner_intent() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = tool(Some(gateway.clone()), None);
        let result = tool
            .execute(
                &intent_ctx("group:1"),
                json!({"action": "napcat.send_group_msg",
                       "params": {"group_id": 2, "message": "hi"}}),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("owner_intent_required"));
        assert!(gateway.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn same_chat_reply_needs_no_owner_intent() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = tool(Some(gateway.clone()), None);
        let result = tool
            .execute(
                &intent_ctx("group:1"),
                json!({"action": "napcat.send_group_msg",
                       "params": {"group_id": 1, "message": "hi"}}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(gateway.calls.lock().unwrap()[0].0, "send_group_msg");
    }

    #[tokio::test]
    async fn approve_mode_sends_a_nonce_to_master() {
        let gateway = Arc::new(FakeGateway::default());
        let approvals = crate::security::action_gate::ApprovalQueue::new();
        let gate = Arc::new(crate::security::action_gate::ActionGate::new(
            &crate::config::GateConfig {
                mode: crate::config::GateMode::Approve,
                max_per_minute: 60,
                llm_review: false,
            },
            Arc::clone(&approvals),
        ));
        let tool = tool(Some(gateway.clone()), None).with_action_gate(Arc::clone(&gate));
        let mut context = intent_ctx("control");
        context.evidence = crate::transcript::TurnEvidence::owner("control");
        let result = tool
            .execute(
                &context,
                json!({"action": "napcat.send_group_msg",
                       "params": {"group_id": 1, "message": "hello"}}),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("approval_required"));
        let id = approvals.pending()[0]["id"].as_str().unwrap().to_owned();
        let nonce = approvals.nonce_for(&id).unwrap();
        let calls = gateway.calls.lock().unwrap();
        let notice = calls
            .iter()
            .find(|(action, _)| action == "send_private_msg")
            .expect("nonce notification");
        assert_eq!(notice.1["user_id"], 10000);
        assert!(notice.1["message"].as_str().unwrap().contains(&nonce));
    }

    #[tokio::test]
    async fn before_send_hooks_can_block() {
        let gateway = Arc::new(FakeGateway::default());
        let hook = crate::config::EventHookConfig {
            event: crate::config::HookEvent::BeforeSend,
            action: crate::config::HookAction::Block,
            reason: "no sends in this test".to_owned(),
            ..crate::config::EventHookConfig::default()
        };
        let tool = tool(Some(gateway.clone()), None).with_event_hooks(vec![hook]);
        let result = tool
            .execute(
                &intent_ctx("group:1"),
                json!({"action": "napcat.send_group_msg",
                       "params": {"group_id": 1, "message": "hello"}}),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("BeforeSend"));
        assert!(gateway.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_msg_uses_the_chat_lookup_for_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::storage::Database::open_in_memory().unwrap());
        let key = Arc::new(crate::storage::crypto::MasterKey::from_bytes([21u8; 32]));
        let messages = Arc::new(
            MessageStore::open(
                db,
                key,
                dir.path().to_path_buf(),
                0,
                crate::config::EncryptionMode::None,
            )
            .unwrap(),
        );
        let message = crate::qq::event::QqMessage {
            message_id: 77,
            chat: crate::qq::event::ChatId::Group(2),
            sender: crate::qq::event::Sender {
                qq: Some(20000),
                nickname: "Bob".to_owned(),
                card: String::new(),
                role: "member".to_owned(),
            },
            time: chrono::Utc::now(),
            segments: vec![crate::qq::event::Segment::Text {
                text: "hi".to_owned(),
            }],
            reply_to: None,
            is_self: false,
            backfilled: false,
            raw: Value::Null,
        };
        messages.append(&message).unwrap();

        let tool = tool(Some(Arc::new(FakeGateway::default())), None)
            .with_receive_scopes(vec!["group:1".to_owned()])
            .with_messages(Arc::clone(&messages));
        let denied = tool
            .execute(
                &intent_ctx("group:1"),
                json!({"action": "napcat.get_msg", "params": {"message_id": 77}}),
            )
            .await;
        assert!(denied.is_error);
        assert!(denied.content.contains("receive scope"));

        // 反查不到的消息在限制读取时同样拒绝。
        let unknown = tool
            .execute(
                &intent_ctx("group:1"),
                json!({"action": "napcat.get_msg", "params": {"message_id": 999}}),
            )
            .await;
        assert!(unknown.is_error);
    }

    #[tokio::test]
    async fn queries_respect_receive_scopes() {
        let gateway = Arc::new(FakeGateway::default());
        let tool =
            tool(Some(gateway.clone()), None).with_receive_scopes(vec!["group:1".to_owned()]);
        let denied = tool
            .execute(
                &intent_ctx("group:1"),
                json!({"action": "napcat.get_group_msg_history",
                       "params": {"group_id": 2, "count": 5}}),
            )
            .await;
        assert!(denied.is_error);
        assert!(denied.content.contains("receive scope"));

        let allowed = tool
            .execute(
                &intent_ctx("group:1"),
                json!({"action": "napcat.get_group_msg_history",
                       "params": {"group_id": 1, "count": 5}}),
            )
            .await;
        assert!(!allowed.is_error, "{}", allowed.content);
    }

    #[tokio::test]
    async fn owner_notification_chat_is_always_allowed() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = tool(Some(gateway.clone()), None);
        // master_qq = 10000；给主人的私聊是通知通道。
        let result = tool
            .execute(
                &intent_ctx("group:1"),
                json!({"action": "napcat.send_private_msg",
                       "params": {"user_id": 10000, "message": "report"}}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);

        let result = tool
            .execute(
                &intent_ctx("group:1"),
                json!({"action": "napcat.send_private_msg",
                       "params": {"user_id": 20000, "message": "hi"}}),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("owner_intent_required"));
    }

    #[tokio::test]
    async fn contraction_actions_do_not_need_owner_intent() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = tool(Some(gateway.clone()), None);
        let result = tool
            .execute(
                &intent_ctx("group:1"),
                json!({"action": "napcat.set_group_kick",
                       "params": {"group_id": 1, "user_id": 20000}}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
    }

    #[tokio::test]
    async fn group_wide_destructive_actions_require_owner_intent() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = tool(Some(gateway.clone()), None);
        for (action, params) in [
            (
                "napcat.set_group_whole_ban",
                json!({"group_id": 1, "enable": true}),
            ),
            ("napcat.set_group_leave", json!({"group_id": 1})),
            ("napcat.set_group_dismiss", json!({"group_id": 1})),
        ] {
            let result = tool
                .execute(
                    &intent_ctx("group:1"),
                    json!({"action": action, "params": params}),
                )
                .await;
            assert!(result.is_error, "{action} should require owner intent");
            assert!(result.content.contains("owner_intent_required"));
        }
        assert!(gateway.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn profile_changes_require_owner_intent() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = tool(Some(gateway.clone()), None);
        let result = tool
            .execute(
                &intent_ctx("group:1"),
                json!({"action": "napcat.set_qq_profile",
                       "params": {"nickname": "new"}}),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("owner_intent_required"));
    }

    #[tokio::test]
    async fn message_actions_without_a_chat_target_work() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = tool(Some(gateway.clone()), None);
        for (action, params) in [
            ("napcat.delete_msg", json!({"message_id": 55555})),
            (
                "napcat.set_msg_emoji_like",
                json!({"message_id": 55555, "emoji_id": "128077"}),
            ),
        ] {
            let result = call(&tool, action, params).await;
            assert!(!result.is_error, "{}: {}", action, result.content);
        }
        let calls = gateway.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "delete_msg");
        assert_eq!(calls[1].0, "set_msg_emoji_like");
    }

    #[tokio::test]
    async fn send_msg_resolves_the_group_target() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = ApiTool::new(
            Some(gateway.clone()),
            None,
            1,
            1,
            None,
            vec!["group:1".to_owned()],
            None,
        );
        let result = call(
            &tool,
            "napcat.send_msg",
            json!({"message_type": "group", "group_id": 1, "message": "hi"}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(gateway.calls.lock().unwrap()[0].0, "send_msg");

        // 发送范围外的群仍然拒绝。
        let result = call(
            &tool,
            "napcat.send_msg",
            json!({"message_type": "group", "group_id": 2, "message": "hi"}),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("send_scopes"));
    }

    #[tokio::test]
    async fn whole_group_actions_are_not_mistaken_for_user_targets() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = tool(Some(gateway.clone()), None);
        // 没有 user_id 参数；旧实现会因为“目标是 owner”而错误拒绝或错误放行。
        let result = call(
            &tool,
            "napcat.set_group_whole_ban",
            json!({"group_id": 1, "enable": true}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(gateway.calls.lock().unwrap()[0].0, "set_group_whole_ban");
    }

    #[tokio::test]
    async fn sends_to_disallowed_scopes_fail_closed() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = ApiTool::new(
            Some(gateway.clone()),
            None,
            1,
            1,
            None,
            vec!["group:1".to_owned()],
            None,
        );
        let result = call(
            &tool,
            "napcat.send_group_msg",
            json!({"group_id": 2, "message": "x"}),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("send_scopes"));
        assert!(gateway.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn read_only_api_cannot_send() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = ApiTool::new(
            Some(gateway.clone()),
            Some(Arc::new(FakeOps::default())),
            10000,
            10000,
            None,
            vec!["*".to_owned()],
            None,
        )
        .read_only();

        let result = call(
            &tool,
            "napcat.send_group_msg",
            json!({"group_id": 1, "message": "hi"}),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("read-only"));
        assert!(gateway.calls.lock().unwrap().is_empty());

        let result = call(&tool, "api.list", json!({})).await;
        assert!(!result.is_error, "{}", result.content);
    }

    #[tokio::test]
    async fn unknown_actions_return_candidates() {
        let tool = tool(None, None);
        let result = call(&tool, "napcat.set_group_banx", json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("unknown_action"));
        assert!(result.content.contains("set_group_ban"));
    }

    #[tokio::test]
    async fn image_info_and_transform_work() {
        let vfs = media_vfs();
        seed_image(&vfs, "/tmp/src.png", 16, 10);
        let tool = tool(None, None);
        let ctx = ToolContext::new(Arc::clone(&vfs), None);

        let info = run_image(&tool, &ctx, "image.info", json!({"path": "/tmp/src.png"})).await;
        assert!(!info.is_error, "{}", info.content);
        let value: Value = serde_json::from_str(&info.content).unwrap();
        assert_eq!(value["width"], 16);
        assert_eq!(value["height"], 10);

        let result = run_image(
            &tool,
            &ctx,
            "image.transform",
            json!({
                "path": "/tmp/src.png",
                "out": "/tmp/rotated.png",
                "crop": {"x": 2, "y": 1, "width": 10, "height": 8},
                "rotate": 45.0,
                "max_width": 20,
                "max_height": 20
            }),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let rotated = load_output(&vfs, "/tmp/rotated.png");
        assert!(rotated.width() > 0 && rotated.height() > 0);

        let result = run_image(
            &tool,
            &ctx,
            "image.transform",
            json!({"path": "/tmp/src.png", "out": "/tmp/resized.png", "width": 8, "height": 5}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(load_output(&vfs, "/tmp/resized.png").dimensions(), (8, 5));

        let empty = run_image(
            &tool,
            &ctx,
            "image.transform",
            json!({"path": "/tmp/src.png", "out": "/tmp/noop.png"}),
        )
        .await;
        assert!(empty.is_error);
        assert!(empty.content.contains("no transform"), "{}", empty.content);
    }

    /// 解压炸弹：文件很小、声明尺寸巨大的图在解码阶段就被拒绝，不分配像素。
    #[tokio::test]
    async fn images_beyond_the_side_limit_are_not_decoded() {
        let vfs = media_vfs();
        // 8193×1 的 PNG 只有几十 KB，但宽度超过 8192 的上限。
        seed_image(&vfs, "/tmp/wide.png", 8193, 1);
        let tool = tool(None, None);
        let ctx = ToolContext::new(Arc::clone(&vfs), None);
        let result = run_image(&tool, &ctx, "image.info", json!({"path": "/tmp/wide.png"})).await;
        assert!(result.is_error);
        assert!(
            result.content.contains("cannot decode"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn image_convert_honors_output_extension() {
        let vfs = media_vfs();
        seed_image(&vfs, "/tmp/src.png", 6, 4);
        let tool = tool(None, None);
        let ctx = ToolContext::new(Arc::clone(&vfs), None);
        for (out, expected) in [
            ("/tmp/out.png", image::ImageFormat::Png),
            ("/tmp/out.jpg", image::ImageFormat::Jpeg),
            ("/tmp/out.webp", image::ImageFormat::WebP),
        ] {
            let result = run_image(
                &tool,
                &ctx,
                "image.convert",
                json!({"path": "/tmp/src.png", "out": out, "quality": 70}),
            )
            .await;
            assert!(!result.is_error, "{}", result.content);
            let path = crate::vfs::path::VfsPath::parse(out).unwrap();
            let bytes = vfs.read_bytes(&path).unwrap();
            assert_eq!(image::guess_format(&bytes).unwrap(), expected, "{out}");
        }
        let bad = run_image(
            &tool,
            &ctx,
            "image.convert",
            json!({"path": "/tmp/src.png", "out": "/tmp/out.gif"}),
        )
        .await;
        assert!(bad.is_error);
        assert!(bad.content.contains(".png"), "{}", bad.content);
    }

    #[tokio::test]
    async fn image_text_and_fonts_work() {
        let vfs = media_vfs();
        seed_image(&vfs, "/tmp/src.png", 48, 24);
        let tool = tool(None, None);
        let ctx = ToolContext::new(Arc::clone(&vfs), None);

        let fonts = run_image(&tool, &ctx, "image.fonts", json!({"limit": 5})).await;
        assert!(!fonts.is_error, "{}", fonts.content);
        let value: Value = serde_json::from_str(&fonts.content).unwrap();
        let count = value["count"].as_u64().unwrap_or(0);

        if count > 0 {
            let family = value["fonts"][0]["family"].as_str().unwrap().to_owned();
            let result = run_image(
                &tool,
                &ctx,
                "image.text",
                json!({
                    "path": "/tmp/src.png", "out": "/tmp/text.png", "text": "Hi 42",
                    "font": family, "x": 2, "y": 2, "size": 14,
                    "color": "#ff0000", "opacity": 0.8
                }),
            )
            .await;
            assert!(!result.is_error, "{}", result.content);
            let source = load_output(&vfs, "/tmp/src.png").to_rgba8();
            let marked = load_output(&vfs, "/tmp/text.png").to_rgba8();
            assert!(
                source.pixels().zip(marked.pixels()).any(|(a, b)| a != b),
                "text did not change any pixel"
            );
        } else {
            let result = run_image(
                &tool,
                &ctx,
                "image.text",
                json!({"path": "/tmp/src.png", "out": "/tmp/text.png", "text": "Hi"}),
            )
            .await;
            assert!(result.is_error);
            assert!(result.content.contains("font"), "{}", result.content);
        }

        let bad = run_image(
            &tool,
            &ctx,
            "image.text",
            json!({"path": "/tmp/src.png", "out": "/tmp/text.png", "text": "x", "color": "red"}),
        )
        .await;
        assert!(bad.is_error);
        assert!(bad.content.contains("RRGGBB"), "{}", bad.content);
    }

    #[tokio::test]
    async fn agent_compact_prefers_chat_then_origin() {
        let ops = Arc::new(FakeOps::default());
        let tool = ApiTool::new(
            None,
            Some(Arc::clone(&ops) as Arc<dyn AgentOps>),
            1,
            1,
            None,
            Vec::new(),
            None,
        );
        let ctx = intent_ctx("group:1");

        for params in [
            json!({"chat": "group:2"}),
            json!({"chat": "not-a-chat"}),
            json!({}),
        ] {
            let result = tool
                .execute(&ctx, json!({"action": "agent.compact", "params": params}))
                .await;
            assert!(!result.is_error, "{}", result.content);
        }
        // 没有 params.chat、也没有 origin_chat 时传 None。
        let result = call(&tool, "agent.compact", json!({})).await;
        assert!(!result.is_error, "{}", result.content);

        assert_eq!(
            *ops.compacted.lock().unwrap(),
            vec![
                Some(ChatId::Group(2)),
                Some(ChatId::Group(1)),
                Some(ChatId::Group(1)),
                None,
            ]
        );
    }

    #[test]
    fn api_base64_response_becomes_an_image_attachment() {
        let images = ApiTool::images_from_response(&serde_json::json!({
            "status": "ok",
            "data": {"base64": "data:image/png;base64,aGVsbG8="}
        }));
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime, "image/png");
        assert_eq!(images[0].data_base64, "aGVsbG8=");
    }

    /// 测试用消息库（不加密）。
    fn message_store() -> (Arc<MessageStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = MessageStore::open(
            Arc::new(crate::storage::Database::open_in_memory().unwrap()),
            Arc::new(crate::storage::crypto::MasterKey::from_bytes([22u8; 32])),
            dir.path().to_path_buf(),
            0,
            crate::config::EncryptionMode::None,
        )
        .unwrap();
        (Arc::new(store), dir)
    }

    /// 一个只回一次固定内容的 HTTP 服务器，返回它的地址。
    async fn one_shot_http(body: &'static [u8]) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
        });
        address
    }

    /// 发出去的媒体只能是 base64：VFS 路径由本进程读出，http 由本进程下载。
    #[tokio::test]
    async fn outbound_media_reaches_napcat_only_as_base64() {
        use base64::Engine;
        let address = one_shot_http(b"remote-bytes").await;
        let gateway = Arc::new(FakeGateway::default());
        let vfs = media_vfs();
        seed_image(&vfs, "/tmp/a.png", 4, 4);
        let local = crate::vfs::Vfs::read_bytes(
            vfs.as_ref(),
            &crate::vfs::path::VfsPath::parse("/tmp/a.png").unwrap(),
        )
        .unwrap();
        let tool = tool(Some(gateway.clone()), None).with_media_net(NetPolicy {
            allow_private: true,
            ..outbound::media_net_policy()
        });
        let ctx = ToolContext::new(Arc::clone(&vfs), None)
            .with_evidence(crate::transcript::TurnEvidence::owner("test"));
        let result = tool
            .execute(
                &ctx,
                json!({"action": "napcat.send_group_msg", "params": {"group_id": 1, "message": [
                    {"type": "image", "data": {"file": "/tmp/a.png"}},
                    {"type": "image", "data": {"url": format!("http://{address}/x.png"),
                                               "file": "ignored-name.png"}}
                ]}}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        let calls = gateway.calls.lock().unwrap();
        let (action, params) = &calls[0];
        assert_eq!(action, "send_group_msg");
        let encode = |bytes: &[u8]| {
            format!(
                "base64://{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            )
        };
        assert_eq!(
            params["message"][0]["data"],
            json!({"file": encode(&local)})
        );
        assert_eq!(
            params["message"][1]["data"],
            json!({"file": encode(b"remote-bytes")})
        );
    }

    /// 宿主机路径永远到不了 NapCat。
    #[tokio::test]
    async fn host_file_references_never_reach_napcat() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = tool(Some(gateway.clone()), None);
        let result = tool
            .execute(
                &intent_ctx("group:1"),
                json!({"action": "napcat.send_group_msg", "params": {"group_id": 1, "message": [
                    {"type": "file", "data": {"file": "file:///root/.sbot/auth.json"}}
                ]}}),
            )
            .await;
        assert!(result.is_error);
        assert!(gateway.calls.lock().unwrap().is_empty());
    }

    /// 未声明的参数（这里是 NapCat 会当作文件读取的 `image`）在任何检查之前被拒绝。
    #[tokio::test]
    async fn undeclared_params_are_rejected_before_napcat() {
        let gateway = Arc::new(FakeGateway::default());
        let tool = tool(Some(gateway.clone()), None);
        let result = tool
            .execute(
                &intent_ctx("group:1"),
                json!({"action": "napcat.send_group_notice",
                       "params": {"group_id": 1, "content": "hi", "image": "/root/.sbot/key"}}),
            )
            .await;
        assert!(result.is_error);
        assert!(
            result.content.contains("does not accept"),
            "{}",
            result.content
        );
        assert!(gateway.calls.lock().unwrap().is_empty());
    }

    /// approve 模式的门禁（测试共用）。
    fn approve_gate() -> (
        Arc<crate::security::action_gate::ActionGate>,
        Arc<crate::security::action_gate::ApprovalQueue>,
    ) {
        let approvals = crate::security::action_gate::ApprovalQueue::new();
        let gate = Arc::new(crate::security::action_gate::ActionGate::new(
            &crate::config::GateConfig {
                mode: crate::config::GateMode::Approve,
                max_per_minute: 60,
                llm_review: false,
            },
            Arc::clone(&approvals),
        ));
        (gate, approvals)
    }

    /// 发送范围检查先于门禁：越界请求不会生成待确认项，也不会私聊主人。
    #[tokio::test]
    async fn send_scope_is_checked_before_the_approval_gate() {
        let gateway = Arc::new(FakeGateway::default());
        let (gate, approvals) = approve_gate();
        let tool = ApiTool::new(
            Some(gateway.clone()),
            None,
            10000,
            20000,
            None,
            vec!["group:1".to_owned()],
            None,
        )
        .with_action_gate(gate);
        let result = tool
            .execute(
                &intent_ctx("group:2"),
                json!({"action": "napcat.send_group_msg",
                       "params": {"group_id": 2, "message": "hi"}}),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("send_scopes"), "{}", result.content);
        assert!(approvals.pending().is_empty());
        assert!(gateway.calls.lock().unwrap().is_empty());
    }

    /// 通知账号就是本账号时，确认码回复无法批准，通知里不放确认码。
    #[tokio::test]
    async fn approval_notice_omits_the_code_when_master_is_this_account() {
        let gateway = Arc::new(FakeGateway::default());
        let (gate, approvals) = approve_gate();
        let tool = tool(Some(gateway.clone()), None)
            .with_account(10000)
            .with_action_gate(gate);
        let result = tool
            .execute(
                &intent_ctx("group:1"),
                json!({"action": "napcat.send_group_msg",
                       "params": {"group_id": 1, "message": "hi"}}),
            )
            .await;
        assert!(
            result.content.contains("approval_required"),
            "{}",
            result.content
        );
        let id = approvals.pending()[0]["id"].as_str().unwrap().to_owned();
        let nonce = approvals.nonce_for(&id).unwrap();
        let calls = gateway.calls.lock().unwrap();
        let notice = calls
            .iter()
            .find(|(action, _)| action == "send_private_msg")
            .expect("the owner is still notified");
        let text = notice.1["message"].as_str().unwrap();
        assert!(!text.contains(&nonce), "{text}");
        assert!(text.contains("sbot gate approve"), "{text}");
    }

    /// 只有带 `message` 的发送进归档，并使用 NapCat 返回的真实 message_id；
    /// 戳一戳不是消息，带 group_id 时作用于那个群。
    #[tokio::test]
    async fn only_real_messages_are_archived_with_their_napcat_id() {
        let (messages, _dir) = message_store();
        let gateway = Arc::new(FakeGateway::default());
        let tool = tool(Some(gateway.clone()), None).with_messages(Arc::clone(&messages));
        let sent = tool
            .execute(
                &intent_ctx("group:1"),
                json!({"action": "napcat.send_group_msg",
                       "params": {"group_id": 1, "message": "hi"}}),
            )
            .await;
        assert!(!sent.is_error, "{}", sent.content);
        // FakeGateway 返回 data.message_id = 42。
        assert_eq!(
            messages.chat_for_message(42).unwrap(),
            Some(crate::qq::event::ChatId::Group(1))
        );

        let poke = tool
            .execute(
                &intent_ctx("group:1"),
                json!({"action": "napcat.send_poke",
                       "params": {"group_id": 1, "user_id": 30000}}),
            )
            .await;
        assert!(!poke.is_error, "{}", poke.content);
        // 戳一戳没有生成归档消息，也不会被记到 `private:30000`。
        let group = messages
            .recent(crate::qq::event::ChatId::Group(1), 10)
            .unwrap();
        assert_eq!(group.len(), 1, "{group:?}");
        assert!(
            messages
                .recent(crate::qq::event::ChatId::Private(30000), 10)
                .unwrap()
                .is_empty()
        );
    }

    /// `send_msg` 只接受恰好一个与 message_type 匹配的目标。
    #[test]
    fn send_msg_targets_must_be_unambiguous() {
        let spec = ApiTool::spec("napcat.send_msg").unwrap();
        let chat = |params: Value| crate::actions::chat_of(spec, &params);
        assert_eq!(
            chat(json!({"message_type": "group", "group_id": 1})),
            Some(ChatId::Group(1))
        );
        assert_eq!(
            chat(json!({"message_type": "private", "user_id": 2})),
            Some(ChatId::Private(2))
        );
        assert_eq!(chat(json!({"group_id": 1})), Some(ChatId::Group(1)));
        // NapCat 在私聊对象查不到时会改发到 group_id：两者同时出现一律不认。
        assert_eq!(
            chat(json!({"message_type": "private", "user_id": 2, "group_id": 1})),
            None
        );
        assert_eq!(chat(json!({"user_id": 2, "group_id": 1})), None);
        assert_eq!(chat(json!({"message_type": "x", "group_id": 1})), None);
    }
}
