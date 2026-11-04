//! `API` 工具：统一动作入口。
//!
//! 设计依据：设计 §5.2 / §5.6 / §5.10。
//!
//! 三个命名空间：
//! - `napcat.*`：转发 OneBot action（全部 QQ 操作）；
//! - `agent.*`：对 agent 自己的操作（status / resume / quarantine / compact /
//!   clear_context），不产生对外影响；
//! - `api.*`：对注册表本身的查询（list / help）。
//!
//! 安全边界：
//! - `hold` 期间 **所有** `napcat.*` 返回 `Disabled during hold`，包括拉黑、禁言、
//!   踢人；`agent.*` 与 `api.*` 照常可用。
//! - 目标为 `owner_qq` / `master_qq` 的 admin/destructive action 在任何模式下都拒绝。
//! - 发送受 `[agent].send_scopes` 限制（fail-closed）。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tracing::warn;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::{ExecutionMode, ToolResult};
use crate::qq::event::{QqMessage, Segment, Sender};
use crate::qq::{ChatId, QqClient, format};
use crate::security::Gate;
use crate::storage::messages::MessageStore;
use crate::vfs::Vfs;
use font8x8::UnicodeFonts;

use super::actions::{ActionSpec, Category, specs};
use super::{Permission, Tool, ToolContext, arg_str, object_schema};

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
    async fn compact(&self) -> Result<Value, String>;
    async fn clear_context(&self) -> Result<Value, String>;
}

