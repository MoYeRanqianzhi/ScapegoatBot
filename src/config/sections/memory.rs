use serde::{Deserialize, Serialize};

use super::ConfigError;

// ---------------------------------------------------------------------------
// [memory]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryReviewMode {
    Off,
    #[default]
    Idle,
    EveryTurn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    pub enabled: bool,
    pub max_entry_chars: u32,
    pub max_index_lines: u32,
    pub max_index_bytes: u32,
    pub user_max_chars: u32,
    pub review: MemoryReviewMode,
    pub idle_minutes: u32,
    pub review_interval_minutes: u32,
    pub review_model: String,
    pub topics: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entry_chars: 4_000,
            max_index_lines: 200,
            max_index_bytes: 25_000,
            user_max_chars: 4_000,
            review: MemoryReviewMode::Idle,
            idle_minutes: 10,
            review_interval_minutes: 60,
            review_model: String::new(),
            topics: true,
        }
    }
}

impl MemoryConfig {
    pub(in crate::config) fn validate(&self, errors: &mut Vec<ConfigError>) {
        for (field, value) in [
            ("memory.max_entry_chars", self.max_entry_chars),
            ("memory.max_index_lines", self.max_index_lines),
            ("memory.max_index_bytes", self.max_index_bytes),
            ("memory.user_max_chars", self.user_max_chars),
            ("memory.idle_minutes", self.idle_minutes),
            (
                "memory.review_interval_minutes",
                self.review_interval_minutes,
            ),
        ] {
            if value == 0 {
                errors.push(ConfigError::new(field, "must be greater than zero"));
            }
        }
    }
}
