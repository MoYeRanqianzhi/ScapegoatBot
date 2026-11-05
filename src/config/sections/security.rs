use serde::{Deserialize, Serialize};

use super::ConfigError;

// ---------------------------------------------------------------------------
// [security]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GuardMode {
    /// 只记录判定，不触发 hold。
    #[default]
    Shadow,
    /// 命中即 hold。
    Enforce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GuardBackend {
    #[default]
    Jev,
    Llm,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuardConfig {
    pub enabled: bool,
    pub mode: GuardMode,
    pub backend: GuardBackend,
    /// 空 = 后端默认模型。
    pub model: String,
    /// 空 = 后端官方地址。
    pub base_url: String,
    /// dangerous 概率阈值。
    pub threshold: f64,
    pub timeout_ms: u64,
    pub max_state_tokens: u32,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: GuardMode::Shadow,
            backend: GuardBackend::Jev,
            model: String::new(),
            base_url: String::new(),
            threshold: 0.9,
            timeout_ms: 800,
            max_state_tokens: 8_000,
        }
    }
}

impl GuardConfig {
    pub(in crate::config) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if !(0.5..=0.99).contains(&self.threshold) {
            errors.push(ConfigError::new(
                "security.guard.threshold",
                "must be between 0.5 and 0.99",
            ));
        }
        if !(50..=5_000).contains(&self.timeout_ms) {
            errors.push(ConfigError::new(
                "security.guard.timeout_ms",
                "must be between 50 and 5000",
            ));
        }
        if !(1_000..=20_000).contains(&self.max_state_tokens) {
            errors.push(ConfigError::new(
                "security.guard.max_state_tokens",
                "must be between 1000 and 20000",
            ));
        }
        if self.backend == GuardBackend::Jev && !self.base_url.is_empty() {
            let ok = self.base_url.starts_with("https://") || self.base_url.starts_with("http://");
            if !ok {
                errors.push(ConfigError::new(
                    "security.guard.base_url",
                    "must start with http:// or https://",
                ));
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    pub check_every_tokens: u64,
    pub check_on_compaction: bool,
    pub check_overlap_tokens: u64,
    pub review_model: String,
    pub check_timeout_secs: u64,
    pub guard: GuardConfig,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            check_every_tokens: 262_144,
            check_on_compaction: true,
            check_overlap_tokens: 4_096,
            review_model: String::new(),
            check_timeout_secs: 60,
            guard: GuardConfig::default(),
        }
    }
}

impl SecurityConfig {
    pub(in crate::config) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if self.check_timeout_secs == 0 {
            errors.push(ConfigError::new(
                "security.check_timeout_secs",
                "must be greater than zero",
            ));
        }
        self.guard.validate(errors);
    }
}