/// `API` 工具。
pub struct ApiTool {
    gateway: Option<Arc<dyn NapcatGateway>>,
    agent_ops: Option<Arc<dyn AgentOps>>,
    owner_qq: u64,
    master_qq: u64,
    gate: Option<Arc<Gate>>,
    /// 允许发送的 scope 列表；空表示拒绝一切发送。
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
            gate,
            send_scopes,
            messages,
            counter: std::sync::atomic::AtomicU64::new(0),
            read_only: false,
            action_gate: None,
            receive_scopes: None,
            event_hooks: Vec::new(),
        }
    }

    /// 接入 `[gate]`；测试不接时按 trusted 处理。
    pub fn with_action_gate(mut self, gate: Arc<crate::security::action_gate::ActionGate>) -> Self {
        self.action_gate = Some(gate);
        self
    }

    /// 限制 `napcat.*` 查询能读到哪些 chat；`["*"]` = 全部。
    pub fn with_receive_scopes(mut self, scopes: Vec<String>) -> Self {
        self.receive_scopes = Some(scopes);
        self
    }

    /// 接入消息库：`get_msg` 的 chat 反查与发送记账用。
    pub fn with_messages(mut self, messages: Arc<MessageStore>) -> Self {
        self.messages = Some(messages);
        self
    }

    /// 接入 `BeforeSend` 事件 hook。
    pub fn with_event_hooks(mut self, hooks: Vec<crate::config::EventHookConfig>) -> Self {
        self.event_hooks = hooks;
        self
    }

    /// 只读副本：只暴露 `api.list` / `api.help`，不能发送或执行 QQ 动作。
    pub fn read_only(mut self) -> Self {
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

    /// 会话（用于发送范围）。
    pub(crate) fn chat_of(spec: &ActionSpec, params: &Value) -> Option<ChatId> {
        // `send_msg` 同时接受群与私聊，靠 message_type/参数决定目标。
        if spec.action == "napcat.send_msg" {
            let message_type = params
                .get("message_type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if message_type == "group"
                || (message_type.is_empty() && params.get("group_id").is_some())
            {
                return params
                    .get("group_id")
                    .and_then(qq_number)
                    .map(ChatId::Group);
            }
            return params
                .get("user_id")
                .and_then(qq_number)
                .map(ChatId::Private);
        }
        match spec.scope_field {
            Some("group_id") => params
                .get("group_id")
                .and_then(qq_number)
                .map(ChatId::Group),
            Some("user_id") => params
                .get("user_id")
                .and_then(qq_number)
                .map(ChatId::Private),
            _ => None,
        }
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
         status, resume, quarantine, compact, clear_context.\n\n\
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

        // 目标为 owner/master 的 admin/destructive action 恒拒绝。
        // 目标字段存在时必须能解析成 QQ 号：解析失败按拒绝处理，不能因为
        // 模型把 user_id 写成字符串就跳过保护。
        if spec.destructive
            && let Some(target) = Self::target_qq(spec, &params)
        {
            if self.admin_target_is_principal(Some(target)) {
                return ToolResult::error(
                    "Refused: the target is the owner or the owner's notification account.",
                );
            }
        } else if spec.destructive && spec.target_field.is_some() {
            return ToolResult::error(format!(
                "{} requires a numeric QQ in {:?}.",
                spec.action, spec.target_field
            ));
        }

        match spec.namespace() {
            "agent" => self.run_agent(spec, &params).await,
            "media" => self.run_media(spec, &params, ctx).await,
            "napcat" => {
                // hold 期间一律拒绝，包括拉黑/禁言/踢人。
                if self.gate.as_ref().is_some_and(|gate| gate.hold_active()) {
                    return ToolResult::error("Disabled during hold");
                }
                if spec.category == Category::Query
                    && let Some(chat) = Self::query_chat(&params)
                    && !self.receive_allows(chat)
                {
                    return ToolResult::error(format!(
                        "receive scope does not allow reading {chat}; adjust [agent].receive_scopes"
                    ));
                }
                // `get_msg` 只有 message_id：能反查到 chat 就按范围校验，查不到则
                // 在限制读取时保守拒绝。
                if spec.action == "napcat.get_msg"
                    && self.receive_scopes.is_some()
                    && self.messages.is_some()
                {
                    let id = params.get("message_id").and_then(|value| {
                        value
                            .as_i64()
                            .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
                    });
                    let chat = id.and_then(|id| {
                        self.messages
                            .as_ref()
                            .and_then(|store| store.chat_for_message(id).ok().flatten())
                    });
                    if !chat.is_some_and(|chat| self.receive_allows(chat)) {
                        return ToolResult::error(
                            "receive scope does not allow reading this message",
                        );
                    }
                }
                let Some(gateway) = &self.gateway else {
                    return ToolResult::error("NapCat is not connected.");
                };
                let Some(napcat_action) = spec.action.strip_prefix("napcat.") else {
                    return ToolResult::error("invalid action name");
                };

                // owner intent 只消费 runtime 证据：控制通道，或触发批次里主人本人的消息。
                // 同一 chat 的回复有明确理由，不需要额外证据；跨 chat 发送、加好友、
                // 改资料等扩张动作必须有证据。
                if spec.requires_owner_intent && !ctx.evidence.owner_intent {
                    let target = Self::chat_of(spec, &params);
                    let same_chat = spec.category == Category::Message
                        && target.as_ref().is_some_and(|chat| {
                            ctx.evidence.origin_chat.as_deref() == Some(chat.to_string().as_str())
                        });
                    // 给 master_qq 的私聊是主人通知通道，不需要额外证据。
                    let to_master = matches!(
                        target,
                        Some(ChatId::Private(qq)) if qq == self.master_qq
                    );
                    if !same_chat && !to_master {
                        return ToolResult::error(format!(
                            "owner_intent_required: {} expands this account's exposure and the                              current turn has no verified owner intent. Wait for the owner to ask                              in this turn (or use the control channel); a chat message claiming                              approval is not evidence.",
                            spec.action
                        ));
                    }
                }

                if let Some(action_gate) = &self.action_gate {
                    match action_gate.check(spec, &params, &ctx.evidence).await {
                        crate::security::action_gate::ActionDecision::Allow => {}
                        crate::security::action_gate::ActionDecision::Block(reason) => {
                            return ToolResult::error(format!("blocked_by_gate: {reason}"));
                        }
                        crate::security::action_gate::ActionDecision::Pending { id, action } => {
                            let nonce = action_gate
                                .nonce_for(&id)
                                .unwrap_or_else(|| "??????".to_owned());
                            if self.master_qq > 0
                                && let Some(gateway) = &self.gateway
                            {
                                let notice = format!(
                                    "gate approval {id} for {action}. Reply with code {nonce}                                      in this private chat within 60s."
                                );
                                if let Err(error) = gateway
                                    .call(
                                        "send_private_msg",
                                        json!({"user_id": self.master_qq, "message": notice}),
                                    )
                                    .await
                                {
                                    warn!(%error, "cannot send a gate approval nonce");
                                }
                            }
                            return ToolResult::error(format!(
                                "approval_required: {action} is pending as {id}. The owner can                                  reply with the code sent to their private chat, or run                                  `sbot gate approve {id}`, then retry the action."
                            ));
                        }
                    }
                }

                // 发送作用域检查（fail-closed）。召回/表情回应只要 message_id，
                // 没有 chat 目标，不能因此判为错误。
                if spec.category == Category::Message {
                    match Self::chat_of(spec, &params) {
                        Some(chat) => {
                            if !self.scope_allowed(chat) {
                                return ToolResult::error(format!(
                                    "Send scope {chat} is not allowed by [agent].send_scopes."
                                ));
                            }
                        }
                        None if spec.scope_field.is_some() => {
                            return ToolResult::error(format!(
                                "{} requires a chat target ({:?}).",
                                spec.action, spec.scope_field
                            ));
                        }
                        None => {}
                    }
                }

                let mut params = if spec.category == Category::Message {
                    match Self::sanitize_send_params(params) {
                        Ok(params) => params,
                        Err(error) => return ToolResult::error(error),
                    }
                } else {
                    params
                };
                if spec.category == Category::Message
                    && let Some(chat) = Self::chat_of(spec, &params)
                    && let Err(error) =
                        crate::agent::hooks::before_send(&self.event_hooks, chat, &mut params)
                {
                    return ToolResult::error(format!("BeforeSend hook blocked the send: {error}"));
                }

                // 发送前登记意图；成功后按 message_id 记为 self。
                let ledger_id = if spec.category == Category::Message {
                    let id = format!(
                        "api-{}-{}",
                        chrono::Utc::now().timestamp_millis(),
                        self.counter.fetch_add(1, Ordering::Relaxed)
                    );
                    if let (Some(messages), Some(chat)) =
                        (&self.messages, Self::chat_of(spec, &params))
                    {
                        let _ = messages.begin_send(&id, chat, spec.action);
                    }
                    Some(id)
                } else {
                    None
                };

                match gateway.call(napcat_action, params.clone()).await {
                    Ok(response) => {
                        // 只有成功才计入重复/跨群规则；失败重试不被自己的记录挡住。
                        if let Some(action_gate) = &self.action_gate
                            && spec.category == Category::Message
                            && let Some(chat) = Self::chat_of(spec, &params)
                        {
                            action_gate.note_send(&params, &chat.to_string());
                        }
                        if let (Some(id), Some(chat), Some(messages)) =
                            (&ledger_id, Self::chat_of(spec, &params), &self.messages)
                        {
                            let message_id = response
                                .get("message_id")
                                .or_else(|| response.pointer("/data/message_id"))
                                .and_then(Value::as_i64);
                            let _ = messages.finish_send(id, "sent", message_id);
                            if spec.category == Category::Message {
                                let self_message = self_message_from_params(chat, &params);
                                if let Err(error) = messages.append(&self_message) {
                                    warn!(%error, "cannot store the outbound self message");
                                }
                            }
                        }
                        let images = images_from_response(&response);
                        let text = format!(
                            "{}
{}",
                            spec.summary,
                            serde_json::to_string_pretty(&response).unwrap_or_default()
                        );
                        if images.is_empty() {
                            ToolResult::ok(text)
                        } else {
                            ToolResult::with_images(text, images)
                        }
                    }
                    Err(error) => {
                        if let (Some(id), Some(messages)) = (&ledger_id, &self.messages) {
                            // 超时/断线可能发生在对方已收到之后；标 unknown，
                            // 不要把它当成“肯定没发出去”再重试一次。
                            let _ = messages.finish_send(id, "unknown", None);
                        }
                        ToolResult::error(format!(
                            "{error}
[ledger] the send outcome is unconfirmed; check the chat                              history before retrying."
                        ))
                    }
                }
            }
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
                "hold": if spec.hold_allowed() { "available" } else { "Disabled during hold" },
                "examples": spec.examples,
            }))
            .unwrap_or_default(),
        )
    }

    async fn run_media(&self, spec: &ActionSpec, params: &Value, ctx: &ToolContext) -> ToolResult {
        let input = match arg_str(params, "path") {
            Ok(path) => path,
            Err(error) => return error,
        };
        let input_path = match crate::vfs::path::VfsPath::parse(input) {
            Ok(path) => path,
            Err(error) => return ToolResult::error(format!("invalid VFS path: {error}")),
        };
        let bytes = match ctx.vfs.read_bytes(&input_path) {
            Ok(bytes) => bytes,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        let image = match image::load_from_memory(&bytes) {
            Ok(image) => image,
            Err(error) => return ToolResult::error(format!("cannot decode image: {error}")),
        };
        let (width, height) = (image.width(), image.height());
        if spec.action == "media.image_info" {
            return ToolResult::ok(
                json!({
                    "path": input,
                    "width": width,
                    "height": height,
                    "bytes": bytes.len(),
                    "format": image::guess_format(&bytes).ok().map(|f| format!("{f:?}")),
                })
                .to_string(),
            );
        }
        let out = match arg_str(params, "out") {
            Ok(path) => path,
            Err(error) => return error,
        };
        let out_path = match crate::vfs::path::VfsPath::parse(out) {
            Ok(path) => path,
            Err(error) => return ToolResult::error(format!("invalid output path: {error}")),
        };
        let ext = out.to_ascii_lowercase();
        if !(ext.ends_with(".png")
            || ext.ends_with(".jpg")
            || ext.ends_with(".jpeg")
            || ext.ends_with(".webp"))
        {
            return ToolResult::error("media output must end with .png/.jpg/.jpeg/.webp");
        }
        if spec.action == "media.image_compress"
            && !(ext.ends_with(".jpg") || ext.ends_with(".jpeg"))
        {
            return ToolResult::error("media.image_compress requires a .jpg/.jpeg output path");
        }
        let output = match spec.action {
            "media.image_resize" => {
                let w = params.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
                let h = params.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
                if w == 0 || h == 0 || w > 8192 || h > 8192 {
                    return ToolResult::error("width and height must be between 1 and 8192");
                }
                image.resize(w, h, image::imageops::FilterType::Lanczos3)
            }
            "media.image_crop" => {
                let x = params.get("x").and_then(Value::as_u64).unwrap_or(0) as u32;
                let y = params.get("y").and_then(Value::as_u64).unwrap_or(0) as u32;
                let w = params.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
                let h = params.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
                if w == 0
                    || h == 0
                    || w > 8192
                    || h > 8192
                    || x >= width
                    || y >= height
                    || x.checked_add(w).is_none_or(|end| end > width)
                    || y.checked_add(h).is_none_or(|end| end > height)
                {
                    return ToolResult::error("crop rectangle is outside the image or too large");
                }
                image.crop_imm(x, y, w, h)
            }
            "media.image_rotate" => {
                let degrees = params.get("degrees").and_then(Value::as_u64).unwrap_or(0);
                match degrees {
                    90 => image::DynamicImage::ImageRgba8(image::imageops::rotate90(&image)),
                    180 => image::DynamicImage::ImageRgba8(image::imageops::rotate180(&image)),
                    270 => image::DynamicImage::ImageRgba8(image::imageops::rotate270(&image)),
                    _ => return ToolResult::error("degrees must be 90, 180 or 270"),
                }
            }
            "media.image_thumbnail" => {
                let w = params.get("max_width").and_then(Value::as_u64).unwrap_or(0) as u32;
                let h = params
                    .get("max_height")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32;
                if w == 0 || h == 0 || w > 8192 || h > 8192 {
                    return ToolResult::error("max_width/max_height must be 1..8192");
                }
                image.thumbnail(w, h)
            }
            "media.image_compress" => {
                let quality = params
                    .get("quality")
                    .and_then(Value::as_u64)
                    .unwrap_or(80)
                    .clamp(1, 100) as u8;
                let mut encoded = Vec::new();
                let rgb = image.to_rgb8();
                let mut encoder =
                    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, quality);
                if let Err(error) = encoder.encode_image(&rgb) {
                    return ToolResult::error(format!("cannot encode JPEG: {error}"));
                }
                if let Err(error) =
                    ctx.vfs
                        .write_binary(&out_path, &encoded, crate::vfs::NodeKind::Image)
                {
                    return ToolResult::error(format!("cannot write {out}: {error}"));
                }
                return ToolResult::ok(
                    json!({
                        "path": out,
                        "source": input,
                        "width": rgb.width(),
                        "height": rgb.height(),
                        "bytes": encoded.len(),
                        "quality": quality,
                    })
                    .to_string(),
                );
            }
            "media.image_watermark" => {
                let text = params.get("text").and_then(Value::as_str).unwrap_or("sbot");
                if !text.is_ascii() {
                    return ToolResult::error("media.image_watermark only supports ASCII text");
                }
                let mut rgba = image.to_rgba8();
                let (w, h) = rgba.dimensions();
                let scale = 2u32;
                let text_w = text.chars().count() as u32 * 8 * scale;
                let x0 = w.saturating_sub(text_w + 8);
                let y0 = h.saturating_sub(8 * scale + 8);
                for (i, ch) in text.chars().enumerate() {
                    let Some(glyph) = font8x8::BASIC_FONTS.get(ch) else {
                        continue;
                    };
                    for (row, bits) in glyph.iter().enumerate() {
                        for col in 0..8 {
                            if bits & (1 << col) == 0 {
                                continue;
                            }
                            for dy in 0..scale {
                                for dx in 0..scale {
                                    let x = x0 + i as u32 * 8 * scale + col as u32 * scale + dx;
                                    let y = y0 + row as u32 * scale + dy;
                                    if x < w && y < h {
                                        rgba.put_pixel(x, y, image::Rgba([255, 255, 255, 180]));
                                    }
                                }
                            }
                        }
                    }
                }
                image::DynamicImage::ImageRgba8(rgba)
            }
            "media.image_convert" => image,
            other => return ToolResult::error(format!("unsupported media action {other:?}")),
        };
        let format = image::ImageFormat::from_path(out).unwrap_or(image::ImageFormat::Png);
        let mut encoded = std::io::Cursor::new(Vec::new());
        if let Err(error) = output.write_to(&mut encoded, format) {
            return ToolResult::error(format!("cannot encode image: {error}"));
        }
        let encoded = encoded.into_inner();
        if let Err(error) = ctx
            .vfs
            .write_binary(&out_path, &encoded, crate::vfs::NodeKind::Image)
        {
            return ToolResult::error(format!("cannot write {out}: {error}"));
        }
        ToolResult::ok(
            json!({
                "path": out,
                "source": input,
                "width": output.width(),
                "height": output.height(),
                "bytes": encoded.len(),
            })
            .to_string(),
        )
    }

    async fn run_agent(&self, spec: &ActionSpec, params: &Value) -> ToolResult {
        let Some(ops) = &self.agent_ops else {
            return ToolResult::error("Agent operations are not available in this runtime.");
        };
        let result = match spec.action {
            "agent.status" => ops.status().await,
            "agent.resume" => ops.resume().await,
            "agent.quarantine" => ops.quarantine(params).await,
            "agent.compact" => ops.compact().await,
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

/// 把 QQ 号参数解析成整数：兼容数字与数字字符串（OneBot 两种都接受）。
///
/// 解析失败返回 `None`；调用方在保护路径上应把 `None` 当成拒绝。
fn qq_number(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_i64().and_then(|value| u64::try_from(value).ok())),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// 把发送参数还原成一条 `self` 消息，供 transcript/收件箱去重使用。
fn self_message_from_params(chat: ChatId, params: &Value) -> QqMessage {
    let segments = match params.get("message") {
        Some(Value::String(text)) => vec![Segment::Text { text: text.clone() }],
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                let kind = item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let data = item.get("data").cloned().unwrap_or(Value::Null);
                match kind {
                    "text" => Segment::Text {
                        text: data
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    },
                    "at" => Segment::At {
                        qq: data
                            .get("qq")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    },
                    "reply" => Segment::Reply {
                        id: data
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    },
                    "face" => Segment::Face {
                        id: data
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    },
                    "image" => Segment::Image {
                        url: data
                            .get("url")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        file: data
                            .get("file")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    },
                    other => Segment::Other {
                        kind: other.to_owned(),
                        data,
                    },
                }
            })
            .collect(),
        _ => Vec::new(),
    };
    QqMessage {
        // 本地占位 id；真实 message_id 在账本里。负数避免与 NapCat id 冲突。
        message_id: -chrono::Utc::now().timestamp_millis(),
        chat,
        sender: Sender {
            qq: None,
            nickname: "self".to_owned(),
            card: String::new(),
            role: String::new(),
        },
        time: chrono::Utc::now(),
        segments,
        reply_to: None,
        is_self: true,
        backfilled: false,
        raw: Value::Null,
    }
}

