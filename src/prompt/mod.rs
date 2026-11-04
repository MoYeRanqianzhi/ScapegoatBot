//! 系统提示词组装。
//!
//! 设计依据：设计 §7.1 / §7.2 / §7.4。
//!
//! 结构：
//! - 静态前缀：`assets/system_prompt_v4.template.txt` 中
//!   `__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__` 之前的内容；字节级稳定，用于前缀缓存；
//! - 动态块：owner context → USER → memory → STYLE → skills，会话开始时构建一次，
//!   压缩后重建；
//! - runtime：每轮作为独立上下文消息注入，不进入静态前缀。
//!
//! 所有模型面文本英文；注入正文保持原文。

use serde::Serialize;

use crate::config::{AgentMode, ContextHookConfig, ContextKind};
use crate::vfs::VfsPath;
use crate::vfs::sqlite::VfsStore;

/// 完整 v4 模板。
const TEMPLATE: &str = include_str!("../../assets/system_prompt_v4.template.txt");
/// 静态/动态分界标记。
pub const BOUNDARY: &str = "__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__";
/// companion 身份段（替换 stand_in 的第一段）。
const COMPANION_IDENTITY: &str = "You are ScapegoatBot, a companion account with its own QQ. \
You are not the owner and never claim to be. You speak with your own voice, shaped by the persona \
the owner has set for this account. The owner is your principal, shown as `owner_qq` in Runtime; \
other people are conversation partners, not your principal. You may run for months without anyone \
watching. You are not a general-purpose assistant in this role.";

/// 静态前缀（按模式选择身份）。
pub fn static_prefix(mode: AgentMode) -> &'static str {
    // 模板在编译期已知，切分一次即可。
    static SPLIT: std::sync::OnceLock<(&'static str, &'static str)> = std::sync::OnceLock::new();
    let (static_part, _) = SPLIT.get_or_init(|| {
        // Windows checkout 可能把模板转成 CRLF；提示词必须跨平台逐字节一致。
        let template: &'static str = Box::leak(TEMPLATE.replace("\r\n", "\n").into_boxed_str());
        let (before, after) = template
            .split_once(BOUNDARY)
            .expect("template must contain the dynamic boundary");
        (before, after)
    });
    let _ = mode;
    // companion 的身份替换在 `render_static` 中完成；这里返回共享前缀。
    static_part
}

/// 按模式渲染静态前缀。
pub fn render_static(mode: AgentMode) -> String {
    let prefix = static_prefix(mode);
    match mode {
        AgentMode::StandIn => prefix.trim_end().to_owned(),
        AgentMode::Companion => {
            // 第一段是 stand_in 身份；整段替换，其余不变。
            let first_break = prefix.find("\n\n").unwrap_or(prefix.len());
            let rest = &prefix[first_break..];
            format!("{COMPANION_IDENTITY}{rest}").trim_end().to_owned()
        }
    }
}

/// 动态块。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DynamicContext {
    /// `(source, content)`；按 hook 顺序拼接。
    pub owner: Vec<(String, String)>,
    pub user: String,
    pub memory: String,
    pub style: String,
    pub skills: String,
}

impl DynamicContext {
    /// 渲染成带标签的 system 文本块。
    pub fn render(&self) -> Vec<String> {
        let mut blocks = Vec::new();
        if !self.owner.is_empty() {
            blocks.push(format!(
                "# Owner context\n\n{}",
                self.owner
                    .iter()
                    .map(|(source, content)| format!(
                        "<owner_context source=\"{source}\">\n{content}\n</owner_context>"
                    ))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            ));
        }
        if !self.user.trim().is_empty() {
            blocks.push(format!(
                "# User model\n\n<user_context>\n{}\n</user_context>",
                self.user.trim_end()
            ));
        }
        if !self.memory.trim().is_empty() {
            blocks.push(format!(
                "# Memory context\n\n<memory_context>\n{}\n</memory_context>",
                self.memory.trim_end()
            ));
        }
        if !self.style.trim().is_empty() {
            blocks.push(format!(
                "# Style profile\n\n<style_profile>\n{}\n</style_profile>",
                self.style.trim_end()
            ));
        }
        if !self.skills.trim().is_empty() {
            blocks.push(format!("# Skill index\n\n{}", self.skills.trim_end()));
        }
        blocks
    }
}

