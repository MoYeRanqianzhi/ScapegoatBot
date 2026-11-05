use serde::{Deserialize, Serialize};

use super::ConfigError;

// ---------------------------------------------------------------------------
// [session]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionScope {
    /// 全局一份 transcript（默认）。
    #[default]
    Shared,
    /// 按 chat 隔离。
    PerChat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    pub scope: SessionScope,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            scope: SessionScope::Shared,
        }
    }
}

impl SessionConfig {
    pub(in crate::config) fn validate(&self, _errors: &mut Vec<ConfigError>) {
        // 枚举已由 serde 约束；保留方法以便未来增加取值相关的检查。
    }
}
