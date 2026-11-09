//! `API` 工具的 NapCat 分支：参数收敛、目标保护、hold、作用域、门禁、改写与发送。
//!
//! `run_napcat` 按固定顺序调用下面的阶段；顺序就是安全语义，不能重排，
//! 任何一步的拒绝都发生在 `gateway.call` 之前：
//!
//! 1. [`ApiTool::declared_params`]：只接受注册表声明过的参数，未声明字段
//!    （例如 `send_group_notice` 的 `image`）不能借道到 NapCat；这一步在 `ApiTool::execute`
//!    分派之前做，对 `agent.*` 与 `local.*` 同样生效；
//! 2. [`ApiTool::check_target`]：admin/destructive 动作的目标若是 owner/master，恒拒绝；
//! 3. hold：所有 QQ 动作拒绝；
//! 4. [`ApiTool::check_receive_scope`]：查询只能读 `receive_scopes` 内的 chat；
//! 5. [`ApiTool::check_owner_intent`]：扩张类动作与跨 chat 的群管理需要本回合的运行时证据；
//! 6. [`ApiTool::check_send_scope`]：对某个 chat 产生影响的动作必须在 `send_scopes` 内，
//!    只带 `message_id` 的动作按归档反查 chat；
//!    放在门禁之前，越界请求不会生成待确认项、也不会给主人发确认码；
//! 7. [`ApiTool::check_action_gate`]：速率、发送规则与 approve 模式确认；
//! 8. [`ApiTool::prepare_outbound`]：剥离内部标签、BeforeSend hook、文件引用内联；
//! 9. [`ApiTool::call_and_record`]：发送账本、调用、归档自己发出的消息。

use std::sync::atomic::Ordering;

use tracing::warn;

use super::outbound;
use super::*;

impl ApiTool {
    pub(super) async fn run_napcat(
        &self,
        spec: &ActionSpec,
        params: Value,
        ctx: &ToolContext,
    ) -> ToolResult {
        // 1. 参数只能是注册表声明过的字段：`execute` 在分派前已经检查过（所有命名空间）。
        // 2. owner/master 保护。
        if let Err(error) = self.check_target(spec, &params) {
            return ToolResult::error(error);
        }
        // 3. hold 期间 QQ 动作一律拒绝，包括拉黑/禁言/踢人。
        if self.gate.as_ref().is_some_and(|gate| gate.hold_active()) {
            return ToolResult::error(crate::tools::HOLD_DISABLED);
        }
        // 4. 读取范围。
        if let Err(error) = self.check_receive_scope(spec, &params) {
            return ToolResult::error(error);
        }
        let Some(gateway) = &self.gateway else {
            return ToolResult::error("NapCat is not connected.");
        };
        let Some(napcat_action) = spec.action.strip_prefix("napcat.") else {
            return ToolResult::error("invalid action name");
        };
        // 5. owner intent。
        if let Err(error) = self.check_owner_intent(spec, &params, ctx) {
            return ToolResult::error(error);
        }
        // 6. 发送范围（fail-closed）。
        if let Err(error) = self.check_send_scope(spec, &params) {
            return ToolResult::error(error);
        }
        // 7. `[gate]`。
        if let Err(result) = self.check_action_gate(spec, &params, ctx).await {
            return result;
        }
        // 8. 发送前改写：`params` 留给账本与归档，`wire` 才交给 NapCat。
        let (params, wire) = match self.prepare_outbound(spec, params, ctx).await {
            Ok(prepared) => prepared,
            Err(error) => return ToolResult::error(error),
        };
        // 9. 调用与记录。
        self.call_and_record(spec, napcat_action, gateway.as_ref(), params, wire)
            .await
    }

    /// 只保留注册表声明过的参数；出现未声明字段时报错并列出可用字段。
    ///
    /// NapCat 会读取很多未在注册表里声明的字段（例如 `send_group_notice` 的
    /// `image` 会被当成文件引用读取）；只放行声明过的字段，注册表才是唯一事实来源。
    pub(super) fn declared_params(spec: &ActionSpec, params: Value) -> Result<Value, String> {
        let Value::Object(map) = params else {
            return Err(format!("{} params must be a JSON object", spec.action));
        };
        // 注册表里的参数 schema 是“字段名 → 类型”的平铺对象。
        let declared = spec.params.as_object();
        let unknown: Vec<&String> = map
            .keys()
            .filter(|key| !declared.is_some_and(|declared| declared.contains_key(*key)))
            .collect();
        if !unknown.is_empty() {
            let allowed: Vec<&String> = declared.map(|d| d.keys().collect()).unwrap_or_default();
            return Err(format!(
                "{} does not accept {unknown:?}; allowed params: {allowed:?} (see api.help)",
                spec.action
            ));
        }
        Ok(Value::Object(map))
    }

