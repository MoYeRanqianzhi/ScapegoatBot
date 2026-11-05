use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use super::ConfigError;

// ---------------------------------------------------------------------------
// [mcp]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    pub enabled: bool,
    /// 外部 MCP 服务器；默认空（不连接任何进程）。
    pub servers: Vec<McpServerConfig>,
}

impl McpConfig {
    pub(crate) fn validate(&self, errors: &mut Vec<ConfigError>) {
        let mut seen = HashSet::new();
        for (index, server) in self.servers.iter().enumerate() {
            let field = format!("mcp.servers[{index}]");
            if server.name.trim().is_empty() {
                errors.push(ConfigError::new(
                    format!("{field}.name"),
                    "must not be empty",
                ));
            } else if !seen.insert(server.name.as_str()) {
                errors.push(ConfigError::new(
                    format!("{field}.name"),
                    format!("{:?} is listed more than once", server.name),
                ));
            }
            if server.command.trim().is_empty() {
                errors.push(ConfigError::new(
                    format!("{field}.command"),
                    "must not be empty",
                ));
            }
        }
        if self.enabled && self.servers.iter().all(|server| !server.enabled) {
            errors.push(ConfigError::new(
                "mcp.enabled",
                "at least one enabled server is required",
            ));
        }
    }
}

/// 一个 stdio MCP 服务器。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpServerConfig {
    pub name: String,
    /// 可执行文件；按 PATH 解析。
    pub command: String,
    pub args: Vec<String>,
    /// 追加到子进程的环境变量。
    pub env: BTreeMap<String, String>,
    pub enabled: bool,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            enabled: true,
        }
    }
}
