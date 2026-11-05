use serde::{Deserialize, Serialize};

use super::ConfigError;

// ---------------------------------------------------------------------------
// [swarm]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SwarmConfig {
    pub enabled: bool,
    pub max_depth: u32,
    pub max_concurrency: u32,
    pub agents_dir: String,
    pub default_max_steps: u32,
}

impl Default for SwarmConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_depth: 8,
            max_concurrency: 16,
            agents_dir: "~/.sbot/agents".to_owned(),
            default_max_steps: 32,
        }
    }
}

impl SwarmConfig {
    pub(crate) fn validate(&self, errors: &mut Vec<ConfigError>) {
        for (field, value) in [
            ("swarm.max_depth", self.max_depth),
            ("swarm.max_concurrency", self.max_concurrency),
            ("swarm.default_max_steps", self.default_max_steps),
        ] {
            if value == 0 {
                errors.push(ConfigError::new(field, "must be greater than zero"));
            }
        }
        if self.agents_dir.trim().is_empty() {
            errors.push(ConfigError::new("swarm.agents_dir", "must not be empty"));
        }
    }
}
