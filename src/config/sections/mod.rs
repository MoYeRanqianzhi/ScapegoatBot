//! `sbot.toml` 的分节 schema。
//!
//! 每个小节一个结构体 + `Default` + `validate`；根结构与加载逻辑在
//! `config/mod.rs`。拆开只是为了让人一次只看一个域（agent / queue / storage
//! / security / monitor / hooks …），字段路径与错误信息仍然是集中的。

mod agent;
mod budget;
mod compaction;
mod filter;
mod gate;
mod hooks;
mod ipc;
mod mcp;
mod memory;
mod monitor;
mod napcat;
mod queue;
mod security;
mod skills;
mod storage;
mod style;
mod swarm;
mod tools;

pub use agent::{AgentConfig, AgentMode, RetryMode};
pub use budget::BudgetConfig;
pub use compaction::CompactionConfig;
pub use filter::FilterConfig;
pub use gate::{GateConfig, GateMode};
pub use hooks::{
    ContextHookConfig, ContextKind, EventHookConfig, HookAction, HookEvent, HookMatch, HookTrigger,
    HooksConfig, InjectMode,
};
pub use ipc::IpcConfig;
pub use mcp::{McpConfig, McpServerConfig};
pub use memory::{MemoryConfig, MemoryReviewMode};
pub use monitor::{
    BacklogMode, CatchUp, DeliverAs, DeliverConfig, InternalSource, LifecycleConfig, MonitorConfig,
    MonitorSource, OnMissing, QqSource, QqSourceConfig, StreamKind, StreamSource, TimerSource,
};
pub use napcat::NapcatConfig;
pub use queue::{DropPolicy, QueueConfig, QueueMode};
pub use security::{GuardBackend, GuardConfig, GuardMode, SecurityConfig};
pub use skills::SkillsConfig;
pub use storage::{EncryptionMode, StorageConfig};
pub use style::StyleConfig;
pub use swarm::SwarmConfig;
pub use tools::{DownloadConfig, ToolsConfig, WebBackend, WebFetchConfig, WebSearchConfig};

pub(in crate::config) use monitor::validate_monitors;

use super::{ConfigError, ModelsFile};

/// 校验 scope 列表：`*`、`group:<gid>`、`private:<qq>`。
pub(super) fn validate_scopes(field: &str, scopes: &[String], errors: &mut Vec<ConfigError>) {
    if scopes.is_empty() {
        // 空表示拒绝一切；这是合法配置（`send_scopes = []` 即禁发）。
        return;
    }
    for scope in scopes {
        if scope == "*" {
            continue;
        }
        let ok = scope
            .strip_prefix("group:")
            .or_else(|| scope.strip_prefix("private:"))
            .is_some_and(|id| id.parse::<u64>().is_ok_and(|id| id != 0));
        if !ok {
            errors.push(ConfigError::new(
                field,
                format!("{scope:?} must be \"*\", \"group:<gid>\" or \"private:<qq>\""),
            ));
        }
    }
}

/// 检查模型是否既在 `available_models` 白名单里、又存在于 `models.json`。
pub(super) fn check_available(
    models: &ModelsFile,
    available: &[String],
    name: &str,
    field: &str,
    errors: &mut Vec<ConfigError>,
) {
    if !available.iter().any(|item| item == name) {
        errors.push(ConfigError::new(
            field,
            format!("{name:?} is not in agent.available_models"),
        ));
    } else if models.find(name).is_none() {
        errors.push(ConfigError::new(
            field,
            format!("{name:?} is not defined in models.json"),
        ));
    }
}

/// 整型配置必须大于零。
pub(super) fn non_zero(errors: &mut Vec<ConfigError>, field: impl Into<String>, value: u64) {
    if value == 0 {
        errors.push(ConfigError::new(field, "must be greater than zero"));
    }
}

/// 字符串配置去掉首尾空白后不能为空。
pub(super) fn non_empty(errors: &mut Vec<ConfigError>, field: impl Into<String>, value: &str) {
    if value.trim().is_empty() {
        errors.push(ConfigError::new(field, "must not be empty"));
    }
}
