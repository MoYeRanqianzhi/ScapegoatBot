use serde::{Deserialize, Serialize};

use super::{ConfigError, non_empty, non_zero};

// ---------------------------------------------------------------------------
// [storage]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EncryptionMode {
    /// 内容加密（默认）。
    #[default]
    #[serde(rename = "xchacha20poly1305")]
    XChaCha20Poly1305,
    /// 仅调试；启动时告警。
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    pub path: String,
    pub encryption: EncryptionMode,
    pub max_blob_bytes: u64,
    pub max_vfs_bytes: u64,
    pub max_message_bytes: u64,
    pub max_session_bytes: u64,
    pub message_retention_days: u32,
    pub session_retention_days: u32,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: "./data".to_owned(),
            encryption: EncryptionMode::XChaCha20Poly1305,
            max_blob_bytes: 512 * 1024 * 1024,
            max_vfs_bytes: 10 * 1024 * 1024 * 1024,
            max_message_bytes: 10 * 1024 * 1024 * 1024,
            max_session_bytes: 2 * 1024 * 1024 * 1024,
            message_retention_days: 0,
            session_retention_days: 0,
        }
    }
}

impl StorageConfig {
    pub(in crate::config) fn validate(&self, errors: &mut Vec<ConfigError>) {
        non_empty(errors, "storage.path", &self.path);
        non_zero(errors, "storage.max_blob_bytes", self.max_blob_bytes);
    }
}
