use serde::{Deserialize, Serialize};

use super::{AgentConfig, ConfigError, non_zero};

// ---------------------------------------------------------------------------
// [compaction]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionConfig {
    pub reserve_tokens: u32,
    pub turn_context_budget: u32,
    pub keep_recent_tokens: u32,
    pub precompact_percent: u32,
    pub max_failures: u32,
    pub memory_flush: bool,
    pub post_compact_recall: bool,
    pub prune: bool,
    pub prune_soft_limit: u32,
    pub prune_keep_turns: u32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            reserve_tokens: 16_384,
            turn_context_budget: 120_000,
            keep_recent_tokens: 20_000,
            precompact_percent: 80,
            max_failures: 3,
            memory_flush: true,
            post_compact_recall: true,
            prune: true,
            prune_soft_limit: 4_000,
            prune_keep_turns: 3,
        }
    }
}

impl CompactionConfig {
    pub(in crate::config) fn validate(&self, errors: &mut Vec<ConfigError>, agent: &AgentConfig) {
        for (field, value) in [
            ("compaction.reserve_tokens", self.reserve_tokens),
            ("compaction.turn_context_budget", self.turn_context_budget),
            ("compaction.keep_recent_tokens", self.keep_recent_tokens),
            ("compaction.max_failures", self.max_failures),
            ("compaction.prune_soft_limit", self.prune_soft_limit),
            ("compaction.prune_keep_turns", self.prune_keep_turns),
        ] {
            non_zero(errors, field, u64::from(value));
        }
        if self.precompact_percent > 99 {
            errors.push(ConfigError::new(
                "compaction.precompact_percent",
                "must be between 0 and 99 (0 turns background compaction off)",
            ));
        }
        if self.reserve_tokens < agent.max_output_tokens {
            errors.push(ConfigError::new(
                "compaction.reserve_tokens",
                format!(
                    "must be at least agent.max_output_tokens ({})",
                    agent.max_output_tokens
                ),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precompact_percent_defaults_to_80_and_accepts_zero_but_not_100() {
        let default: CompactionConfig = toml::from_str("").unwrap();
        assert_eq!(default.precompact_percent, 80);
        let agent = AgentConfig::default();

        let disabled: CompactionConfig = toml::from_str("precompact_percent = 0").unwrap();
        assert_eq!(disabled.precompact_percent, 0);
        let mut errors = Vec::new();
        disabled.validate(&mut errors, &agent);
        assert!(errors.is_empty(), "{errors:?}");

        let invalid: CompactionConfig = toml::from_str("precompact_percent = 100").unwrap();
        invalid.validate(&mut errors, &agent);
        assert_eq!(
            errors,
            [ConfigError::new(
                "compaction.precompact_percent",
                "must be between 0 and 99 (0 turns background compaction off)",
            )]
        );
    }
}
