use serde::{Deserialize, Serialize};

use super::{ConfigError, non_zero};

// ---------------------------------------------------------------------------
// [gate]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GateMode {
    /// 不做逐条审查。
    Trusted,
    /// 规则检查（+ 可选 LLM 审查）。
    #[default]
    Auto,
    /// 每条都需要主人确认。
    Approve,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GateConfig {
    pub mode: GateMode,
    pub max_per_minute: u32,
    pub llm_review: bool,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            mode: GateMode::Auto,
            max_per_minute: 20,
            llm_review: false,
        }
    }
}

impl GateConfig {
    pub(in crate::config) fn validate(&self, errors: &mut Vec<ConfigError>) {
        non_zero(
            errors,
            "gate.max_per_minute",
            u64::from(self.max_per_minute),
        );
    }
}
