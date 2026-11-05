use serde::{Deserialize, Serialize};

use super::ConfigError;

// ---------------------------------------------------------------------------
// [filter]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct FilterConfig {
    pub ignore_groups: Vec<u64>,
    pub ignore_users: Vec<u64>,
    pub ignore_patterns: Vec<String>,
    pub require_mention: bool,
}

impl FilterConfig {
    pub(crate) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if self.ignore_groups.contains(&0) {
            errors.push(ConfigError::new(
                "filter.ignore_groups",
                "group ids must be non-zero",
            ));
        }
        if self.ignore_users.contains(&0) {
            errors.push(ConfigError::new(
                "filter.ignore_users",
                "user ids must be non-zero",
            ));
        }
        for pattern in &self.ignore_patterns {
            if let Err(error) = regex::Regex::new(pattern) {
                errors.push(ConfigError::new(
                    "filter.ignore_patterns",
                    format!("invalid regex {pattern:?}: {error}"),
                ));
            }
        }
    }
}
