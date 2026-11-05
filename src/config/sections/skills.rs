use serde::{Deserialize, Serialize};

use super::{ConfigError, non_empty, non_zero};

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
        non_empty(errors, "skills.shared_dir", &self.shared_dir);
        non_zero(
            errors,
            "skills.curator_interval_hours",
            u64::from(self.curator_interval_hours),
        );
        non_zero(
            errors,
            "skills.curator_min_new_skills",
            u64::from(self.curator_min_new_skills),
        );
    }
}
