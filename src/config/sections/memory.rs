use serde::{Deserialize, Serialize};

use super::{ConfigError, non_zero};

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
    /// 后台复盘与压缩前记忆提取用的模型（`provider/id`）；空 = 跟随主模型。
    pub review_model: String,
    /// 已移除的 `topics`；只为给出明确的迁移提示而保留解析。
    #[serde(skip_serializing)]
    topics: Option<toml::Value>,
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
            topics: None,
        }
    }
}

impl MemoryConfig {
    pub(in crate::config) fn validate(&self, errors: &mut Vec<ConfigError>) {
        // `/memory/topics` 只是一个约定目录，模型可以自由使用；这个开关从未被读取。
        if self.topics.is_some() {
            errors.push(ConfigError::new(
                "memory.topics",
                "was removed in 0.2.0-alpha.42: it never had an effect (/memory/topics is an \
                 ordinary directory); delete this key",
            ));
        }
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
            non_zero(errors, field, u64::from(value));
        }
    }
}
