use serde::{Deserialize, Serialize};

use super::{CompactionConfig, ConfigError};

// ---------------------------------------------------------------------------
// [budget]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    pub daily_tokens: u64,
    pub daily_usd: f64,
    pub turn_tokens: u64,
    pub chat_sends_per_hour: u32,
    pub chat_turns_per_hour: u32,
    pub round_trip_wall_secs: u64,
    pub guard_usd_per_day: f64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            daily_tokens: 2_000_000,
            daily_usd: 10.0,
            turn_tokens: 150_000,
            chat_sends_per_hour: 60,
            chat_turns_per_hour: 120,
            round_trip_wall_secs: 600,
            guard_usd_per_day: 0.20,
        }
    }
}

impl BudgetConfig {
    pub(in crate::config) fn validate(
        &self,
        errors: &mut Vec<ConfigError>,
        compaction: &CompactionConfig,
    ) {
        if !self.daily_usd.is_finite() || self.daily_usd < 0.0 {
            errors.push(ConfigError::new(
                "budget.daily_usd",
                "must be a non-negative number",
            ));
        }
        if !self.guard_usd_per_day.is_finite() || self.guard_usd_per_day < 0.0 {
            errors.push(ConfigError::new(
                "budget.guard_usd_per_day",
                "must be a non-negative number",
            ));
        }
        if self.chat_sends_per_hour == 0 || self.chat_turns_per_hour == 0 {
            errors.push(ConfigError::new(
                "budget.chat_*_per_hour",
                "must be greater than zero",
            ));
        }
        if self.round_trip_wall_secs == 0 {
            errors.push(ConfigError::new(
                "budget.round_trip_wall_secs",
                "must be greater than zero",
            ));
        }
        // `turn_tokens` 是回合累计用量，`turn_context_budget` 是单次请求工作集；
        // 两者量纲不同，不设大小关系（设计 §9.6 已据此修正）。
        let _ = compaction;
    }
}
