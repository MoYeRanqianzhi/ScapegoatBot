use serde::{Deserialize, Serialize};

use super::ConfigError;

// ---------------------------------------------------------------------------
// [style]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StyleConfig {
    pub enabled: bool,
    pub profile_path: String,
    pub max_chars: u32,
}

impl Default for StyleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            profile_path: "/STYLE.md".to_owned(),
            max_chars: 2_000,
        }
    }
}

impl StyleConfig {
    pub(in crate::config) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if self.max_chars == 0 {
            errors.push(ConfigError::new(
                "style.max_chars",
                "must be greater than zero",
            ));
        }
        if !self.profile_path.starts_with('/') {
            errors.push(ConfigError::new(
                "style.profile_path",
                "must be an absolute VFS path",
            ));
        }
    }
}