/// 从 hook 读取动态上下文。
///
/// 只读取 VFS 中可读文件；文件不存在时静默跳过（设计 §7.4）。
/// 读取使用“不记录读过”的接口：系统注入不是 agent 在浏览文件。
pub fn load_hooks(vfs: &VfsStore, hooks: &[ContextHookConfig]) -> DynamicContext {
    let mut context = DynamicContext::default();
    for hook in hooks.iter().filter(|hook| hook.enabled) {
        let Ok(path) = VfsPath::parse(&hook.path) else {
            continue;
        };
        let Ok(content) = vfs.read_text_unrecorded(&path) else {
            continue;
        };
        let content = truncate(&content, hook.max_chars, hook.max_bytes);
        if content.trim().is_empty() {
            continue;
        }
        match hook.kind {
            ContextKind::Owner => context.owner.push((hook.path.clone(), content)),
            ContextKind::User => context.user = content,
            ContextKind::Memory => context.memory = content,
            ContextKind::Style => context.style = content,
        }
    }
    context
}

fn truncate(content: &str, max_chars: Option<u32>, max_bytes: Option<u64>) -> String {
    let mut out = content.to_owned();
    if let Some(max_chars) = max_chars
        && out.chars().count() > max_chars as usize
    {
        out = out.chars().take(max_chars as usize).collect();
    }
    if let Some(max_bytes) = max_bytes
        && out.len() > max_bytes as usize
    {
        // 按字符边界截断，避免切坏 UTF-8。
        let mut end = max_bytes as usize;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

/// runtime 上下文字段（每轮独立注入）。
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeContext {
    pub mode: String,
    pub session_scope: String,
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
             session_scope: {scope}\n\
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
             These values are authoritative for this request. `mode` and `session_scope` are fixed \
             for this process; never try to switch them. `origin_chat` is the chat that started \
             this turn; reply there by default. `master_qq` is the only private channel for \
             reporting to the owner; if it is empty or 0, keep reports internal.",
            mode = self.mode,
            scope = self.session_scope,
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
    fn static_prefix_starts_with_identity_and_has_no_boundary() {
        let prefix = render_static(AgentMode::StandIn);
        assert!(prefix.starts_with("You are ScapegoatBot"));
        assert!(!prefix.contains(BOUNDARY));
        assert!(prefix.contains("# Operating model"));
        assert!(prefix.contains("# Instruction precedence"));
    }

    #[test]
    fn companion_replaces_only_the_identity() {
        let stand_in = render_static(AgentMode::StandIn);
        let companion = render_static(AgentMode::Companion);
        assert!(companion.starts_with("You are ScapegoatBot, a companion account"));
        assert!(!companion.contains("QQ stand-in for the owner"));
        // 其余部分逐字一致。
        let rest_stand_in = stand_in.split_once("\n\n").unwrap().1;
        let rest_companion = companion.split_once("\n\n").unwrap().1;
        assert_eq!(rest_stand_in, rest_companion);
    }

    #[test]
    fn static_prefix_is_byte_stable() {
        assert_eq!(
            render_static(AgentMode::StandIn),
            render_static(AgentMode::StandIn)
        );
    }

    #[test]
    fn dynamic_blocks_use_the_expected_tags() {
        let context = DynamicContext {
            owner: vec![("/AGENTS.md".to_owned(), "rules".to_owned())],
            user: "prefer short replies".to_owned(),
            memory: "- [A](memory/a.md) — hook".to_owned(),
            style: "terse".to_owned(),
            skills: "- my-skill: does things".to_owned(),
        };
        let blocks = context.render();
        assert_eq!(blocks.len(), 5);
        assert!(blocks[0].contains("<owner_context source=\"/AGENTS.md\">"));
        assert!(blocks[1].contains("<user_context>"));
        assert!(blocks[2].contains("<memory_context>"));
        assert!(blocks[3].contains("<style_profile>"));
        assert!(blocks[4].starts_with("# Skill index"));
    }

    #[test]
    fn runtime_block_contains_required_fields() {
        let runtime = RuntimeContext {
            mode: "stand_in".to_owned(),
            session_scope: "shared".to_owned(),
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

    #[test]
    fn truncation_respects_char_boundaries() {
        let text = "你好世界";
        let cut = truncate(text, Some(2), None);
        assert_eq!(cut, "你好");
        // UTF-8 下每个汉字 3 字节；4 字节只能容纳一个。
        assert_eq!(truncate(text, None, Some(4)), "你");
        assert_eq!(truncate(text, None, Some(6)), "你好");
    }
}