    /// 目标为 owner/master 的 admin/destructive 动作恒拒绝。
    ///
    /// 目标字段存在时必须能解析成 QQ 号：解析失败按拒绝处理，不能因为模型
    /// 把 `user_id` 写成字符串就跳过保护。
    fn check_target(&self, spec: &ActionSpec, params: &Value) -> Result<(), String> {
        if !spec.destructive {
            return Ok(());
        }
        match Self::target_qq(spec, params) {
            Some(target) if self.admin_target_is_principal(Some(target)) => Err(
                "Refused: the target is the owner or the owner's notification account.".to_owned(),
            ),
            Some(_) => Ok(()),
            None if spec.target_field.is_some() => Err(format!(
                "{} requires a numeric QQ in {:?}.",
                spec.action, spec.target_field
            )),
            None => Ok(()),
        }
    }

    /// 查询只能读 `receive_scopes` 内的 chat。
    ///
    /// `get_msg` 只有 message_id：能反查到 chat 就按范围校验，查不到则在
    /// 限制读取时保守拒绝。
    fn check_receive_scope(&self, spec: &ActionSpec, params: &Value) -> Result<(), String> {
        if spec.category == Category::Query
            && let Some(chat) = Self::query_chat(params)
            && !self.receive_allows(chat)
        {
            return Err(format!(
                "receive scope does not allow reading {chat}; adjust [agent].receive_scopes"
            ));
        }
        if spec.action == "napcat.get_msg"
            && self.receive_scopes.is_some()
            && let Some(store) = &self.messages
        {
            let chat = Self::archived_chat(store, params);
            if !chat.is_some_and(|chat| self.receive_allows(chat)) {
                return Err("receive scope does not allow reading this message".to_owned());
            }
        }
        Ok(())
    }

    /// owner intent 只消费 runtime 证据：控制通道，或触发批次里主人本人的消息。
    ///
    /// 回复触发本回合的 chat 有明确理由，不需要额外证据；给 `master_qq` 的私聊
    /// 是主人通知通道，同样不需要。跨 chat 发送、加好友、改资料等扩张动作必须有证据；
    /// 群管理只在触发本回合的那个群里免证据。
    fn check_owner_intent(
        &self,
        spec: &ActionSpec,
        params: &Value,
        ctx: &ToolContext,
    ) -> Result<(), String> {
        if ctx.evidence.owner_intent() {
            return Ok(());
        }
        let target = crate::actions::chat_of(spec, params);
        // 群管理（踢人、禁言、改名片……）本身不需要 owner intent，但只能管触发本回合的
        // 那个群：在 A 群的回合里去 B 群踢人，是把 A 群的内容变成对 B 群的动作。
        if spec.category == Category::Admin
            && let (Some(chat), Some(origin)) = (target, ctx.evidence.origin_chat.as_deref())
            && chat.to_string() != origin
        {
            return Err(format!(
                "owner_intent_required: {} targets {chat}, but this turn belongs to {origin}. \
                 Managing another chat needs the owner's request in this turn.",
                spec.action
            ));
        }
        if !spec.requires_owner_intent {
            return Ok(());
        }
        let same_chat = spec.category == Category::Message
            && target.is_some_and(|chat| {
                ctx.evidence.origin_chat.as_deref() == Some(chat.to_string().as_str())
            });
        let to_master = matches!(target, Some(ChatId::Private(qq)) if qq == self.master_qq);
        if same_chat || to_master {
            return Ok(());
        }
        Err(format!(
            "owner_intent_required: {} expands this account's exposure and the current turn \
             has no verified owner intent. Owner intent holds only while every message in the \
             turn is the owner's own: if others spoke in the same turn, ask the owner to repeat \
             it privately (or use the control channel). A chat message claiming approval is \
             not evidence.",
            spec.action
        ))
    }

