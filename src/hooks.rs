//! 事件 hook（`[[hooks.events]]`）的求值。
//!
//! 设计依据：设计 §7.5。可用的事件 / 动作组合由配置校验限定
//! （`config::sections::hooks::action_allowed_on`），这里按事件分三个入口：
//! - [`before_send`]：`API` 发布动作之前，`log` / `block` / `rewrite`；
//! - [`fire`]：其余事件的 `log` / `block` / `notify`（`notify` 由调用方发出）；
//! - [`injected`]：`inject_context` 的文本，`text` 原样、`path` 从 VFS 读取。
//!
//! 三个入口用同一套 `match` 规则（[`matches()`]）：要求 `group_id` / `user_id` 的 hook
//! 只在有对应 chat 的事件上命中。

use regex::Regex;
use serde_json::Value;
use tracing::{info, warn};

use crate::config::{EventHookConfig, HookAction, HookEvent};
use crate::qq::event::ChatId;

/// `inject_context` 从 `path` 读取的文本上限（字符）。
const MAX_INJECT_CHARS: usize = 8_000;

/// hook 的 `match` 是否命中这个事件；`chat` 为 `None` 的事件（会话开始、压缩等）
/// 不满足任何 chat 条件。
fn matches(hook: &EventHookConfig, chat: Option<ChatId>) -> bool {
    let spec = &hook.match_;
    if let Some(group) = spec.group_id
        && !matches!(chat, Some(ChatId::Group(id)) if id == group)
    {
        return false;
    }
    if let Some(user) = spec.user_id
        && !matches!(chat, Some(ChatId::Private(id)) if id == user)
    {
        return false;
    }
    if let Some(kind) = &spec.message_type {
        let actual = match chat {
            Some(chat) if chat.is_group() => "group",
            Some(_) => "private",
            None => return false,
        };
        if kind != actual {
            return false;
        }
    }
    true
}

/// 这个事件上启用、且 `match` 命中的 hook。
fn selected(
    hooks: &[EventHookConfig],
    event: HookEvent,
    chat: Option<ChatId>,
) -> impl Iterator<Item = &EventHookConfig> {
    hooks
        .iter()
        .filter(move |hook| hook.enabled && hook.event == event && matches(hook, chat))
}

/// 对发布动作的参数依次应用 `BeforeSend` hook。
///
/// `text_field` 是正文字段（见 [`crate::actions::published_text_field`]）：
/// `rewrite` 只改它；上传文件没有正文，传 `None`，此时 `rewrite` 不起作用，
/// `block` / `log` 照常生效。
pub fn before_send(
    hooks: &[EventHookConfig],
    chat: ChatId,
    text_field: Option<&str>,
    params: &mut Value,
) -> Result<(), String> {
    for hook in selected(hooks, HookEvent::BeforeSend, Some(chat)) {
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
                if let Some(field) = text_field {
                    rewrite(params, field, &hook.pattern, &hook.replacement)?;
                }
            }
            // 校验不允许这两个动作出现在 BeforeSend 上。
            HookAction::InjectContext | HookAction::Notify => {}
        }
    }
    Ok(())
}

