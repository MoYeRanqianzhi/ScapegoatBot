//! `API` 工具的 NapCat 分支：目标保护、hold、作用域、门禁与发送。
//!
//! 这里的判定顺序就是安全语义，不能重排：任何一步的拒绝都必须发生在
//! `gateway.call` 之前。`execute` 只按 namespace 分派到本模块。

use std::sync::atomic::Ordering;

use tracing::warn;

use super::*;

impl ApiTool {
    pub(super) async fn run_napcat(
        &self,
        spec: &ActionSpec,
        params: Value,
        ctx: &ToolContext,
    ) -> ToolResult {
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

        // hold 期间一律拒绝，包括拉黑/禁言/踢人。
        if self.gate.as_ref().is_some_and(|gate| gate.hold_active()) {
            return ToolResult::error(crate::tools::HOLD_DISABLED);
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
                return ToolResult::error("receive scope does not allow reading this message");
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
            if let (Some(messages), Some(chat)) = (&self.messages, Self::chat_of(spec, &params)) {
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
                let images = Self::images_from_response(&response);
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
}
