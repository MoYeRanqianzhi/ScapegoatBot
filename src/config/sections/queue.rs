use serde::{Deserialize, Serialize};

use super::{ConfigError, non_zero};

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
    pub(in crate::config) fn validate(&self, errors: &mut Vec<ConfigError>) {
        non_zero(errors, "queue.max_concurrent", self.max_concurrent as u64);
        non_zero(errors, "queue.cap_messages", self.cap_messages as u64);
        non_zero(errors, "queue.debounce_ms", self.debounce_ms);
    }
}