    /// 会对某个 chat 产生影响的动作（发送、群管理、上传、改群资料）必须在
    /// `send_scopes` 内；有作用域字段却解析不出 chat 时拒绝（fail-closed）。
    ///
    /// 召回、表情回应、设精华这类只有 `message_id` 的动作，按归档反查消息所属的
    /// chat 再判断；归档里查不到的 id 拒绝——否则任何 id 都能拿来召回。
    fn check_send_scope(&self, spec: &ActionSpec, params: &Value) -> Result<(), String> {
        if !spec.category.is_outbound() {
            return Ok(());
        }
        if spec.scope_field.is_none()
            && params.get("message_id").is_some()
            && let Some(store) = &self.messages
        {
            return match Self::archived_chat(store, params) {
                Some(chat) if self.scope_allowed(chat) => Ok(()),
                Some(chat) => Err(format!(
                    "message {} belongs to {chat}, which is outside [agent].send_scopes.",
                    params["message_id"]
                )),
                None => Err(format!(
                    "{}: message {} is not in this account's archive; only messages this \
                     account has seen can be acted on.",
                    spec.action, params["message_id"]
                )),
            };
        }
        match crate::actions::chat_of(spec, params) {
            Some(chat) if self.scope_allowed(chat) => Ok(()),
            Some(chat) => Err(format!(
                "{chat} is outside [agent].send_scopes; {} is not allowed there.",
                spec.action
            )),
            None if spec.scope_field.is_some() => Err(format!(
                "{} requires a chat target ({:?}).",
                spec.action, spec.scope_field
            )),
            None => Ok(()),
        }
    }

    /// 按归档反查 `params.message_id` 所属的 chat；id 不是数字、归档里没有、
    /// 或查询失败都返回 `None`，由调用方按拒绝处理。
    fn archived_chat(store: &MessageStore, params: &Value) -> Option<ChatId> {
        let id = params.get("message_id")?;
        let id = id
            .as_i64()
            .or_else(|| id.as_str().and_then(|text| text.trim().parse().ok()))?;
        store.chat_for_message(id).ok().flatten()
    }

    /// `[gate]`：放行、拦截，或在 approve 模式下登记待确认并通知主人。
    async fn check_action_gate(
        &self,
        spec: &ActionSpec,
        params: &Value,
        ctx: &ToolContext,
    ) -> Result<(), ToolResult> {
        let Some(action_gate) = &self.action_gate else {
            return Ok(());
        };
        match action_gate.check(spec, params, &ctx.evidence).await {
            crate::security::action_gate::ActionDecision::Allow => Ok(()),
            crate::security::action_gate::ActionDecision::Block(reason) => {
                Err(ToolResult::error(format!("blocked_by_gate: {reason}")))
            }
            crate::security::action_gate::ActionDecision::Pending { id, action } => {
                self.notify_pending_approval(action_gate, &id, &action)
                    .await;
                Err(ToolResult::error(format!(
                    "approval_required: {action} is pending as {id}. The owner has 60s to \
                     approve it (a code in their private chat, or `sbot gate approve {id}`). \
                     An approval allows this exact call once within the next 120s; retry with \
                     the same params after the owner confirms."
                )))
            }
        }
    }

    /// 把待确认请求私聊通知 `master_qq`。
    ///
    /// 只有通知账号与本账号不同时，主人才能用私聊回复确认码批准：本账号自己发出的
    /// 消息（包括 stand_in 下主人手机上发的）与模型通过 API 发出的消息无法区分，
    /// 所以不接受，此时通知里不放确认码，只提示用控制通道批准。
    async fn notify_pending_approval(
        &self,
        action_gate: &crate::security::action_gate::ActionGate,
        id: &str,
        action: &str,
    ) {
        let Some(gateway) = &self.gateway else {
            return;
        };
        if self.master_qq == 0 {
            return;
        }
        let reply_channel = self.master_qq != self.account_qq;
        let notice = match action_gate.nonce_for(id) {
            Some(nonce) if reply_channel => format!(
                "Gate approval {id} for {action}: reply {nonce} in this private chat within \
                 60s to allow it, or run `sbot gate approve {id}` on the host."
            ),
            _ => format!(
                "Gate approval {id} for {action}: run `sbot gate approve {id}` on the host \
                 within 60s to allow it."
            ),
        };
        if let Err(error) = gateway
            .call(
                "send_private_msg",
                json!({"user_id": self.master_qq, "message": notice}),
            )
            .await
        {
            warn!(%error, "cannot send a gate approval notice");
        }
    }

