//! `Views` 的 system 部分。
//!
//! 由 `views/mod.rs` 的 `list` / `read` 路由调用。

use super::*;

impl Views {
    pub(super) fn list_system(&self) -> Vec<ViewEntry> {
        ["status", "storage", "security", "gate", "hooks", "context"]
            .into_iter()
            .map(|name| text_entry(&format!("/system/{name}")))
            .collect()
    }

    pub(super) fn read_system(&self, section: &str) -> Result<ViewRead, VfsError> {
        let config = self
            .config_snapshot()
            .ok_or_else(|| VfsError::Offline("/system".to_owned()))?;
        let text = match section {
            "status" => {
                let mut out = String::from("# Runtime\n");
                out.push_str(&format!("mode: {:?}\n", config.agent.mode));
                out.push_str(&format!("session_scope: {:?}\n", config.session.scope));
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
            "storage" => {
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
            "security" => {
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
                            event.created_at,
                            event.checkpoint,
                            event.allowed,
                            event.score,
                            event.reason
                        ));
                    }
                }
                out
            }
            "gate" => format!(
                "# API gate\n\nmode: {:?}\nreceive_scopes: {:?}\nsend_scopes: {:?}\n",
                config.gate.mode, config.agent.receive_scopes, config.agent.send_scopes
            ),
            "hooks" => {
                let mut out = String::from("# Context hooks\n");
                for hook in &config.hooks.context {
                    out.push_str(&format!(
                        "- {} path={} kind={:?} inject={:?} enabled={}\n",
                        hook.name, hook.path, hook.kind, hook.inject, hook.enabled
                    ));
                }
                out
            }
            "context" => {
                let static_prompt = crate::prompt::render_static(config.agent.mode);
                format!(
                    "# Context budget\n\nstatic_prompt_tokens≈{}\nturn_context_budget: {}\nkeep_recent_tokens: {}\nmax_output_tokens: {}\n",
                    crate::session::compaction::estimate_tokens(&static_prompt),
                    config.compaction.turn_context_budget,
                    config.compaction.keep_recent_tokens,
                    config.agent.max_output_tokens
                )
            }
            _ => return Err(VfsError::NotFound(format!("/system/{section}"))),
        };
        Ok(ViewRead::Text(text))
    }

    // ---------------------------------------------------------------------
    // 兜底
    // ---------------------------------------------------------------------
}
