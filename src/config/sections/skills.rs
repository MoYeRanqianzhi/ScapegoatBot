use serde::{Deserialize, Serialize};

use super::{ConfigError, non_zero};

// ---------------------------------------------------------------------------
// [skills]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    /// 已移除的 `shared_dir`；只为给出明确的迁移提示而保留解析。
    #[serde(skip_serializing)]
    shared_dir: Option<toml::Value>,
    pub curator: bool,
    pub curator_interval_hours: u32,
    pub curator_min_new_skills: u32,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            shared_dir: None,
            curator: true,
            curator_interval_hours: 168,
            curator_min_new_skills: 3,
        }
    }
}

impl SkillsConfig {
    pub(in crate::config) fn validate(&self, errors: &mut Vec<ConfigError>) {
        // 技能一直存放在 VFS 的 `/skills`（随数据库备份）；这个宿主路径从未用于加载，
        // 却是模型可写的 `/config/skills` 的一部分，能让 `sbot backup` 打包任意目录。
        if self.shared_dir.is_some() {
            errors.push(ConfigError::new(
                "skills.shared_dir",
                "was removed in 0.2.0-alpha.40: skills live in the VFS under /skills and are \
                 part of every backup; delete this key",
            ));
        }
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
