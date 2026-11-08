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

/// 事件 hook 的匹配条件（精确匹配）；字段全部可选，全部为 `None` 表示匹配一切。
///
/// 设了 `group_id` / `user_id` / `message_type` 的 hook 只在带 chat 的事件
/// （`message_received`、`before_send`）上命中。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct HookMatch {
    pub group_id: Option<u64>,
    pub user_id: Option<u64>,
    /// `group` 或 `private`。
    pub message_type: Option<String>,
    /// 已移除的 `tool_name`；只为给出明确的迁移提示而保留解析（`pub(crate)` 只为测试里的
    /// 结构体更新语法）。
    #[serde(skip_serializing)]
    pub(crate) tool_name: Option<toml::Value>,
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
    /// `inject_context` 的来源：`text` 原样，或 `path`（VFS 路径，读取时最多 8000 字符）。
    pub path: String,
    pub text: String,
    /// `notify` 的渠道：空 = 只记日志；`owner`（或 `master`）给 `master_qq` 发私聊。
    pub notify_channel: String,
    /// 已移除的 `scope`；只为给出明确的迁移提示而保留解析（`pub(crate)` 只为测试里的
    /// 结构体更新语法）。
    #[serde(skip_serializing)]
    pub(crate) scope: Option<toml::Value>,
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
            scope: None,
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
                    if hook.path.trim().is_empty() && hook.text.trim().is_empty() {
                        errors.push(ConfigError::new(
                            field.to_string(),
                            "inject_context requires \"path\" or \"text\"",
                        ));
                    } else if hook.text.trim().is_empty()
                        && let Err(error) = crate::vfs::VfsPath::parse(&hook.path)
                    {
                        errors.push(ConfigError::new(
                            format!("{field}.path"),
                            format!("must be a VFS path: {error}"),
                        ));
                    }
                }
                HookAction::Notify => {
                    if !matches!(hook.notify_channel.as_str(), "" | "owner" | "master") {
                        errors.push(ConfigError::new(
                            format!("{field}.notify_channel"),
                            "must be empty (log only), \"owner\" or \"master\"",
                        ));
                    }
                }
                HookAction::Log => {}
            }
            // 这两个键从未改变过行为：BeforeSend 只作用于 agent 的发布动作，没有工具事件。
            if hook.scope.is_some() {
                errors.push(ConfigError::new(
                    format!("{field}.scope"),
                    "was removed in 0.2.0-alpha.42: before_send hooks always apply to what the \
                     agent publishes through API; delete this key",
                ));
            }
            if hook.match_.tool_name.is_some() {
                errors.push(ConfigError::new(
                    format!("{field}.match.tool_name"),
                    "was removed in 0.2.0-alpha.42: no hook event carries a tool name, so it \
                     never matched; delete this key",
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

/// 设计 §7.5 的事件 / 动作矩阵；运行时只实现这些组合。
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
