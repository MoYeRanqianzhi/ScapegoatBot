use serde::{Deserialize, Serialize};

use super::ConfigError;

// ---------------------------------------------------------------------------
// [skills]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    pub shared_dir: String,
    pub curator: bool,
    pub curator_interval_hours: u32,
    pub curator_min_new_skills: u32,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            shared_dir: "~/.sbot/skills".to_owned(),
            curator: true,
            curator_interval_hours: 168,
            curator_min_new_skills: 3,
        }
    }
}

impl SkillsConfig {
    pub(in crate::config) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if self.shared_dir.trim().is_empty() {
            errors.push(ConfigError::new("skills.shared_dir", "must not be empty"));
        }
        if self.curator_interval_hours == 0 {
            errors.push(ConfigError::new(
                "skills.curator_interval_hours",
                "must be greater than zero",
            ));
        }
        if self.curator_min_new_skills == 0 {
            errors.push(ConfigError::new(
                "skills.curator_min_new_skills",
                "must be greater than zero",
            ));
        }
    }
}
