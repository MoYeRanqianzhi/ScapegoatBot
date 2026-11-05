use serde::{Deserialize, Serialize};

use super::ConfigError;

// ---------------------------------------------------------------------------
// [ipc]
// ---------------------------------------------------------------------------

/// IPC 端点配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IpcConfig {
    /// Windows 管道名（不含 `\\.\pipe\` 前缀）；Unix 下是 socket 文件名。
    pub name: String,
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            name: "scapegoat-bot".to_owned(),
        }
    }
}

impl IpcConfig {
    pub(in crate::config) fn validate(&self, errors: &mut Vec<ConfigError>) {
        let name = &self.name;
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            errors.push(ConfigError::new(
                "ipc.name",
                "must be non-empty and contain only ASCII letters, digits, '-', '_' or '.'",
            ));
        }
    }
}
