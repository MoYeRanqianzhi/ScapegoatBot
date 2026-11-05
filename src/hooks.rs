//! 事件 hook 的发送前阶段。
//!
//! 设计依据：设计 §7.4 与 `[hooks.events]`。当前只接线 `BeforeSend`：
//! `log` 记录、`block` 拒绝、`rewrite` 改写文本段；其他事件/动作尚未
//! 接入运行时（见 `docs/guide/limits.md`）。

use regex::Regex;
use serde_json::Value;
use tracing::{info, warn};

use crate::config::{EventHookConfig, HookAction, HookEvent};
use crate::qq::event::ChatId;

fn matches(hook: &EventHookConfig, chat: ChatId, message_type: &str) -> bool {
    let spec = &hook.match_;
    if let Some(group) = spec.group_id
        && !matches!(chat, ChatId::Group(id) if id == group)
    {
        return false;
    }
    if let Some(user) = spec.user_id
        && !matches!(chat, ChatId::Private(id) if id == user)
    {
        return false;
    }
    if let Some(kind) = &spec.message_type
        && kind != message_type
    {
        return false;
    }
    // 发送路径没有工具名；要求 tool_name 的 hook 不会命中。
    if spec.tool_name.is_some() {
        return false;
    }
    true
}

/// 对发送参数依次应用 `BeforeSend` hook。
pub fn before_send(
    hooks: &[EventHookConfig],
    chat: ChatId,
    params: &mut Value,
) -> Result<(), String> {
    let message_type = if chat.is_group() { "group" } else { "private" };
    for hook in hooks.iter().filter(|hook| {
        hook.enabled
            && hook.event == HookEvent::BeforeSend
            && (hook.scope.is_empty() || hook.scope == "model" || hook.scope == "all")
    }) {
        if !matches(hook, chat, message_type) {
            continue;
        }
        match hook.action {
            HookAction::Log => {
                info!(chat = %chat, "BeforeSend hook matched");
            }
            HookAction::Block => {
                let reason = if hook.reason.trim().is_empty() {
                    "blocked by a BeforeSend hook".to_owned()
                } else {
                    hook.reason.clone()
                };
                return Err(reason);
            }
            HookAction::Rewrite => {
                rewrite(params, &hook.pattern, &hook.replacement)?;
            }
            HookAction::InjectContext | HookAction::Notify => {
                // 这两个动作对发送内容没有定义；忽略而不是猜语义。
            }
        }
    }
    Ok(())
}

