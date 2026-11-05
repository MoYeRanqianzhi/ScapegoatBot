use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::ConfigError;

// ---------------------------------------------------------------------------
// [hooks]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookTrigger {
    SessionStart,
    PostCompact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    Owner,
    User,
    Memory,
    Style,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectMode {
    System,
    UserPrefix,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextHookConfig {
    pub name: String,
    /// VFS 路径或合法 glob。
    pub path: String,
    pub kind: ContextKind,
    pub when: Vec<HookTrigger>,
    pub inject: InjectMode,
    /// 可选字节上限；缺省按 kind 的默认预算。
    pub max_bytes: Option<u64>,
    /// 可选字符上限；与 `max_bytes` 同时存在时取先触发者。
    pub max_chars: Option<u32>,
    pub enabled: bool,
}

impl Default for ContextHookConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            path: String::new(),
            kind: ContextKind::Owner,
            when: vec![HookTrigger::SessionStart, HookTrigger::PostCompact],
            inject: InjectMode::System,
            max_bytes: None,
            max_chars: None,
            enabled: true,
        }
    }
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HooksConfig {
    pub context: Vec<ContextHookConfig>,
    pub events: Vec<EventHookConfig>,
}

impl Default for HooksConfig {
    fn default() -> Self {
        // 设计 §7.4 的默认 hook：AGENTS / USER / MEMORY / STYLE 各一条。
        Self {
            context: vec![
                ContextHookConfig {
                    name: "agents".to_owned(),
                    path: "/AGENTS.md".to_owned(),
                    kind: ContextKind::Owner,
                    max_bytes: Some(65_536),
                    ..ContextHookConfig::default()
                },
                ContextHookConfig {
                    name: "user".to_owned(),
                    path: "/USER.md".to_owned(),
                    kind: ContextKind::User,
                    max_chars: Some(4_000),
                    ..ContextHookConfig::default()
                },
                ContextHookConfig {
                    name: "memory".to_owned(),
                    path: "/MEMORY.md".to_owned(),
                    kind: ContextKind::Memory,
                    max_bytes: Some(25_000),
                    ..ContextHookConfig::default()
                },
                ContextHookConfig {
                    name: "style".to_owned(),
                    path: "/STYLE.md".to_owned(),
                    kind: ContextKind::Style,
                    max_chars: Some(2_000),
                    ..ContextHookConfig::default()
                },
            ],
            events: Vec::new(),
        }
    }
}

impl HooksConfig {
    pub(in crate::config) fn validate(&self, errors: &mut Vec<ConfigError>) {
        let mut names = HashSet::new();
        for (index, hook) in self.context.iter().enumerate() {
            let field = format!("hooks.context[{index}]");
            if hook.name.trim().is_empty() {
                errors.push(ConfigError::new(
                    format!("{field}.name"),
                    "must not be empty",
                ));
            } else if !names.insert(hook.name.as_str()) {
                errors.push(ConfigError::new(
                    format!("{field}.name"),
                    format!("duplicate hook name {:?}", hook.name),
                ));
            }
            if !hook.path.starts_with('/') {
                errors.push(ConfigError::new(
                    format!("{field}.path"),
                    "must be an absolute VFS path or glob",
                ));
            }
            if hook.when.is_empty() {
                errors.push(ConfigError::new(
                    format!("{field}.when"),
                    "must list at least one trigger",
                ));
            }
            if hook.max_bytes == Some(0) || hook.max_chars == Some(0) {
                errors.push(ConfigError::new(
                    format!("{field}.max_bytes / {field}.max_chars"),
                    "must be greater than zero when set",
                ));
            }
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
