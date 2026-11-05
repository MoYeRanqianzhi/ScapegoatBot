use serde::{Deserialize, Serialize};

use super::{ConfigError, non_zero};

// ---------------------------------------------------------------------------
// [tools]
// ---------------------------------------------------------------------------

/// Web 工具后端选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WebBackend {
    /// 优先用协议服务端工具，其次用用户配置的 HTTP 后端。
    #[default]
    Auto,
    /// 只用协议服务端工具。
    Server,
    /// 只用用户配置的 HTTP 后端。
    Tool,
    /// 不注册该工具。
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchConfig {
    pub backend: WebBackend,
    pub base_url: String,
    pub count: u32,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            backend: WebBackend::Auto,
            base_url: String::new(),
            count: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebFetchConfig {
    pub backend: WebBackend,
    pub base_url: String,
    pub max_content_tokens: u32,
    /// 本地抓取：不经过 HTTP 后端，逐跳校验后直接 GET。
    pub local: bool,
}

impl Default for WebFetchConfig {
    fn default() -> Self {
        Self {
            backend: WebBackend::Auto,
            base_url: String::new(),
            max_content_tokens: 20_000,
            local: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DownloadConfig {
    pub max_bytes: u64,
    pub allowed_domains: Vec<String>,
    pub blocked_domains: Vec<String>,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            max_bytes: 512 * 1024 * 1024,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    pub max_result_bytes: usize,
    pub max_schema_tokens: usize,
    pub web_search: WebSearchConfig,
    pub web_fetch: WebFetchConfig,
    pub download: DownloadConfig,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            max_result_bytes: 32_768,
            max_schema_tokens: 12_000,
            web_search: WebSearchConfig::default(),
            web_fetch: WebFetchConfig::default(),
            download: DownloadConfig::default(),
        }
    }
}

impl ToolsConfig {
    pub(in crate::config) fn validate(&self, errors: &mut Vec<ConfigError>) {
        non_zero(
            errors,
            "tools.max_result_bytes",
            self.max_result_bytes as u64,
        );
        non_zero(
            errors,
            "tools.max_schema_tokens",
            self.max_schema_tokens as u64,
        );
        if self.web_search.backend == WebBackend::Tool && self.web_search.base_url.trim().is_empty()
        {
            errors.push(ConfigError::new(
                "tools.web_search.base_url",
                "is required when backend = \"tool\"",
            ));
        }
        if self.web_fetch.backend == WebBackend::Tool && self.web_fetch.base_url.trim().is_empty() {
            errors.push(ConfigError::new(
                "tools.web_fetch.base_url",
                "is required when backend = \"tool\"",
            ));
        }
        non_zero(
            errors,
            "tools.web_search.count",
            u64::from(self.web_search.count),
        );
        non_zero(
            errors,
            "tools.web_fetch.max_content_tokens",
            u64::from(self.web_fetch.max_content_tokens),
        );
        non_zero(errors, "tools.download.max_bytes", self.download.max_bytes);
    }
}
