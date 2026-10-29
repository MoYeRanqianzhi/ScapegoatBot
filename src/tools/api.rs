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

use super::{Permission, Tool, ToolContext, arg_str, object_schema};

/// action 类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Message,
    Query,
    Request,
    Admin,
    File,
    Profile,
    Agent,
    Meta,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Query => "query",
            Self::Request => "request",
            Self::Admin => "admin",
            Self::File => "file",
            Self::Profile => "profile",
            Self::Agent => "agent",
            Self::Meta => "meta",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "message" => Self::Message,
            "query" => Self::Query,
            "request" => Self::Request,
            "admin" => Self::Admin,
            "file" => Self::File,
            "profile" => Self::Profile,
            "agent" => Self::Agent,
            "meta" => Self::Meta,
            _ => return None,
        })
    }

    /// 是否会产生对外影响。
    pub fn is_outbound(self) -> bool {
        matches!(
            self,
            Self::Message | Self::Request | Self::Admin | Self::File | Self::Profile
        )
    }
}

/// 一个 action 的元数据。
#[derive(Debug, Clone)]
pub struct ActionSpec {
    /// 完整名，例如 `napcat.send_group_msg`。
    pub action: &'static str,
    pub category: Category,
    pub summary: &'static str,
    /// 参数 JSON Schema。
    pub params: Value,
    /// 返回字段说明。
    pub returns: &'static str,
    /// 是否需要 owner intent（发送、请求处理、破坏性操作）。
    pub requires_owner_intent: bool,
    /// 作用域字段（用于 send_scopes 与 chat 解析）。
    pub scope_field: Option<&'static str>,
    /// 目标 QQ 字段（用于 owner/master 保护）；与 scope_field 不同。
    pub target_field: Option<&'static str>,
    /// 是否属于 admin/destructive（目标为 owner 时恒拒绝）。
    pub destructive: bool,
    pub examples: &'static [&'static str],
}

impl ActionSpec {
    pub fn namespace(&self) -> &'static str {
        self.action.split_once('.').map(|(ns, _)| ns).unwrap_or("")
    }

    /// `hold` 期间是否可用。
    pub fn hold_allowed(&self) -> bool {
        self.namespace() != "napcat"
    }
}

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