/// NapCat/扩展动作若直接返回 base64 图片，把它作为 tool result 的图片附件；
/// 调用方（provider）会转成 image block，和 Playwright 截图的行为一致。
fn images_from_response(response: &Value) -> Vec<crate::agent::message::ImageAttachment> {
    let data = response.get("data").unwrap_or(response);
    let mut out = Vec::new();
    for (key, mime) in [
        ("base64", "image/png"),
        ("image_base64", "image/png"),
        ("image", "image/png"),
    ] {
        let Some(raw) = data.get(key).and_then(Value::as_str) else {
            continue;
        };
        let (mime, payload) = raw
            .strip_prefix("data:")
            .and_then(|rest| rest.split_once(','))
            .map(|(header, payload)| {
                (
                    header.split(';').next().unwrap_or(mime).to_owned(),
                    payload.to_owned(),
                )
            })
            .unwrap_or_else(|| (mime.to_owned(), raw.to_owned()));
        if matches!(
            mime.as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ) && payload.len() <= 4_700_000
        {
            out.push(crate::agent::message::ImageAttachment {
                mime,
                data_base64: payload,
            });
        }
        break;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::config::{GuardBackend, GuardConfig, GuardMode};

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
    struct FakeOps;

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
        async fn compact(&self) -> Result<Value, String> {
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
            Some(Arc::new(FakeOps)),
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
            .with_evidence(crate::agent::TurnEvidence::owner("test")),
            json!({"action": action, "params": params}),
        )
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
    }

    #[tokio::test]
    async fn hold_blocks_every_napcat_action() {
        let (gate, _dir) = temp_gate(GuardMode::Enforce);
        gate.force_hold("test");
        let gateway = Arc::new(FakeGateway::default());
        let tool = tool(Some(gateway.clone()), Some(gate));

        // 连查询与拉黑都被拒绝：设计 §5.10 明确“全部拒绝”。
        for action in ["napcat.get_login_info", "napcat.set_group_kick"] {
            let result = call(&tool, action, json!({"group_id": 1, "user_id": 2})).await;
            assert!(result.is_error);
            assert!(result.content.contains("Disabled during hold"));
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
                .with_evidence(crate::agent::TurnEvidence::owner("test")),
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
        .with_evidence(crate::agent::TurnEvidence::chat(origin))
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
        context.evidence = crate::agent::TurnEvidence::owner("control");
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
            Some(Arc::new(FakeOps)),
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
    async fn media_image_info_and_resize_work() {
        let vfs = Arc::new(
            crate::vfs::sqlite::VfsStore::open(
                Arc::new(crate::storage::Database::open_in_memory().unwrap()),
                Arc::new(crate::storage::crypto::MasterKey::from_bytes([3u8; 32])),
                tempfile::tempdir().unwrap().keep(),
                crate::vfs::quota::Limits::default(),
            )
            .unwrap(),
        );
        let image = image::DynamicImage::new_rgb8(4, 2);
        let mut png = std::io::Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let input = crate::vfs::path::VfsPath::parse("/tmp/a.png").unwrap();
        vfs.write_binary(&input, &png.into_inner(), crate::vfs::NodeKind::Image)
            .unwrap();

        let tool = tool(None, None);
        let ctx = ToolContext::new(Arc::clone(&vfs), None);
        let result = tool
            .execute(
                &ctx,
                json!({"action": "media.image_info", "params": {"path": "/tmp/a.png"}}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        let info: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(info["width"], 4);
        assert_eq!(info["height"], 2);

        let result = tool
            .execute(
                &ctx,
                json!({
                    "action": "media.image_resize",
                    "params": {"path": "/tmp/a.png", "out": "/tmp/b.png", "width": 2, "height": 1}
                }),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        let out = crate::vfs::path::VfsPath::parse("/tmp/b.png").unwrap();
        assert!(vfs.stat(&out).is_ok());

        let result = tool
            .execute(
                &ctx,
                json!({
                    "action": "media.image_watermark",
                    "params": {"path": "/tmp/a.png", "out": "/tmp/wm.png", "text": "sbot"}
                }),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            vfs.stat(&crate::vfs::path::VfsPath::parse("/tmp/wm.png").unwrap())
                .is_ok()
        );
    }

    #[test]
    fn api_base64_response_becomes_an_image_attachment() {
        let images = images_from_response(&serde_json::json!({
            "status": "ok",
            "data": {"base64": "data:image/png;base64,aGVsbG8="}
        }));
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime, "image/png");
        assert_eq!(images[0].data_base64, "aGVsbG8=");
    }
}
