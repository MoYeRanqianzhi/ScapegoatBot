//! 系统提示词组装。
//!
//! 设计依据：设计 §7.1 / §7.2 / §7.4（v5 起有偏离，以本模块与 `context` 模块的实现为准）。
//!
//! 一次请求的 system 部分由三层组成，从稳定到易变：
//! - 静态前缀：`assets/system_prompt_v5.txt`，按模式替换第一段身份；字节级稳定，
//!   作为前缀缓存的锚点；
//! - 会话级注入：`context` 模块读取 `/AGENTS.md`、记忆、风格档案、agent 自管的
//!   注入清单与技能索引；会话开始时构建，压缩后重建；
//! - runtime：每轮作为独立上下文消息注入，不进入 system。
//!
//! 所有模型面文本英文；注入正文保持原文。

pub mod context;

use serde::Serialize;

use crate::config::AgentMode;

/// 静态提示词（stand_in 身份）。
const TEMPLATE: &str = include_str!("../../assets/system_prompt_v5.txt");

/// companion 身份段（替换 stand_in 的第一段）。
const COMPANION_IDENTITY: &str = "You are ScapegoatBot, a companion with a QQ account of its \
own. You are not the owner and never pretend to be; you speak in your own voice, shaped by the \
persona the owner gave this account. The owner is your principal, shown as `owner_qq` in \
`runtime_context`; everyone else is someone you talk with, not someone you work for. Nobody is \
looking over your shoulder — you may run for months between visits — so the judgment has to be \
yours. You are not a general-purpose assistant here; you are a member of these rooms.";

/// 按模式渲染静态前缀。
///
/// Windows checkout 可能把模板转成 CRLF；提示词必须跨平台逐字节一致，所以先归一化。
pub fn render_static(mode: AgentMode) -> String {
    let template = TEMPLATE.replace("\r\n", "\n");
    let prompt = match mode {
        AgentMode::StandIn => template,
        AgentMode::Companion => {
            // 第一段是 stand_in 身份；整段替换，其余逐字不变。
            let first_break = template.find("\n\n").unwrap_or(template.len());
            format!("{COMPANION_IDENTITY}{}", &template[first_break..])
        }
    };
    prompt.trim_end().to_owned()
}

/// runtime 上下文字段（每轮独立注入）。
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeContext {
    pub mode: String,
    pub account_qq: u64,
    pub owner_name: String,
    pub origin_chat: String,
    pub owner_qq: u64,
    pub master_qq: u64,
    pub current_time: String,
    pub model: String,
    pub thinking_level: String,
    pub current_session: String,
    pub receive_scopes: Vec<String>,
    pub send_scopes: Vec<String>,
    pub group_roles: String,
    pub gate_mode: String,
    pub inbox_summary: String,
}