fn rewrite(params: &mut Value, pattern: &str, replacement: &str) -> Result<(), String> {
    if pattern.is_empty() {
        return Err("BeforeSend rewrite hook has an empty pattern".to_owned());
    }
    let regex =
        Regex::new(pattern).map_err(|error| format!("invalid BeforeSend pattern: {error}"))?;
    let Some(message) = params.get_mut("message") else {
        return Ok(());
    };
    match message {
        Value::String(text) => {
            *text = regex.replace_all(text, replacement).into_owned();
        }
        Value::Array(segments) => {
            for segment in segments.iter_mut() {
                if segment.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                if let Some(data) = segment.get_mut("data")
                    && let Some(text) = data.get("text").and_then(Value::as_str)
                {
                    let replaced = regex.replace_all(text, replacement).into_owned();
                    data["text"] = Value::String(replaced);
                }
            }
        }
        _ => {
            warn!("BeforeSend rewrite skipped: message is neither text nor segments");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HookMatch;
    use serde_json::json;

    fn hook(action: HookAction, pattern: &str, replacement: &str) -> EventHookConfig {
        EventHookConfig {
            event: HookEvent::BeforeSend,
            action,
            match_: HookMatch::default(),
            reason: "nope".to_owned(),
            pattern: pattern.to_owned(),
            replacement: replacement.to_owned(),
            ..EventHookConfig::default()
        }
    }

    #[test]
    fn rewrite_replaces_text_segments() {
        let mut params = json!({
            "group_id": 1,
            "message": [{"type": "text", "data": {"text": "my secret token"}}]
        });
        before_send(
            &[hook(HookAction::Rewrite, "secret", "[redacted]")],
            ChatId::Group(1),
            &mut params,
        )
        .unwrap();
        assert!(
            params["message"][0]["data"]["text"]
                .as_str()
                .unwrap()
                .contains("[redacted]")
        );
    }

    #[test]
    fn block_stops_the_send() {
        let mut params = json!({"group_id": 1, "message": "hello"});
        let error = before_send(
            &[hook(HookAction::Block, "", "")],
            ChatId::Group(1),
            &mut params,
        )
        .unwrap_err();
        assert_eq!(error, "nope");
    }

    #[test]
    fn fire_collects_block_and_inject() {
        let block = hook(HookAction::Block, "", "");
        let mut inject = hook(HookAction::InjectContext, "", "");
        inject.event = HookEvent::MessageReceived;
        inject.text = "remember the rules".to_owned();
        let mut block = block;
        block.event = HookEvent::MessageReceived;
        let outcome = fire(
            &[block, inject],
            HookEvent::MessageReceived,
            Some(ChatId::Group(1)),
            Some("hi"),
            None,
        );
        assert_eq!(outcome.blocked.as_deref(), Some("nope"));
        assert_eq!(outcome.injected, vec!["remember the rules"]);
        assert!(
            injected(
                &[hook(HookAction::InjectContext, "", "")],
                HookEvent::SessionStart
            )
            .is_empty()
        );
    }

    #[test]
    fn match_filters_by_group() {
        let mut params = json!({"group_id": 2, "message": "hello"});
        let mut filtered = hook(HookAction::Block, "", "");
        filtered.match_.group_id = Some(1);
        before_send(&[filtered], ChatId::Group(2), &mut params).unwrap();
    }
}

/// 除 BeforeSend 之外的事件 hook 结果。
#[derive(Debug, Default)]
pub struct HookOutcome {
    pub blocked: Option<String>,
    pub injected: Vec<String>,
    pub notify: Vec<String>,
}

/// 触发一个事件 hook；`chat` 用于匹配群/私聊条件。
///
/// 这里只做与内容无关的动作：`log`、`block`、`inject_context`、`notify`。
/// `rewrite` 只对 BeforeSend 有定义，其他事件忽略。
pub fn fire(
    hooks: &[EventHookConfig],
    event: HookEvent,
    chat: Option<ChatId>,
    text: Option<&str>,
    tool: Option<&str>,
) -> HookOutcome {
    let mut outcome = HookOutcome::default();
    for hook in hooks
        .iter()
        .filter(|hook| hook.enabled && hook.event == event)
    {
        if let Some(chat) = chat
            && !matches(
                hook,
                chat,
                if chat.is_group() { "group" } else { "private" },
            )
        {
            continue;
        }
        if hook.match_.group_id.is_some() && chat.is_none() {
            continue;
        }
        if hook.match_.user_id.is_some() && chat.is_none() {
            continue;
        }
        if let Some(name) = &hook.match_.tool_name
            && tool != Some(name.as_str())
        {
            continue;
        }
        match hook.action {
            HookAction::Log => {
                info!(event = ?event, chat = ?chat, "event hook matched");
            }
            HookAction::Block => {
                if outcome.blocked.is_none() {
                    outcome.blocked = Some(if hook.reason.trim().is_empty() {
                        format!("blocked by a {event:?} hook")
                    } else {
                        hook.reason.clone()
                    });
                }
            }
            HookAction::InjectContext => {
                if !hook.text.trim().is_empty() {
                    outcome.injected.push(hook.text.clone());
                } else if !hook.path.trim().is_empty() {
                    // path 需要 VFS，调用方在能读盘时自行处理；这里只记录。
                    warn!(path = %hook.path, "inject_context hook path requires the caller");
                }
            }
            HookAction::Notify => {
                outcome.notify.push(if hook.notify_channel.is_empty() {
                    "owner".to_owned()
                } else {
                    hook.notify_channel.clone()
                });
            }
            HookAction::Rewrite => {}
        }
    }
    let _ = text;
    outcome
}

/// 读取某个事件配置的注入文本（用于 system prompt 重建）。
pub fn injected(hooks: &[EventHookConfig], event: HookEvent) -> Vec<String> {
    hooks
        .iter()
        .filter(|hook| {
            hook.enabled && hook.event == event && hook.action == HookAction::InjectContext
        })
        .map(|hook| hook.text.trim())
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .collect()
}