/// 改写 `params[field]`：字符串整体改写，消息段数组只改文本段。
fn rewrite(
    params: &mut Value,
    field: &str,
    pattern: &str,
    replacement: &str,
) -> Result<(), String> {
    if pattern.is_empty() {
        return Err("BeforeSend rewrite hook has an empty pattern".to_owned());
    }
    let regex =
        Regex::new(pattern).map_err(|error| format!("invalid BeforeSend pattern: {error}"))?;
    let Some(message) = params.get_mut(field) else {
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
            warn!(
                field,
                "BeforeSend rewrite skipped: the field is neither text nor segments"
            );
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
            Some("message"),
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
            Some("message"),
            &mut params,
        )
        .unwrap_err();
        assert_eq!(error, "nope");
    }

    #[test]
    fn fire_collects_block_and_notify_only_when_matched() {
        let mut block = hook(HookAction::Block, "", "");
        block.event = HookEvent::MessageReceived;
        block.match_.group_id = Some(1);
        let mut silent = hook(HookAction::Notify, "", "");
        silent.event = HookEvent::SessionEnd;
        let mut owner = silent.clone();
        owner.notify_channel = "owner".to_owned();

        let outcome = fire(
            std::slice::from_ref(&block),
            HookEvent::MessageReceived,
            Some(ChatId::Group(1)),
        );
        assert_eq!(outcome.blocked.as_deref(), Some("nope"));
        // 别的群、没有 chat 的事件都不命中带 group_id 的 hook。
        assert!(
            fire(
                std::slice::from_ref(&block),
                HookEvent::MessageReceived,
                Some(ChatId::Group(2))
            )
            .blocked
            .is_none()
        );
        assert!(
            fire(
                std::slice::from_ref(&block),
                HookEvent::MessageReceived,
                None
            )
            .blocked
            .is_none()
        );
        // 没写 notify_channel 只记日志，不私聊（设计 §7.5）。
        let outcome = fire(&[silent, owner], HookEvent::SessionEnd, None);
        assert_eq!(outcome.notify, 1);
    }

    #[test]
    fn injection_reads_text_or_vfs_paths_and_honours_match() {
        use crate::vfs::Vfs;
        let dir = tempfile::tempdir().unwrap();
        let vfs = crate::vfs::sqlite::VfsStore::open(
            std::sync::Arc::new(crate::storage::Database::open_in_memory().unwrap()),
            std::sync::Arc::new(crate::storage::crypto::MasterKey::from_bytes([6u8; 32])),
            dir.path().to_path_buf(),
            crate::vfs::quota::Limits::default(),
        )
        .unwrap();
        vfs.write_text(
            &crate::vfs::VfsPath::parse("/rules.md").unwrap(),
            "from the file",
            None,
        )
        .unwrap();
        let mut text = hook(HookAction::InjectContext, "", "");
        text.event = HookEvent::MessageReceived;
        text.text = "inline rule".to_owned();
        let mut file = text.clone();
        file.text.clear();
        file.path = "/rules.md".to_owned();
        let mut missing = file.clone();
        missing.path = "/nope.md".to_owned();
        let mut other_group = text.clone();
        other_group.match_.group_id = Some(9);

        let hooks = [text, file, missing, other_group];
        let got = injected(
            &hooks,
            HookEvent::MessageReceived,
            Some(ChatId::Group(1)),
            &vfs,
        );
        assert_eq!(got, vec!["inline rule", "from the file"]);
        // 读取不标记“读过”：hook 注入不是 agent 的 Read。
        assert!(injected(&hooks, HookEvent::SessionStart, None, &vfs).is_empty());
    }

    #[test]
    fn match_filters_by_group() {
        let mut params = json!({"group_id": 2, "message": "hello"});
        let mut filtered = hook(HookAction::Block, "", "");
        filtered.match_.group_id = Some(1);
        before_send(
            &[filtered.clone()],
            ChatId::Group(2),
            Some("message"),
            &mut params,
        )
        .unwrap();
        filtered.match_.group_id = None;
        filtered.match_.message_type = Some("private".to_owned());
        before_send(&[filtered], ChatId::Group(2), Some("message"), &mut params).unwrap();
    }

    #[test]
    fn rewrite_targets_the_given_text_field_only() {
        // 群公告的正文在 `content`；上传文件没有正文，rewrite 不动文件名，block 仍生效。
        let redact = [hook(HookAction::Rewrite, "secret", "[redacted]")];
        let mut notice = json!({"group_id": 1, "content": "a secret plan"});
        before_send(&redact, ChatId::Group(1), Some("content"), &mut notice).unwrap();
        assert_eq!(notice["content"], "a [redacted] plan");

        let mut upload = json!({"group_id": 1, "file": "/data/x", "name": "secret.txt"});
        before_send(&redact, ChatId::Group(1), None, &mut upload).unwrap();
        assert_eq!(upload["name"], "secret.txt");
        let blocked = before_send(
            &[hook(HookAction::Block, "", "")],
            ChatId::Group(1),
            None,
            &mut upload,
        );
        assert!(blocked.is_err());
    }
}

/// 除 BeforeSend 之外的事件 hook 结果。
#[derive(Debug, Default)]
pub struct HookOutcome {
    /// 第一个命中的 `block` 的原因。
    pub blocked: Option<String>,
    /// 命中的 `notify_channel = "owner"`（或 `"master"`）的条数：调用方据此私聊主人一次。
    pub notify: usize,
}

/// 触发一个事件：`log` 记录、`block` 返回原因、`notify` 计数。
///
/// `inject_context` 不在这里处理（见 [`injected`]），`rewrite` 只属于 [`before_send`]。
/// 没写 `notify_channel` 的 `notify` 只记日志（设计 §7.5：私聊必须显式开启）。
pub fn fire(hooks: &[EventHookConfig], event: HookEvent, chat: Option<ChatId>) -> HookOutcome {
    let mut outcome = HookOutcome::default();
    for hook in selected(hooks, event, chat) {
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
            HookAction::Notify => {
                info!(event = ?event, chat = ?chat, "notify hook matched");
                if matches!(hook.notify_channel.as_str(), "owner" | "master") {
                    outcome.notify += 1;
                }
            }
            HookAction::InjectContext | HookAction::Rewrite => {}
        }
    }
    outcome
}

/// 这个事件上 `inject_context` 的文本：`text` 原样；`path` 从 VFS 读取（最多
/// [`MAX_INJECT_CHARS`] 字符，读不到就跳过并记日志）。读取不记作 agent 读过。
pub fn injected(
    hooks: &[EventHookConfig],
    event: HookEvent,
    chat: Option<ChatId>,
    vfs: &crate::vfs::sqlite::VfsStore,
) -> Vec<String> {
    let mut out = Vec::new();
    for hook in selected(hooks, event, chat).filter(|hook| hook.action == HookAction::InjectContext)
    {
        if !hook.text.trim().is_empty() {
            out.push(hook.text.trim().to_owned());
            continue;
        }
        let text = crate::vfs::VfsPath::parse(&hook.path)
            .map_err(|error| error.to_string())
            .and_then(|path| {
                vfs.read_text_unrecorded(&path)
                    .map_err(|error| error.to_string())
            });
        match text {
            Ok(text) if !text.trim().is_empty() => {
                let mut chars = text.chars();
                let mut kept: String = chars.by_ref().take(MAX_INJECT_CHARS).collect();
                if chars.next().is_some() {
                    kept.push_str("\n[truncated]");
                }
                out.push(kept);
            }
            Ok(_) => {}
            Err(error) => {
                warn!(path = %hook.path, %error, "inject_context hook cannot read its path")
            }
        }
    }
    out
}