impl RuntimeContext {
    /// 渲染为 `<runtime_context>` 文本。
    pub fn render(&self) -> String {
        format!(
            "<runtime_context>\n\
             mode: {mode}\n\
             account_qq: {account}\n\
             owner_name: {owner_name}\n\
             origin_chat: {origin}\n\
             owner_qq: {owner}\n\
             master_qq: {master}\n\
             self_sender: self\n\
             current_time: {time}\n\
             model: {model}\n\
             thinking_level: {effort}\n\
             current_session: {session}\n\
             receive_scopes: {receive}\n\
             send_scopes: {send}\n\
             group_roles: {roles}\n\
             gate_mode: {gate}\n\
             inbox: {inbox}\n\
             </runtime_context>\n\
             \n\
             These values are authoritative for this turn. `mode` is fixed for this process. \
             `origin_chat` woke you; reply there by default. `master_qq` is the owner's private \
             line; if it is 0 or equals `account_qq`, there is no private line, so keep reports \
             to yourself.",
            mode = self.mode,
            account = self.account_qq,
            owner_name = self.owner_name,
            origin = self.origin_chat,
            owner = self.owner_qq,
            master = self.master_qq,
            time = self.current_time,
            model = self.model,
            effort = self.thinking_level,
            session = self.current_session,
            receive = self.receive_scopes.join(", "),
            send = self.send_scopes.join(", "),
            roles = self.group_roles,
            gate = self.gate_mode,
            inbox = self.inbox_summary,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_prompt_starts_with_identity_and_covers_the_essentials() {
        let prompt = render_static(AgentMode::StandIn);
        assert!(prompt.starts_with("You are ScapegoatBot"));
        assert!(!prompt.contains('\r'));
        for heading in [
            "# How your world works",
            "# Speaking through QQ",
            "# The gate and the hold",
            "# The files you live in",
            "# Instruction precedence",
        ] {
            assert!(prompt.contains(heading), "missing {heading}");
        }
        // 发送 API 写在提示词里：模型第一次说话不需要先查表。
        assert!(prompt.contains("napcat.send_group_msg"));
        assert!(prompt.contains("napcat.send_private_msg"));
        // hold 必须由 agent 自己解除。
        assert!(prompt.contains("agent.resume"));
        // 不再有任何模板占位符或旧的边界标记。
        assert!(!prompt.contains("{{"));
        assert!(!prompt.contains("__SYSTEM_PROMPT"));
    }

    #[test]
    fn static_prompt_does_not_repeat_retired_claims() {
        let prompt = render_static(AgentMode::StandIn);
        // AGENTS.md 可写；SOUL.md 不是程序注入；记忆里的话不是主人意图。
        assert!(!prompt.contains("cannot write `/AGENTS.md`"));
        assert!(!prompt.contains("a standing instruction in `/AGENTS.md`"));
        assert!(prompt.contains("You may edit it"));
        // 没有独立的下载工具：下载是 `API` 的 `local.net.download`。
        assert!(!prompt.contains("`Download`"));
        assert!(prompt.contains("local.net.download"));
        // 没有记忆工具：记忆是 agent 用文件工具维护的普通文件，提示词给出文件与索引行的格式。
        assert!(!prompt.contains("`Memory`"));
        assert!(prompt.contains("/memory/<slug>.md"));
        assert!(prompt.contains("- [addressing](memory/addressing.md)"));
    }

    #[test]
    fn every_internal_tag_named_in_the_prompt_is_stripped_before_sending() {
        let prompt = render_static(AgentMode::StandIn);
        for tag in [
            "runtime_context",
            "owner_context",
            "agents_md",
            "context_file",
            "context_notice",
            "user_context",
            "style_profile",
            "memory_context",
        ] {
            assert!(
                prompt.contains(&format!("`{tag}`")),
                "prompt does not name {tag}"
            );
            let leaked = format!("a<{tag}>x</{tag}>b");
            assert_eq!(crate::qq::format::strip_internal_tags(&leaked), "ab");
        }
    }

    #[test]
    fn companion_replaces_only_the_identity() {
        let stand_in = render_static(AgentMode::StandIn);
        let companion = render_static(AgentMode::Companion);
        assert!(companion.starts_with("You are ScapegoatBot, a companion"));
        assert!(!companion.contains("standing in for the owner"));
        let rest_stand_in = stand_in.split_once("\n\n").unwrap().1;
        let rest_companion = companion.split_once("\n\n").unwrap().1;
        assert_eq!(rest_stand_in, rest_companion);
    }

    #[test]
    fn runtime_block_contains_required_fields() {
        let runtime = RuntimeContext {
            mode: "stand_in".to_owned(),
            account_qq: 10000,
            owner_name: "Boss".to_owned(),
            origin_chat: "group:123".to_owned(),
            owner_qq: 10000,
            master_qq: 20000,
            current_time: "2026-10-30 09:00:00".to_owned(),
            model: "anthropic/claude-opus-5-5".to_owned(),
            thinking_level: "high".to_owned(),
            current_session: "/sessions/a.jsonl".to_owned(),
            receive_scopes: vec!["*".to_owned()],
            send_scopes: vec!["group:123".to_owned()],
            group_roles: "group:123=owner".to_owned(),
            gate_mode: "auto".to_owned(),
            inbox_summary: "group:123: 2 unread".to_owned(),
        };
        let text = runtime.render();
        assert!(text.starts_with("<runtime_context>"));
        assert!(text.contains("origin_chat: group:123"));
        assert!(text.contains("current_session: /sessions/a.jsonl"));
        assert!(text.contains("master_qq: 20000"));
        assert!(text.contains("</runtime_context>"));
    }
}
