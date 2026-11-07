use serde::{Deserialize, Serialize};

use super::ConfigError;

// ---------------------------------------------------------------------------
// [hooks]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    SessionEnd,
    PreCompact,
    PostCompact,
    MessageReceived,
    BeforeSend,
    Stop,
    ConfigChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookAction {
    Log,
    Block,
    Rewrite,
    InjectContext,
    Notify,
}

/// 事件 hook 的匹配条件；字段全部可选，全部为 `None` 表示匹配一切。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct HookMatch {
    pub group_id: Option<u64>,
    pub user_id: Option<u64>,
    pub message_type: Option<String>,
    pub tool_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventHookConfig {
    pub event: HookEvent,
    pub action: HookAction,
    #[serde(rename = "match")]
    pub match_: HookMatch,
    /// `block` 的原因。
    pub reason: String,
    /// `rewrite` 的正则与替换。
    pub pattern: String,
    pub replacement: String,
    /// `inject_context` 的来源。
    pub path: String,
    pub text: String,
    /// `notify` 的渠道；`owner` 才会给主人发私聊。
    pub notify_channel: String,
    /// `BeforeSend` 的作用域：`model`（默认）或 `all`。
    pub scope: String,
    pub enabled: bool,
}

impl Default for EventHookConfig {
    fn default() -> Self {
        Self {
            event: HookEvent::SessionStart,
            action: HookAction::Log,
            match_: HookMatch::default(),
            reason: String::new(),
            pattern: String::new(),
            replacement: String::new(),
            path: String::new(),
            text: String::new(),
            notify_channel: String::new(),
            scope: "model".to_owned(),
            enabled: true,
        }
    }
}

/// `[hooks]`：运维写给运行时的事件规则。
///
/// 上下文注入不在这里配置：`/AGENTS.md`、记忆与风格档案由程序固定注入，其余文件由
/// agent 自己在 VFS 的 `/hooks/context.toml` 里决定（见 `crate::prompt::context`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HooksConfig {
    pub events: Vec<EventHookConfig>,
    /// 已移除的 `[[hooks.context]]`；只为给出明确的迁移提示而保留解析。
    #[serde(skip_serializing)]
    context: Option<toml::Value>,
}

impl HooksConfig {
    pub(in crate::config) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if self.context.is_some() {
            errors.push(ConfigError::new(
                "hooks.context",
                "was removed in 0.2.0-alpha.37: /AGENTS.md, /USER.md, /MEMORY.md and the style \
                 profile are always injected, and the agent lists any other files in \
                 /hooks/context.toml inside its VFS; delete this section",
            ));
        }
        // 同一事件最多 16 条；动作参数必须齐全。
        let mut per_event = std::collections::HashMap::new();
        for (index, hook) in self.events.iter().enumerate() {
            let field = format!("hooks.events[{index}]");
            let count = per_event
                .entry(format!("{:?}", hook.event))
                .or_insert(0usize);
            *count += 1;
            if *count > 16 {
                errors.push(ConfigError::new(
                    format!("{field}.event"),
                    "at most 16 hooks may target the same event",
                ));
            }
            match hook.action {
                HookAction::Block => {
                    if hook.reason.trim().is_empty() {
                        errors.push(ConfigError::new(
                            format!("{field}.reason"),
                            "is required when action = \"block\"",
                        ));
                    }
                }
                HookAction::Rewrite => {
                    if hook.pattern.is_empty() {
                        errors.push(ConfigError::new(
                            format!("{field}.pattern"),
                            "is required when action = \"rewrite\"",
                        ));
                    } else if let Err(error) = regex::Regex::new(&hook.pattern) {
                        errors.push(ConfigError::new(
                            format!("{field}.pattern"),
                            format!("invalid regex: {error}"),
                        ));
                    }
                }
                HookAction::InjectContext => {
                    if hook.path.is_empty() && hook.text.is_empty() {
                        errors.push(ConfigError::new(
                            field.to_string(),
                            "inject_context requires \"path\" or \"text\"",
                        ));
                    }
                }
                HookAction::Log | HookAction::Notify => {}
            }
            if !matches!(hook.scope.as_str(), "model" | "all") {
                errors.push(ConfigError::new(
                    format!("{field}.scope"),
                    "must be \"model\" or \"all\"",
                ));
            }
            // 动作与事件的组合是否可用，见设计 §7.5 的表格。
            if !action_allowed_on(hook.event, hook.action) {
                errors.push(ConfigError::new(
                    format!("{field}.action"),
                    format!("{:?} is not allowed on {:?}", hook.action, hook.event),
                ));
            }
        }
    }
}

/// 设计 §7.5 的事件 / 动作矩阵。
fn action_allowed_on(event: HookEvent, action: HookAction) -> bool {
    use HookAction::*;
    use HookEvent::*;
    matches!(
        (event, action),
        (SessionStart, InjectContext)
            | (SessionStart, Log)
            | (SessionEnd, Log)
            | (SessionEnd, Notify)
            | (PreCompact, Log)
            | (PreCompact, InjectContext)
            | (PostCompact, InjectContext)
            | (PostCompact, Log)
            | (MessageReceived, InjectContext)
            | (MessageReceived, Log)
            | (MessageReceived, Block)
            | (BeforeSend, Block)
            | (BeforeSend, Rewrite)
            | (BeforeSend, Log)
            | (Stop, Log)
            | (ConfigChange, Log)
    )
}
