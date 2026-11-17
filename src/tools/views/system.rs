//! `Views` 的 system 部分。
//!
//! 由 `views/mod.rs` 的 `list` / `read` 路由调用。`/system/<section>` 的每个 section 是下面的
//! 一个 `system_*` 方法；[`Views::read_system`] 只做分派。

use super::*;

/// `/system` 下的 section，按 `LS` 的显示顺序。新增 section 时这里与
/// [`Views::read_system`] 的分派一起改（测试检查两边一致）。
const SYSTEM_SECTIONS: [&str; 7] = [
    "status", "storage", "security", "gate", "hooks", "context", "events",
];

impl Views {
    pub(super) fn list_system(&self) -> Vec<ViewEntry> {
        SYSTEM_SECTIONS
            .into_iter()
            .map(|name| text_entry(&format!("/system/{name}")))
            .collect()
    }

    pub(super) fn read_system(&self, section: &str) -> Result<ViewRead, VfsError> {
        let config = self
            .config_snapshot()
            .ok_or_else(|| VfsError::Offline("/system".to_owned()))?;
        let text = match section {
            "status" => self.system_status(&config),
            "storage" => self.system_storage(&config),
            "security" => self.system_security(&config),
            "gate" => format!(
                "# API gate\n\nmode: {:?}\nreceive_scopes: {:?}\nsend_scopes: {:?}\n",
                config.gate.mode, config.agent.receive_scopes, config.agent.send_scopes
            ),
            "hooks" => system_hooks(&config),
            "context" => system_context(&config),
            "events" => self.system_events()?,
            _ => return Err(VfsError::NotFound(format!("/system/{section}"))),
        };
        Ok(ViewRead::Text(text))
    }

    /// `/system/status`：模式、模型、三个开关与运行时长。
    fn system_status(&self, config: &SbotConfig) -> String {
        let mut out = String::from("# Runtime\n");
        out.push_str(&format!("mode: {:?}\n", config.agent.mode));
        out.push_str(&format!("model: {}\n", config.agent.model));
        out.push_str(&format!(
            "thinking_level: {}\n",
            config.agent.thinking_level
        ));
        if let Some(enabled) = &self.context.ai_enabled {
            out.push_str(&format!(
                "ai_enabled: {}\n",
                enabled.load(Ordering::Relaxed)
            ));
        }
        if let Some(gate) = &self.context.gate {
            out.push_str(&format!("hold: {}\n", gate.hold_active()));
        }
        if let Some(estop) = &self.context.estop_file {
            out.push_str(&format!("estop: {}\n", estop.exists()));
        }
        if let Some(started) = self.context.started_at {
            out.push_str(&format!("started_at: {}\n", started.to_rfc3339()));
            out.push_str(&format!(
                "uptime_secs: {}\n",
                (Utc::now() - started).num_seconds().max(0)
            ));
        }
        out
    }

    /// `/system/storage`：VFS、消息归档与对话记录的用量，以及按年龄清理的天数。
    fn system_storage(&self, config: &SbotConfig) -> String {
        let mut out = String::from("# Storage\n");
        if let Ok(usage) = self.vfs.usage() {
            out.push_str(&format!(
                "vfs: {} active bytes, {} trash bytes, {} nodes, {} trashed\n",
                usage.active_bytes, usage.trash_bytes, usage.nodes, usage.trash_nodes
            ));
        }
        if let Ok((bytes, count)) = self.messages.usage() {
            out.push_str(&format!("messages: {bytes} bytes, {count} messages\n"));
        }
        if let Ok((bytes, count)) = self.sessions.usage() {
            out.push_str(&format!("sessions: {bytes} bytes, {count} messages\n"));
        }
        out.push_str(&format!(
            "retention: messages {} days, sessions {} days\n",
            config.storage.message_retention_days, config.storage.session_retention_days
        ));
        out
    }

    /// `/system/security`：hold、安全闸的配置与最近 10 条安全事件。
    fn system_security(&self, config: &SbotConfig) -> String {
        let mut out = format!(
            "# Security\n\nhold: {}\n",
            self.context
                .gate
                .as_ref()
                .map(|gate| gate.hold_active().to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        );
        out.push_str(&format!(
            "guard: enabled={} mode={:?} backend={:?} threshold={}\n",
            config.security.guard.enabled,
            config.security.guard.mode,
            config.security.guard.backend,
            config.security.guard.threshold
        ));
        if let Some(db) = &self.context.db
            && let Ok(events) = crate::security::events::recent(db, 10)
        {
            out.push_str("\n## Recent events\n");
            for event in events {
                out.push_str(&format!(
                    "- {} {:?} allowed={} score={:.2} {}\n",
                    event.created_at, event.source, event.allowed, event.score, event.reason
                ));
            }
        }
        out
    }

    /// `/system/events`：最近 100 条 notice / request / meta / unknown 原始事件，正文已解密。
    fn system_events(&self) -> Result<String, VfsError> {
        let offline = || VfsError::Offline("/system/events".to_owned());
        let db = self.context.db.as_ref().ok_or_else(offline)?;
        let key = self.context.key.as_ref().ok_or_else(offline)?;
        let mut out = String::from("# Event audit (newest first)\n");
        for (kind, chat, actor, sealed, created_at) in db
            .recent_events(100)
            .map_err(|error| VfsError::Storage(error.to_string()))?
        {
            let raw = key
                .open("event-audit", b"", &sealed)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_else(|_| "<cannot decrypt>".to_owned());
            out.push_str(&format!(
                "## {kind} {created_at}\nchat: {}\nactor: {}\n{raw}\n\n",
                chat.as_deref().unwrap_or("-"),
                actor
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "-".to_owned())
            ));
        }
        Ok(out)
    }
}

/// `/system/hooks`：哪些文件会被注入上下文，以及配置里的事件 hook。
///
/// 注入规则见 `crate::prompt::context`：固定注入 + agent 自管的清单。
fn system_hooks(config: &SbotConfig) -> String {
    let mut out = String::from(
        "# Context injection\n\nAlways injected when present: /AGENTS.md, /USER.md, /MEMORY.md",
    );
    if config.style.enabled {
        out.push_str(&format!(", {}", config.style.profile_path));
    }
    out.push_str(&format!(
        ", and the skill index.\nYour own list: {} (read at session start).\n\n# Event hooks\n",
        crate::prompt::context::HOOKS_PATH
    ));
    if config.hooks.events.is_empty() {
        out.push_str("(none)\n");
    }
    for hook in &config.hooks.events {
        out.push_str(&format!(
            "- {:?} -> {:?} enabled={}\n",
            hook.event, hook.action, hook.enabled
        ));
    }
    out
}

/// `/system/context`：静态提示词的估算大小与压缩相关的预算。
fn system_context(config: &SbotConfig) -> String {
    let static_prompt = crate::prompt::render_static(config.agent.mode);
    format!(
        "# Context budget\n\nstatic_prompt_tokens≈{}\nturn_context_budget: {}\n\
         keep_recent_tokens: {}\nmax_output_tokens: {}\n",
        crate::session::compaction::estimate_tokens(&static_prompt),
        config.compaction.turn_context_budget,
        config.compaction.keep_recent_tokens,
        config.agent.max_output_tokens
    )
}
