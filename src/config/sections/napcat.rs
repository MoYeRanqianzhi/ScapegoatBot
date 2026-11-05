use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// [napcat]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NapcatConfig {
    pub url: String,
}

impl Default for NapcatConfig {
    fn default() -> Self {
        Self {
            url: "ws://127.0.0.1:3001".to_owned(),
        }
    }
}