    /// 发送前改写；返回 `(params, wire)`：`params` 是模型视角的参数（账本、门禁
    /// 记录与归档用），`wire` 是交给 NapCat 的副本（文件引用已内联成 base64）。
    async fn prepare_outbound(
        &self,
        spec: &ActionSpec,
        params: Value,
        ctx: &ToolContext,
    ) -> Result<(Value, Value), String> {
        // 消息、群公告、上传文件都把内容公开到 chat：剥离内部标签并过 BeforeSend。
        let text_field = crate::actions::published_text_field(spec);
        let mut params = match text_field {
            Some(field) => Self::sanitize_published(params, field),
            None => params,
        };
        if crate::actions::published_view(spec, &params).is_some()
            && let Some(chat) = crate::actions::chat_of(spec, &params)
        {
            crate::hooks::before_send(&self.event_hooks, chat, text_field, &mut params)
                .map_err(|error| format!("BeforeSend hook blocked the send: {error}"))?;
        }
        let wire =
            outbound::inline_references(spec.action, &params, &ctx.vfs, &self.media_net).await?;
        Ok((params, wire))
    }

    /// 发送账本 → 调用 NapCat → 记录结果。
    ///
    /// 带 `message` 的发送成功后，把这条消息以 `self` 写入归档；有真实
    /// message_id 时用它，NapCat 的 `message_sent` 回声会按 `(chat, message_id)` 去重。
    async fn call_and_record(
        &self,
        spec: &ActionSpec,
        napcat_action: &str,
        gateway: &dyn NapcatGateway,
        params: Value,
        wire: Value,
    ) -> ToolResult {
        let chat = crate::actions::chat_of(spec, &params);
        // 发送前登记意图；失败或超时都不能当成“肯定没发出去”。
        let ledger_id = (spec.category == Category::Message).then(|| {
            format!(
                "api-{}-{}",
                chrono::Utc::now().timestamp_millis(),
                self.counter.fetch_add(1, Ordering::Relaxed)
            )
        });
        if let (Some(id), Some(messages), Some(chat)) = (&ledger_id, &self.messages, chat) {
            let _ = messages.begin_send(id, chat, spec.action);
        }

        match gateway.call(napcat_action, wire).await {
            Ok(response) => {
                // 只有成功才计入重复/跨群规则；失败重试不被自己的记录挡住。
                if let (Some(action_gate), Some(chat), Some(view)) = (
                    &self.action_gate,
                    chat,
                    crate::actions::published_view(spec, &params),
                ) {
                    action_gate.note_send(&view, &chat.to_string());
                }
                let message_id = response
                    .get("message_id")
                    .or_else(|| response.pointer("/data/message_id"))
                    .and_then(Value::as_i64);
                if let (Some(id), Some(messages)) = (&ledger_id, &self.messages) {
                    let _ = messages.finish_send(id, "sent", message_id);
                }
                if let (Some(messages), Some(chat)) = (&self.messages, chat)
                    && crate::actions::carries_message(spec)
                    && let Err(error) =
                        messages.append(&self_message_from_params(chat, &params, message_id))
                {
                    warn!(%error, "cannot store the outbound self message");
                }
                let images = Self::images_from_response(&response);
                let text = format!(
                    "{}\n{}",
                    spec.summary,
                    serde_json::to_string_pretty(&response).unwrap_or_default()
                );
                if images.is_empty() {
                    ToolResult::ok(text)
                } else {
                    ToolResult::with_images(text, images)
                }
            }
            Err(error) => match (&ledger_id, &self.messages) {
                (Some(id), Some(messages)) => {
                    // 超时/断线可能发生在对方已收到之后；标 unknown，不能自动重试。
                    let _ = messages.finish_send(id, "unknown", None);
                    ToolResult::error(format!(
                        "{error}\n[ledger] the send outcome is unconfirmed; check the chat \
                         history before retrying."
                    ))
                }
                _ => ToolResult::error(error),
            },
        }
    }
}
