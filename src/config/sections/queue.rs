use serde::{Deserialize, Serialize};

use super::ConfigError;

// ---------------------------------------------------------------------------
// [queue]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QueueMode {
    #[default]
    Steer,
    Followup,
    Collect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DropPolicy {
    #[default]
    Summarize,
    Old,
    New,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueueConfig {
    pub mode: QueueMode,
    pub max_concurrent: usize,
    pub cap_messages: usize,
    /// Deprecated: accepted for old configs, no longer used by the inbox wake model.
    #[serde(default = "default_cap_batches")]
    pub cap_batches: usize,
    pub drop: DropPolicy,
    pub debounce_ms: u64,
}

fn default_cap_batches() -> usize {
    20
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            mode: QueueMode::Steer,
            max_concurrent: 4,
            cap_messages: 200,
            cap_batches: 20,
            drop: DropPolicy::Summarize,
            debounce_ms: 3_000,
        }
    }
}

impl QueueConfig {
    pub(crate) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if self.max_concurrent == 0 {
            errors.push(ConfigError::new(
                "queue.max_concurrent",
                "must be greater than zero",
            ));
        }
        if self.cap_messages == 0 {
            errors.push(ConfigError::new(
                "queue.cap_messages",
                "must be greater than zero",
            ));
        }
        if self.debounce_ms == 0 {
            errors.push(ConfigError::new(
                "queue.debounce_ms",
                "must be greater than zero",
            ));
        }
    }
}
