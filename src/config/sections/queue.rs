use serde::{Deserialize, Serialize};

use super::{ConfigError, non_zero};

// ---------------------------------------------------------------------------
// [queue]
// ---------------------------------------------------------------------------

/// 回合运行期间到达的新消息如何处理（`[queue].mode`），实现见 `agent::queue`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QueueMode {
    /// 送进 steering 队列，当前回合在下一个模型边界就能看到。
    #[default]
    Steer,
    /// 留到回合结束后，每条消息各开一个回合（按到达顺序）。
    Followup,
    /// 留到回合结束后，再等一个 `debounce_ms`，把这段时间到达的消息合并成一个回合。
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
    pub drop: DropPolicy,
    pub debounce_ms: u64,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            mode: QueueMode::Steer,
            max_concurrent: 4,
            cap_messages: 200,
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