/// 对 agent 自身的操作；由 runtime 实现（Phase 5 接入压缩与重开）。
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
        }
    }

    /// 查表。
    fn spec(action: &str) -> Option<&'static ActionSpec> {
        specs().iter().find(|spec| spec.action == action)
    }

    /// 目标 QQ（用于 owner/master 保护）。
    fn target_qq(spec: &ActionSpec, params: &Value) -> Option<u64> {
        spec.target_field
            .and_then(|field| params.get(field))
            .and_then(Value::as_u64)
    }

    /// 会话（用于发送范围）。
    fn chat_of(spec: &ActionSpec, params: &Value) -> Option<ChatId> {
        match spec.scope_field {
            Some("group_id") => params
                .get("group_id")
                .and_then(Value::as_u64)
                .map(ChatId::Group),
            Some("user_id") => params
                .get("user_id")
                .and_then(Value::as_u64)
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
         schema. Every napcat.* call passes the gate, and during a hold every one of them returns \
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

    async fn execute(&self, _ctx: &ToolContext, args: Value) -> ToolResult {
        let action = match arg_str(&args, "action") {
            Ok(action) => action,
            Err(error) => return error,
        };
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
        if spec.destructive {
            let target = Self::target_qq(spec, &params);
            if self.admin_target_is_principal(target) {
                return ToolResult::error(
                    "Refused: the target is the owner or the owner's notification account.",
                );
            }
        }

        match spec.namespace() {
            "agent" => self.run_agent(spec, &params).await,
            "napcat" => {
                // hold 期间一律拒绝，包括拉黑/禁言/踢人。
                if self.gate.as_ref().is_some_and(|gate| gate.hold_active()) {
                    return ToolResult::error("Disabled during hold");
                }
                let Some(gateway) = &self.gateway else {
                    return ToolResult::error("NapCat is not connected.");
                };
                let Some(napcat_action) = spec.action.strip_prefix("napcat.") else {
                    return ToolResult::error("invalid action name");
                };

                // 发送作用域检查（fail-closed）。
                if spec.category == Category::Message {
                    let Some(chat) = Self::chat_of(spec, &params) else {
                        return ToolResult::error(format!(
                            "{} requires a chat target ({:?}).",
                            spec.action, spec.scope_field
                        ));
                    };
                    if !self.scope_allowed(chat) {
                        return ToolResult::error(format!(
                            "Send scope {chat} is not allowed by [agent].send_scopes."
                        ));
                    }
                }

                let params = if spec.category == Category::Message {
                    match Self::sanitize_send_params(params) {
                        Ok(params) => params,
                        Err(error) => return ToolResult::error(error),
                    }
                } else {
                    params
                };

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
                        ToolResult::ok(format!(
                            "{}\n{}",
                            spec.summary,
                            serde_json::to_string_pretty(&response).unwrap_or_default()
                        ))
                    }
                    Err(error) => {
                        if let (Some(id), Some(messages)) = (&ledger_id, &self.messages) {
                            let _ = messages.finish_send(id, "failed", None);
                        }
                        ToolResult::error(error)
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

fn send_spec(
    action: &'static str,
    summary: &'static str,
    scope_field: &'static str,
    params: Value,
    examples: &'static [&'static str],
) -> ActionSpec {
    ActionSpec {
        action,
        category: Category::Message,
        summary,
        params,
        returns: "OneBot response; `data.message_id` for sends.",
        requires_owner_intent: true,
        scope_field: Some(scope_field),
        target_field: if scope_field == "user_id" {
            Some("user_id")
        } else {
            None
        },
        destructive: false,
        examples,
    }
}

/// 全部内置 action。
pub fn specs() -> &'static [ActionSpec] {
    use std::sync::OnceLock;
    static SPECS: OnceLock<Vec<ActionSpec>> = OnceLock::new();
    SPECS.get_or_init(|| {
        let mut list = Vec::new();

        // ---- napcat: message ----
        list.push(send_spec(
            "napcat.send_group_msg",
            "Send a message to a group.",
            "group_id",
            json!({
                "group_id": {"type": "number"},
                "message": {"description": "Message segment array or plain string."}
            }),
            &[r#"{"group_id":123,"message":[{"type":"text","data":{"text":"hello"}}]}"#],
        ));
        list.push(send_spec(
            "napcat.send_private_msg",
            "Send a message to a friend.",
            "user_id",
            json!({
                "user_id": {"type": "number"},
                "message": {"description": "Message segment array or plain string."}
            }),
            &[r#"{"user_id":10001,"message":"hello"}"#],
        ));
        list.push(send_spec(
            "napcat.send_msg",
            "Send a message by message_type/chat target.",
            "user_id",
            json!({
                "message_type": {"type": "string"},
                "user_id": {"type": "number"},
                "group_id": {"type": "number"},
                "message": {"description": "Message segment array or plain string."}
            }),
            &[],
        ));

        list.push(ActionSpec {
            action: "napcat.delete_msg",
            category: Category::Message,
            summary: "Recall a message by id.",
            params: json!({"message_id": {"type": "number"}}),
            returns: "OneBot response.",
            requires_owner_intent: false,
            scope_field: None,
            target_field: None,
            destructive: false,
            examples: &[r#"{"message_id":55555}"#],
        });
        list.push(ActionSpec {
            action: "napcat.set_msg_emoji_like",
            category: Category::Message,
            summary: "React to a message with an emoji.",
            params: json!({"message_id": {"type": "number"}, "emoji_id": {"type": "string"}}),
            returns: "OneBot response.",
            requires_owner_intent: false,
            scope_field: None,
            target_field: None,
            destructive: false,
            examples: &[r#"{"message_id":55555,"emoji_id":"128077"}"#],
        });
        list.push(ActionSpec {
            action: "napcat.send_poke",
            category: Category::Message,
            summary: "Poke someone.",
            params: json!({"user_id": {"type": "number"}, "group_id": {"type": "number"}}),
            returns: "OneBot response.",
            requires_owner_intent: true,
            scope_field: Some("user_id"),
            target_field: Some("user_id"),
            destructive: false,
            examples: &[r#"{"user_id":10001,"group_id":123}"#],
        });

        // ---- napcat: query ----
        for (action, summary, params, example) in [
            (
                "napcat.get_friend_list",
                "List friends.",
                json!({}),
                r#"{}"#,
            ),
            (
                "napcat.get_group_list",
                "List joined groups.",
                json!({}),
                r#"{}"#,
            ),
            (
                "napcat.get_group_member_list",
                "List members of a group.",
                json!({"group_id": {"type": "number"}}),
                r#"{"group_id":123}"#,
            ),
            (
                "napcat.get_group_member_info",
                "Get one member's profile.",
                json!({"group_id": {"type": "number"}, "user_id": {"type": "number"}}),
                r#"{"group_id":123,"user_id":10001}"#,
            ),
            (
                "napcat.get_group_info",
                "Get group profile.",
                json!({"group_id": {"type": "number"}}),
                r#"{"group_id":123}"#,
            ),
            (
                "napcat.get_msg",
                "Get one message by id.",
                json!({"message_id": {"type": "number"}}),
                r#"{"message_id":55555}"#,
            ),
            (
                "napcat.get_group_msg_history",
                "Get group message history.",
                json!({"group_id": {"type": "number"}, "message_seq": {"type": "number"}, "count": {"type": "number"}}),
                r#"{"group_id":123,"count":50}"#,
            ),
            (
                "napcat.get_friend_msg_history",
                "Get private message history.",
                json!({"user_id": {"type": "number"}, "message_seq": {"type": "number"}, "count": {"type": "number"}}),
                r#"{"user_id":10001,"count":50}"#,
            ),
            (
                "napcat.get_stranger_info",
                "Get a stranger's profile.",
                json!({"user_id": {"type": "number"}}),
                r#"{"user_id":10001}"#,
            ),
            (
                "napcat.get_login_info",
                "Get the logged-in account.",
                json!({}),
                r#"{}"#,
            ),
            (
                "napcat.get_version_info",
                "Get the NapCat version.",
                json!({}),
                r#"{}"#,
            ),
            (
                "napcat.get_status",
                "Get online status.",
                json!({}),
                r#"{}"#,
            ),
        ] {
            list.push(ActionSpec {
                action,
                category: Category::Query,
                summary,
                params,
                returns: "OneBot response; see NapCat docs for data fields.",
                requires_owner_intent: false,
                scope_field: None,
                target_field: None,
                destructive: false,
                examples: Box::leak(Box::new([example])),
            });
        }

        // ---- napcat: request ----
        for (action, summary, params, example) in [
            (
                "napcat.set_friend_add_request",
                "Approve or reject a friend request.",
                json!({"flag": {"type": "string"}, "approve": {"type": "boolean"}, "remark": {"type": "string"}}),
                r#"{"flag":"f1","approve":true}"#,
            ),
            (
                "napcat.set_group_add_request",
                "Approve or reject a group request.",
                json!({"flag": {"type": "string"}, "sub_type": {"type": "string"}, "approve": {"type": "boolean"}, "reason": {"type": "string"}}),
                r#"{"flag":"f1","sub_type":"add","approve":true}"#,
            ),
        ] {
            list.push(ActionSpec {
                action,
                category: Category::Request,
                summary,
                params,
                returns: "OneBot response.",
                requires_owner_intent: true,
                scope_field: None,
                target_field: None,
                destructive: false,
                examples: Box::leak(Box::new([example])),
            });
        }

        // ---- napcat: admin / destructive ----
        for (action, summary, params, example) in [
            (
                "napcat.set_group_kick",
                "Kick a member from a group.",
                json!({"group_id": {"type": "number"}, "user_id": {"type": "number"}, "reject_add_request": {"type": "boolean"}}),
                r#"{"group_id":123,"user_id":10001}"#,
            ),
            (
                "napcat.set_group_ban",
                "Mute a member in a group.",
                json!({"group_id": {"type": "number"}, "user_id": {"type": "number"}, "duration": {"type": "number"}}),
                r#"{"group_id":123,"user_id":10001,"duration":600}"#,
            ),
            (
                "napcat.set_group_whole_ban",
                "Mute the whole group.",
                json!({"group_id": {"type": "number"}, "enable": {"type": "boolean"}}),
                r#"{"group_id":123,"enable":true}"#,
            ),
            (
                "napcat.set_group_leave",
                "Leave a group.",
                json!({"group_id": {"type": "number"}, "is_dismiss": {"type": "boolean"}}),
                r#"{"group_id":123}"#,
            ),
            (
                "napcat.set_group_dismiss",
                "Dismiss a group (owner only).",
                json!({"group_id": {"type": "number"}}),
                r#"{"group_id":123}"#,
            ),
            (
                "napcat.set_group_card",
                "Change a member's group card.",
                json!({"group_id": {"type": "number"}, "user_id": {"type": "number"}, "card": {"type": "string"}}),
                r#"{"group_id":123,"user_id":10001,"card":"new"}"#,
            ),
            (
                "napcat.set_group_name",
                "Change a group's name.",
                json!({"group_id": {"type": "number"}, "group_name": {"type": "string"}}),
                r#"{"group_id":123,"group_name":"new"}"#,
            ),
            (
                "napcat.set_group_admin",
                "Grant or revoke group admin.",
                json!({"group_id": {"type": "number"}, "user_id": {"type": "number"}, "enable": {"type": "boolean"}}),
                r#"{"group_id":123,"user_id":10001,"enable":true}"#,
            ),
            (
                "napcat.send_group_notice",
                "Publish a group notice.",
                json!({"group_id": {"type": "number"}, "content": {"type": "string"}}),
                r#"{"group_id":123,"content":"hello"}"#,
            ),
        ] {
            list.push(ActionSpec {
                action,
                category: Category::Admin,
                summary,
                params,
                returns: "OneBot response.",
                requires_owner_intent: true,
                scope_field: Some("group_id"),
                target_field: Some("user_id"),
                destructive: true,
                examples: Box::leak(Box::new([example])),
            });
        }

        // ---- napcat: file ----
        for (action, summary, params, example) in [
            (
                "napcat.upload_group_file",
                "Upload a file to a group.",
                json!({"group_id": {"type": "number"}, "file": {"type": "string"}, "name": {"type": "string"}}),
                r#"{"group_id":123,"file":"/data/x.bin","name":"x.bin"}"#,
            ),
            (
                "napcat.upload_private_file",
                "Upload a file to a friend.",
                json!({"user_id": {"type": "number"}, "file": {"type": "string"}, "name": {"type": "string"}}),
                r#"{"user_id":10001,"file":"/data/x.bin","name":"x.bin"}"#,
            ),
            (
                "napcat.get_group_file_url",
                "Get a group file download URL.",
                json!({"group_id": {"type": "number"}, "file_id": {"type": "string"}}),
                r#"{"group_id":123,"file_id":"abc"}"#,
            ),
            (
                "napcat.get_group_file_system_info",
                "Get group file system info.",
                json!({"group_id": {"type": "number"}}),
                r#"{"group_id":123}"#,
            ),
            (
                "napcat.delete_group_file",
                "Delete a group file.",
                json!({"group_id": {"type": "number"}, "file_id": {"type": "string"}}),
                r#"{"group_id":123,"file_id":"abc"}"#,
            ),
        ] {
            list.push(ActionSpec {
                action,
                category: Category::File,
                summary,
                params,
                returns: "OneBot response.",
                requires_owner_intent: true,
                scope_field: if action.contains("private") {
                    Some("user_id")
                } else {
                    Some("group_id")
                },
                target_field: if action.contains("private") {
                    Some("user_id")
                } else {
                    None
                },
                destructive: action.ends_with("delete_group_file"),
                examples: Box::leak(Box::new([example])),
            });
        }

        // ---- napcat: profile ----
        for (action, summary, params, example) in [
            (
                "napcat.set_qq_profile",
                "Update the account profile.",
                json!({"nickname": {"type": "string"}}),
                r#"{"nickname":"new"}"#,
            ),
            (
                "napcat.set_online_status",
                "Set online status.",
                json!({"status": {"type": "number"}, "ext_status": {"type": "number"}}),
                r#"{"status":10}"#,
            ),
            (
                "napcat.set_qq_avatar",
                "Change the avatar.",
                json!({"file": {"type": "string"}}),
                r#"{"file":"/data/a.png"}"#,
            ),
        ] {
            list.push(ActionSpec {
                action,
                category: Category::Profile,
                summary,
                params,
                returns: "OneBot response.",
                requires_owner_intent: true,
                scope_field: None,
                target_field: None,
                destructive: false,
                examples: Box::leak(Box::new([example])),
            });
        }

        // ---- agent ----
        for (action, summary) in [
            ("agent.status", "Hold state, recent verdicts, budget usage and scopes."),
            ("agent.resume", "Re-check the current context; clears the hold when it passes."),
            ("agent.quarantine", "Move a suspicious excerpt into /quarantine/ and mark it non-executable."),
            ("agent.compact", "Run compaction now."),
            ("agent.clear_context", "Discard the in-memory context and start a new segment."),
        ] {
            list.push(ActionSpec {
                action,
                category: Category::Agent,
                summary,
                params: if action == "agent.quarantine" {
                    json!({
                        "message_id": {"type": "number"},
                        "text": {"type": "string"},
                        "reason": {"type": "string"}
                    })
                } else {
                    json!({})
                },
                returns: "JSON object.",
                requires_owner_intent: false,
                scope_field: None,
                target_field: None,
                destructive: false,
                examples: &[],
            });
        }

        // ---- api ----
        list.push(ActionSpec {
            action: "api.list",
            category: Category::Meta,
            summary: "List registered actions, optionally filtered and paginated.",
            params: json!({
                "namespace": {"type": "string"},
                "category": {"type": "string"},
                "query": {"type": "string"},
                "offset": {"type": "number"},
                "limit": {"type": "number"}
            }),
            returns: "{total, offset, items, next_offset}.",
            requires_owner_intent: false,
            scope_field: None,
            target_field: None,
            destructive: false,
            examples: &[r#"{"namespace":"napcat","category":"admin"}"#],
        });
        list.push(ActionSpec {
            action: "api.help",
            category: Category::Meta,
            summary: "Show one action's schema, returns, gate attributes and examples.",
            params: json!({"action": {"type": "string"}}),
            returns: "JSON object.",
            requires_owner_intent: false,
            scope_field: None,
            target_field: None,
            destructive: false,
            examples: &[r#"{"action":"napcat.set_group_ban"}"#],
        });

        list
    })
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
            ),
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
                ),
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
    async fn unknown_actions_return_candidates() {
        let tool = tool(None, None);
        let result = call(&tool, "napcat.set_group_banx", json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("unknown_action"));
        assert!(result.content.contains("set_group_ban"));
    }
}
