//! `sbot.toml` 的分节 schema。
//!
//! 每个小节一个结构体 + `Default` + `validate`；根结构与加载逻辑在
//! `config/mod.rs`。拆开只是为了让人一次只看一个域（agent / queue / storage
//! / security / monitor / hooks …），字段路径与错误信息仍然是集中的。

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use super::{ConfigError, ModelsFile, parse_duration_secs};

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
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>) {
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

// ---------------------------------------------------------------------------
// [agent]
// ---------------------------------------------------------------------------

/// 代理运行模式：替身或伙伴。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    /// 使用主人自己的 QQ，替主人说话。
    #[default]
    StandIn,
    /// 使用另一个账号，作为有独立人格的伙伴。
    Companion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    pub mode: AgentMode,
    /// companion 必填；stand_in 由登录账号决定。
    pub owner_qq: u64,
    pub owner_name: String,
    pub available_models: Vec<String>,
    pub model: String,
    pub thinking_level: String,
    pub compaction_model: String,
    pub max_output_tokens: u32,
    pub fallback_models: Vec<String>,
    pub retry_attempts: u32,
    pub retry_base_ms: u64,
    pub retry_max_ms: u64,
    /// owner 通知目标：`self` 或 QQ 号。
    pub master: String,
    pub receive_scopes: Vec<String>,
    pub send_scopes: Vec<String>,
    pub max_steps: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: "ScapegoatBot".to_owned(),
            mode: AgentMode::StandIn,
            owner_qq: 0,
            owner_name: String::new(),
            available_models: vec![
                "anthropic/claude-opus-5-5".to_owned(),
                "anthropic/claude-haiku-5-5".to_owned(),
                "deepseek/deepseek-chat".to_owned(),
            ],
            model: "anthropic/claude-opus-5-5".to_owned(),
            thinking_level: "high".to_owned(),
            compaction_model: "anthropic/claude-haiku-5-5".to_owned(),
            max_output_tokens: 8192,
            fallback_models: Vec::new(),
            retry_attempts: 3,
            retry_base_ms: 400,
            retry_max_ms: 30_000,
            master: "self".to_owned(),
            receive_scopes: vec!["*".to_owned()],
            send_scopes: vec!["*".to_owned()],
            max_steps: 64,
        }
    }
}

impl AgentConfig {
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if self.name.trim().is_empty() {
            errors.push(ConfigError::new("agent.name", "must not be empty"));
        }
        if self.owner_name.chars().count() > 64 {
            errors.push(ConfigError::new(
                "agent.owner_name",
                "must be at most 64 characters",
            ));
        }
        if self.master != "self" && self.master.parse::<u64>().map_or(true, |qq| qq == 0) {
            errors.push(ConfigError::new(
                "agent.master",
                "must be \"self\" or a non-zero QQ number",
            ));
        }
        if self.available_models.is_empty() {
            errors.push(ConfigError::new(
                "agent.available_models",
                "must list at least one model",
            ));
        }
        let mut seen = HashSet::new();
        for model in &self.available_models {
            if model.trim().is_empty() || !model.contains('/') {
                errors.push(ConfigError::new(
                    "agent.available_models",
                    format!("{model:?} must be in the form provider/model"),
                ));
            }
            if !seen.insert(model.as_str()) {
                errors.push(ConfigError::new(
                    "agent.available_models",
                    format!("{model:?} is listed more than once"),
                ));
            }
        }
        if !self.available_models.contains(&self.model) {
            errors.push(ConfigError::new(
                "agent.model",
                format!("{:?} is not in agent.available_models", self.model),
            ));
        }
        if !self.available_models.contains(&self.compaction_model) {
            errors.push(ConfigError::new(
                "agent.compaction_model",
                format!(
                    "{:?} is not in agent.available_models",
                    self.compaction_model
                ),
            ));
        }
        for model in &self.fallback_models {
            if !self.available_models.contains(model) {
                errors.push(ConfigError::new(
                    "agent.fallback_models",
                    format!("{model:?} is not in agent.available_models"),
                ));
            }
            if model == &self.model {
                errors.push(ConfigError::new(
                    "agent.fallback_models",
                    "must not contain the primary model",
                ));
            }
        }
        if self.retry_attempts == 0 {
            errors.push(ConfigError::new(
                "agent.retry_attempts",
                "must be greater than zero",
            ));
        }
        if self.retry_base_ms == 0 || self.retry_max_ms == 0 {
            errors.push(ConfigError::new(
                "agent.retry_base_ms / agent.retry_max_ms",
                "must be greater than zero",
            ));
        }
        if self.max_steps == 0 {
            errors.push(ConfigError::new(
                "agent.max_steps",
                "must be greater than zero",
            ));
        }
        if self.max_output_tokens == 0 {
            errors.push(ConfigError::new(
                "agent.max_output_tokens",
                "must be greater than zero",
            ));
        }
        validate_scopes("agent.receive_scopes", &self.receive_scopes, errors);
        validate_scopes("agent.send_scopes", &self.send_scopes, errors);
    }

    /// 需要模型清单的校验。
    pub(super) fn validate_with_models(&self, models: &ModelsFile, errors: &mut Vec<ConfigError>) {
        let Some(model) = models.find(&self.model) else {
            errors.push(ConfigError::new(
                "agent.model",
                format!("{:?} is not defined in models.json", self.model),
            ));
            return;
        };

        if !model.model.supports("tools") {
            // 没有工具能力的模型无法驱动 agent；显式拒绝，避免运行时才暴露。
            errors.push(ConfigError::new(
                "agent.model",
                format!("{:?} does not declare the \"tools\" capability", self.model),
            ));
        }

        if !model.model.thinking_levels.is_empty()
            && !model.model.thinking_levels.contains(&self.thinking_level)
        {
            errors.push(ConfigError::new(
                "agent.thinking_level",
                format!(
                    "{:?} is not supported by {:?}; supported: {:?}",
                    self.thinking_level, self.model, model.model.thinking_levels
                ),
            ));
        }

        for name in std::iter::once(&self.compaction_model).chain(self.fallback_models.iter()) {
            check_available(models, &self.available_models, name, "agent", errors);
        }

        if self.max_output_tokens > model.model.max_tokens {
            errors.push(ConfigError::new(
                "agent.max_output_tokens",
                format!(
                    "{} exceeds the model's max_tokens ({})",
                    self.max_output_tokens, model.model.max_tokens
                ),
            ));
        }
    }
}

/// 校验 scope 列表：`*`、`group:<gid>`、`private:<qq>`。
pub(super) fn validate_scopes(field: &str, scopes: &[String], errors: &mut Vec<ConfigError>) {
    if scopes.is_empty() {
        // 空表示拒绝一切；这是合法配置（`send_scopes = []` 即禁发）。
        return;
    }
    for scope in scopes {
        if scope == "*" {
            continue;
        }
        let ok = scope
            .strip_prefix("group:")
            .or_else(|| scope.strip_prefix("private:"))
            .is_some_and(|id| id.parse::<u64>().is_ok_and(|id| id != 0));
        if !ok {
            errors.push(ConfigError::new(
                field,
                format!("{scope:?} must be \"*\", \"group:<gid>\" or \"private:<qq>\""),
            ));
        }
    }
}

/// 检查模型是否既在 `available_models` 白名单里、又存在于 `models.json`。
pub(super) fn check_available(
    models: &ModelsFile,
    available: &[String],
    name: &str,
    field: &str,
    errors: &mut Vec<ConfigError>,
) {
    if !available.iter().any(|item| item == name) {
        errors.push(ConfigError::new(
            field,
            format!("{name:?} is not in agent.available_models"),
        ));
    } else if models.find(name).is_none() {
        errors.push(ConfigError::new(
            field,
            format!("{name:?} is not defined in models.json"),
        ));
    }
}

// ---------------------------------------------------------------------------
// [napcat] / [filter] / [session] / [queue]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct FilterConfig {
    pub ignore_groups: Vec<u64>,
    pub ignore_users: Vec<u64>,
    pub ignore_patterns: Vec<String>,
    pub require_mention: bool,
}

impl FilterConfig {
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if self.ignore_groups.contains(&0) {
            errors.push(ConfigError::new(
                "filter.ignore_groups",
                "group ids must be non-zero",
            ));
        }
        if self.ignore_users.contains(&0) {
            errors.push(ConfigError::new(
                "filter.ignore_users",
                "user ids must be non-zero",
            ));
        }
        for pattern in &self.ignore_patterns {
            if let Err(error) = regex::Regex::new(pattern) {
                errors.push(ConfigError::new(
                    "filter.ignore_patterns",
                    format!("invalid regex {pattern:?}: {error}"),
                ));
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionScope {
    /// 全局一份 transcript（默认）。
    #[default]
    Shared,
    /// 按 chat 隔离。
    PerChat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    pub scope: SessionScope,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            scope: SessionScope::Shared,
        }
    }
}

impl SessionConfig {
    pub(super) fn validate(&self, _errors: &mut Vec<ConfigError>) {
        // 枚举已由 serde 约束；保留方法以便未来增加取值相关的检查。
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QueueMode {
    #[default]
    Steer,
    Followup,
    Collect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DropPolicy {
    #[default]
    Summarize,
    Old,
    New,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueueConfig {
    pub mode: QueueMode,
    pub max_concurrent: usize,
    pub cap_messages: usize,
    pub cap_batches: usize,
    pub drop: DropPolicy,
    pub debounce_ms: u64,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            mode: QueueMode::Steer,
            max_concurrent: 4,
            cap_messages: 200,
            cap_batches: 20,
            drop: DropPolicy::Summarize,
            debounce_ms: 3_000,
        }
    }
}

impl QueueConfig {
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if self.max_concurrent == 0 {
            errors.push(ConfigError::new(
                "queue.max_concurrent",
                "must be greater than zero",
            ));
        }
        if self.cap_messages == 0 {
            errors.push(ConfigError::new(
                "queue.cap_messages",
                "must be greater than zero",
            ));
        }
        if self.cap_batches == 0 {
            errors.push(ConfigError::new(
                "queue.cap_batches",
                "must be greater than zero",
            ));
        }
        if self.debounce_ms == 0 {
            errors.push(ConfigError::new(
                "queue.debounce_ms",
                "must be greater than zero",
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// [storage] / [tools] / [mcp]
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
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if self.path.trim().is_empty() {
            errors.push(ConfigError::new("storage.path", "must not be empty"));
        }
        if self.max_blob_bytes == 0 {
            errors.push(ConfigError::new(
                "storage.max_blob_bytes",
                "must be greater than zero",
            ));
        }
    }
}

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
}

impl Default for WebFetchConfig {
    fn default() -> Self {
        Self {
            backend: WebBackend::Auto,
            base_url: String::new(),
            max_content_tokens: 20_000,
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
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if self.max_result_bytes == 0 {
            errors.push(ConfigError::new(
                "tools.max_result_bytes",
                "must be greater than zero",
            ));
        }
        if self.max_schema_tokens == 0 {
            errors.push(ConfigError::new(
                "tools.max_schema_tokens",
                "must be greater than zero",
            ));
        }
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
        if self.web_search.count == 0 {
            errors.push(ConfigError::new(
                "tools.web_search.count",
                "must be greater than zero",
            ));
        }
        if self.web_fetch.max_content_tokens == 0 {
            errors.push(ConfigError::new(
                "tools.web_fetch.max_content_tokens",
                "must be greater than zero",
            ));
        }
        if self.download.max_bytes == 0 {
            errors.push(ConfigError::new(
                "tools.download.max_bytes",
                "must be greater than zero",
            ));
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    pub enabled: bool,
    /// 外部 MCP 服务器；默认空（不连接任何进程）。
    pub servers: Vec<McpServerConfig>,
}

impl McpConfig {
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>) {
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

// ---------------------------------------------------------------------------
// [memory] / [skills] / [style] / [swarm]
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
    pub review_model: String,
    pub topics: bool,
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
            topics: true,
        }
    }
}

impl MemoryConfig {
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>) {
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
            if value == 0 {
                errors.push(ConfigError::new(field, "must be greater than zero"));
            }
        }
    }
}

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
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>) {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StyleConfig {
    pub enabled: bool,
    pub profile_path: String,
    pub max_chars: u32,
    pub sample_retention_days: u32,
}

impl Default for StyleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            profile_path: "/STYLE.md".to_owned(),
            max_chars: 2_000,
            sample_retention_days: 30,
        }
    }
}

impl StyleConfig {
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if self.max_chars == 0 {
            errors.push(ConfigError::new(
                "style.max_chars",
                "must be greater than zero",
            ));
        }
        if !self.profile_path.starts_with('/') {
            errors.push(ConfigError::new(
                "style.profile_path",
                "must be an absolute VFS path",
            ));
        }
    }
}

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
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>) {
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

// ---------------------------------------------------------------------------
// [[monitor]]
// ---------------------------------------------------------------------------

/// QQ 事件源。支持 `qq = true` 与 `qq = { backlog = "inbox" }` 两种写法。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum QqSource {
    Enabled(bool),
    Detailed(QqSourceConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QqSourceConfig {
    pub backlog: BacklogMode,
}

impl Default for QqSourceConfig {
    fn default() -> Self {
        Self {
            backlog: BacklogMode::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BacklogMode {
    /// 只跟新事件。
    #[default]
    None,
    /// 创建时把 `/inbox?filter=<when>` 的结果投递一次。
    Inbox,
}

/// 定时源：`cron` / `every` / `at` 三选一。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct TimerSource {
    /// 本地时区 5 字段 cron。
    pub cron: Option<String>,
    /// 固定间隔，最小 60s。
    pub every: Option<String>,
    /// 一次性本地时间（`YYYY-MM-DD HH:MM:SS`）。
    pub at: Option<String>,
    /// 抖动上限（秒）；0 = 服务端按周期 10%（上限 15 分钟）自动计算。
    pub jitter_secs: Option<u64>,
}

/// 外部流源（只能由主人写进配置文件）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StreamSource {
    pub url: String,
    pub kind: StreamKind,
}

impl Default for StreamSource {
    fn default() -> Self {
        Self {
            url: String::new(),
            kind: StreamKind::Websocket,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    #[default]
    Websocket,
    Sse,
}

/// 内部状态变化源。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct InternalSource {
    /// `agent` / `security` / `budget` / `storage` 之一。
    pub kind: String,
}

/// 一个 monitor 只能有一种 source；校验阶段保证恰好一个非空。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct MonitorSource {
    pub qq: Option<QqSource>,
    pub timer: Option<TimerSource>,
    pub stream: Option<StreamSource>,
    pub internal: Option<InternalSource>,
}

impl MonitorSource {
    /// 返回 source 类型名，用于错误信息与 `Monitor list`。
    pub fn kind(&self) -> Option<&'static str> {
        match (
            self.qq.is_some(),
            self.timer.is_some(),
            self.stream.is_some(),
            self.internal.is_some(),
        ) {
            (true, false, false, false) => Some("qq"),
            (false, true, false, false) => Some("timer"),
            (false, false, true, false) => Some("stream"),
            (false, false, false, true) => Some("internal"),
            _ => None,
        }
    }
}

/// 投递目标不存在时的处理方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnMissing {
    /// 丢弃并告警。
    Drop,
    /// 排队等待目标出现。
    #[default]
    Queue,
    /// 为目标创建常驻子代理。
    Spawn,
}

/// 投递形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeliverAs {
    /// 原样投递事件。
    #[default]
    Event,
    /// 投递渲染后的指令（`prompt` 字段）。
    Prompt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeliverConfig {
    /// `main` / `agent:<name>` / `<spawn_id>`。
    pub to: String,
    #[serde(rename = "as")]
    pub as_: DeliverAs,
    /// `as = "prompt"` 时的模板，支持 `{{event.*}}` 与 `{{count}}`。
    pub prompt: String,
    pub batch_window_ms: u64,
    /// 是否抄送给父代理。
    pub cc: bool,
}

impl Default for DeliverConfig {
    fn default() -> Self {
        Self {
            to: "main".to_owned(),
            as_: DeliverAs::Event,
            prompt: String::new(),
            batch_window_ms: 200,
            cc: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CatchUp {
    /// 错过多次只补一次。
    #[default]
    Coalesce,
    /// 直接跳过错过的触发。
    Skip,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LifecycleConfig {
    pub idle_only: bool,
    pub min_interval_secs: u64,
    pub max_events: u64,
    /// 时长字符串；`0` = 不过期。
    pub expires: String,
    pub one_shot: bool,
    pub catch_up: CatchUp,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            idle_only: false,
            min_interval_secs: 0,
            max_events: 1_000,
            expires: "0".to_owned(),
            one_shot: false,
            catch_up: CatchUp::Coalesce,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MonitorConfig {
    pub name: String,
    pub enabled: bool,
    pub source: MonitorSource,
    /// 谓词表达式；省略 = 该 source 的全部事件。
    pub when: Option<String>,
    pub on_missing: OnMissing,
    pub deliver: DeliverConfig,
    pub lifecycle: LifecycleConfig,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            source: MonitorSource::default(),
            when: None,
            on_missing: OnMissing::Queue,
            deliver: DeliverConfig::default(),
            lifecycle: LifecycleConfig::default(),
        }
    }
}

/// 校验全部 monitor；错误带 `monitor[i]` 位置。
pub(super) fn validate_monitors(monitors: &[MonitorConfig], errors: &mut Vec<ConfigError>) {
    let mut names = HashSet::new();
    for (index, monitor) in monitors.iter().enumerate() {
        let field = format!("monitor[{index}]");

        if monitor.name.trim().is_empty() {
            errors.push(ConfigError::new(
                format!("{field}.name"),
                "must not be empty",
            ));
        } else if !names.insert(monitor.name.as_str()) {
            errors.push(ConfigError::new(
                format!("{field}.name"),
                format!("duplicate monitor name {:?}", monitor.name),
            ));
        }

        let Some(kind) = monitor.source.kind() else {
            errors.push(ConfigError::new(
                format!("{field}.source"),
                "must set exactly one of \"qq\", \"timer\", \"stream\" or \"internal\"",
            ));
            continue;
        };

        if let Some(timer) = &monitor.source.timer {
            let set = [
                timer.cron.is_some(),
                timer.every.is_some(),
                timer.at.is_some(),
            ]
            .into_iter()
            .filter(|present| *present)
            .count();
            if set != 1 {
                errors.push(ConfigError::new(
                    format!("{field}.source.timer"),
                    "must set exactly one of \"cron\", \"every\" or \"at\"",
                ));
            }
            if let Some(every) = &timer.every {
                match parse_duration_secs(every) {
                    Some(secs) if secs >= 60 => {}
                    Some(_) => errors.push(ConfigError::new(
                        format!("{field}.source.timer.every"),
                        "must be at least 60s",
                    )),
                    None => errors.push(ConfigError::new(
                        format!("{field}.source.timer.every"),
                        format!("invalid duration {every:?}"),
                    )),
                }
            }
        }

        if let Some(stream) = &monitor.source.stream
            && !(stream.url.starts_with("ws://")
                || stream.url.starts_with("wss://")
                || stream.url.starts_with("http://")
                || stream.url.starts_with("https://"))
        {
            errors.push(ConfigError::new(
                format!("{field}.source.stream.url"),
                "must be an http(s) or ws(s) URL",
            ));
        }

        if let Some(internal) = &monitor.source.internal
            && !["agent", "security", "budget", "storage"].contains(&internal.kind.as_str())
        {
            errors.push(ConfigError::new(
                format!("{field}.source.internal.kind"),
                "must be one of \"agent\", \"security\", \"budget\" or \"storage\"",
            ));
        }

        if let Some(when) = &monitor.when
            && when.trim().is_empty()
        {
            errors.push(ConfigError::new(
                format!("{field}.when"),
                "must not be empty; omit the field to match everything",
            ));
        }

        if monitor.deliver.to.trim().is_empty() {
            errors.push(ConfigError::new(
                format!("{field}.deliver.to"),
                "must not be empty",
            ));
        } else if monitor.deliver.to != "main"
            && let Some(name) = monitor.deliver.to.strip_prefix("agent:")
            && name.is_empty()
        {
            errors.push(ConfigError::new(
                format!("{field}.deliver.to"),
                "\"agent:\" requires a name",
            ));
        }

        if monitor.deliver.as_ == DeliverAs::Prompt && monitor.deliver.prompt.trim().is_empty() {
            errors.push(ConfigError::new(
                format!("{field}.deliver.prompt"),
                "is required when deliver.as = \"prompt\"",
            ));
        }

        if monitor.deliver.batch_window_ms == 0 {
            errors.push(ConfigError::new(
                format!("{field}.deliver.batch_window_ms"),
                "must be greater than zero",
            ));
        }

        match parse_duration_secs(&monitor.lifecycle.expires) {
            Some(_) => {}
            None => errors.push(ConfigError::new(
                format!("{field}.lifecycle.expires"),
                format!("invalid duration {:?}", monitor.lifecycle.expires),
            )),
        }

        if monitor.lifecycle.one_shot && kind != "timer" {
            errors.push(ConfigError::new(
                format!("{field}.lifecycle.one_shot"),
                "one_shot requires a timer source",
            ));
        }

        if monitor.lifecycle.max_events == 0 {
            errors.push(ConfigError::new(
                format!("{field}.lifecycle.max_events"),
                "must be greater than zero",
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// [security]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GuardMode {
    /// 只记录判定，不触发 hold。
    #[default]
    Shadow,
    /// 命中即 hold。
    Enforce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GuardBackend {
    #[default]
    Jev,
    Llm,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuardConfig {
    pub enabled: bool,
    pub mode: GuardMode,
    pub backend: GuardBackend,
    /// 空 = 后端默认模型。
    pub model: String,
    /// 空 = 后端官方地址。
    pub base_url: String,
    /// dangerous 概率阈值。
    pub threshold: f64,
    pub timeout_ms: u64,
    pub max_state_tokens: u32,
    pub delta_every_tokens: u32,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: GuardMode::Shadow,
            backend: GuardBackend::Jev,
            model: String::new(),
            base_url: String::new(),
            threshold: 0.9,
            timeout_ms: 800,
            max_state_tokens: 8_000,
            delta_every_tokens: 200,
        }
    }
}

impl GuardConfig {
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if !(0.5..=0.99).contains(&self.threshold) {
            errors.push(ConfigError::new(
                "security.guard.threshold",
                "must be between 0.5 and 0.99",
            ));
        }
        if !(50..=5_000).contains(&self.timeout_ms) {
            errors.push(ConfigError::new(
                "security.guard.timeout_ms",
                "must be between 50 and 5000",
            ));
        }
        if !(1_000..=20_000).contains(&self.max_state_tokens) {
            errors.push(ConfigError::new(
                "security.guard.max_state_tokens",
                "must be between 1000 and 20000",
            ));
        }
        if self.delta_every_tokens < 50 {
            errors.push(ConfigError::new(
                "security.guard.delta_every_tokens",
                "must be at least 50",
            ));
        }
        if self.backend == GuardBackend::Llm {
            errors.push(ConfigError::new(
                "security.guard.backend",
                "`llm` is not implemented yet; use `jev` (with base_url and auth.json key) or `off`",
            ));
        }
        if self.backend == GuardBackend::Jev && !self.base_url.is_empty() {
            let ok = self.base_url.starts_with("https://") || self.base_url.starts_with("http://");
            if !ok {
                errors.push(ConfigError::new(
                    "security.guard.base_url",
                    "must start with http:// or https://",
                ));
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    pub check_every_tokens: u64,
    pub check_on_compaction: bool,
    pub check_overlap_tokens: u64,
    pub review_model: String,
    pub check_timeout_secs: u64,
    pub guard: GuardConfig,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            check_every_tokens: 262_144,
            check_on_compaction: true,
            check_overlap_tokens: 4_096,
            review_model: String::new(),
            check_timeout_secs: 60,
            guard: GuardConfig::default(),
        }
    }
}

impl SecurityConfig {
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if self.check_timeout_secs == 0 {
            errors.push(ConfigError::new(
                "security.check_timeout_secs",
                "must be greater than zero",
            ));
        }
        self.guard.validate(errors);
    }
}

// ---------------------------------------------------------------------------
// [budget] / [gate] / [compaction]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    pub daily_tokens: u64,
    pub daily_usd: f64,
    pub turn_tokens: u64,
    pub chat_sends_per_hour: u32,
    pub chat_turns_per_hour: u32,
    pub round_trip_wall_secs: u64,
    pub guard_usd_per_day: f64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            daily_tokens: 2_000_000,
            daily_usd: 10.0,
            turn_tokens: 150_000,
            chat_sends_per_hour: 60,
            chat_turns_per_hour: 120,
            round_trip_wall_secs: 600,
            guard_usd_per_day: 0.20,
        }
    }
}

impl BudgetConfig {
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>, compaction: &CompactionConfig) {
        if !self.daily_usd.is_finite() || self.daily_usd < 0.0 {
            errors.push(ConfigError::new(
                "budget.daily_usd",
                "must be a non-negative number",
            ));
        }
        if !self.guard_usd_per_day.is_finite() || self.guard_usd_per_day < 0.0 {
            errors.push(ConfigError::new(
                "budget.guard_usd_per_day",
                "must be a non-negative number",
            ));
        }
        if self.chat_sends_per_hour == 0 || self.chat_turns_per_hour == 0 {
            errors.push(ConfigError::new(
                "budget.chat_*_per_hour",
                "must be greater than zero",
            ));
        }
        if self.round_trip_wall_secs == 0 {
            errors.push(ConfigError::new(
                "budget.round_trip_wall_secs",
                "must be greater than zero",
            ));
        }
        // `turn_tokens` 是回合累计用量，`turn_context_budget` 是单次请求工作集；
        // 两者量纲不同，不设大小关系（设计 §9.6 已据此修正）。
        let _ = compaction;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GateMode {
    /// 不做逐条审查。
    Trusted,
    /// 规则检查（+ 可选 LLM 审查）。
    #[default]
    Auto,
    /// 每条都需要主人确认。
    Approve,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GateConfig {
    pub mode: GateMode,
    pub max_per_minute: u32,
    pub llm_review: bool,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            mode: GateMode::Auto,
            max_per_minute: 20,
            llm_review: false,
        }
    }
}

impl GateConfig {
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if self.max_per_minute == 0 {
            errors.push(ConfigError::new(
                "gate.max_per_minute",
                "must be greater than zero",
            ));
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionConfig {
    pub reserve_tokens: u32,
    pub turn_context_budget: u32,
    pub keep_recent_tokens: u32,
    pub max_failures: u32,
    pub memory_flush: bool,
    pub post_compact_recall: bool,
    pub prune: bool,
    pub prune_soft_limit: u32,
    pub prune_keep_turns: u32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            reserve_tokens: 16_384,
            turn_context_budget: 120_000,
            keep_recent_tokens: 20_000,
            max_failures: 3,
            memory_flush: true,
            post_compact_recall: true,
            prune: true,
            prune_soft_limit: 4_000,
            prune_keep_turns: 3,
        }
    }
}

impl CompactionConfig {
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>, agent: &AgentConfig) {
        for (field, value) in [
            ("compaction.reserve_tokens", self.reserve_tokens),
            ("compaction.turn_context_budget", self.turn_context_budget),
            ("compaction.keep_recent_tokens", self.keep_recent_tokens),
            ("compaction.max_failures", self.max_failures),
            ("compaction.prune_soft_limit", self.prune_soft_limit),
            ("compaction.prune_keep_turns", self.prune_keep_turns),
        ] {
            if value == 0 {
                errors.push(ConfigError::new(field, "must be greater than zero"));
            }
        }
        if self.reserve_tokens < agent.max_output_tokens {
            errors.push(ConfigError::new(
                "compaction.reserve_tokens",
                format!(
                    "must be at least agent.max_output_tokens ({})",
                    agent.max_output_tokens
                ),
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// [hooks]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookTrigger {
    SessionStart,
    PostCompact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    Owner,
    User,
    Memory,
    Style,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectMode {
    System,
    UserPrefix,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextHookConfig {
    pub name: String,
    /// VFS 路径或合法 glob。
    pub path: String,
    pub kind: ContextKind,
    pub when: Vec<HookTrigger>,
    pub inject: InjectMode,
    /// 可选字节上限；缺省按 kind 的默认预算。
    pub max_bytes: Option<u64>,
    /// 可选字符上限；与 `max_bytes` 同时存在时取先触发者。
    pub max_chars: Option<u32>,
    pub enabled: bool,
}

impl Default for ContextHookConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            path: String::new(),
            kind: ContextKind::Owner,
            when: vec![HookTrigger::SessionStart, HookTrigger::PostCompact],
            inject: InjectMode::System,
            max_bytes: None,
            max_chars: None,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    SessionEnd,
    PreCompact,
    PostCompact,
    MessageReceived,
    BeforeSend,
    Stop,
    ConfigChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookAction {
    Log,
    Block,
    Rewrite,
    InjectContext,
    Notify,
}

/// 事件 hook 的匹配条件；字段全部可选，全部为 `None` 表示匹配一切。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct HookMatch {
    pub group_id: Option<u64>,
    pub user_id: Option<u64>,
    pub message_type: Option<String>,
    pub tool_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventHookConfig {
    pub event: HookEvent,
    pub action: HookAction,
    #[serde(rename = "match")]
    pub match_: HookMatch,
    /// `block` 的原因。
    pub reason: String,
    /// `rewrite` 的正则与替换。
    pub pattern: String,
    pub replacement: String,
    /// `inject_context` 的来源。
    pub path: String,
    pub text: String,
    /// `notify` 的渠道；`owner` 才会给主人发私聊。
    pub notify_channel: String,
    /// `BeforeSend` 的作用域：`model`（默认）或 `all`。
    pub scope: String,
    pub enabled: bool,
}

impl Default for EventHookConfig {
    fn default() -> Self {
        Self {
            event: HookEvent::SessionStart,
            action: HookAction::Log,
            match_: HookMatch::default(),
            reason: String::new(),
            pattern: String::new(),
            replacement: String::new(),
            path: String::new(),
            text: String::new(),
            notify_channel: String::new(),
            scope: "model".to_owned(),
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HooksConfig {
    pub context: Vec<ContextHookConfig>,
    pub events: Vec<EventHookConfig>,
}

impl Default for HooksConfig {
    fn default() -> Self {
        // 设计 §7.4 的默认 hook：AGENTS / USER / MEMORY / STYLE 各一条。
        Self {
            context: vec![
                ContextHookConfig {
                    name: "agents".to_owned(),
                    path: "/AGENTS.md".to_owned(),
                    kind: ContextKind::Owner,
                    max_bytes: Some(65_536),
                    ..ContextHookConfig::default()
                },
                ContextHookConfig {
                    name: "user".to_owned(),
                    path: "/USER.md".to_owned(),
                    kind: ContextKind::User,
                    max_chars: Some(4_000),
                    ..ContextHookConfig::default()
                },
                ContextHookConfig {
                    name: "memory".to_owned(),
                    path: "/MEMORY.md".to_owned(),
                    kind: ContextKind::Memory,
                    max_bytes: Some(25_000),
                    ..ContextHookConfig::default()
                },
                ContextHookConfig {
                    name: "style".to_owned(),
                    path: "/STYLE.md".to_owned(),
                    kind: ContextKind::Style,
                    max_chars: Some(2_000),
                    ..ContextHookConfig::default()
                },
            ],
            events: Vec::new(),
        }
    }
}

impl HooksConfig {
    pub(super) fn validate(&self, errors: &mut Vec<ConfigError>) {
        let mut names = HashSet::new();
        for (index, hook) in self.context.iter().enumerate() {
            let field = format!("hooks.context[{index}]");
            if hook.name.trim().is_empty() {
                errors.push(ConfigError::new(
                    format!("{field}.name"),
                    "must not be empty",
                ));
            } else if !names.insert(hook.name.as_str()) {
                errors.push(ConfigError::new(
                    format!("{field}.name"),
                    format!("duplicate hook name {:?}", hook.name),
                ));
            }
            if !hook.path.starts_with('/') {
                errors.push(ConfigError::new(
                    format!("{field}.path"),
                    "must be an absolute VFS path or glob",
                ));
            }
            if hook.when.is_empty() {
                errors.push(ConfigError::new(
                    format!("{field}.when"),
                    "must list at least one trigger",
                ));
            }
            if hook.max_bytes == Some(0) || hook.max_chars == Some(0) {
                errors.push(ConfigError::new(
                    format!("{field}.max_bytes / {field}.max_chars"),
                    "must be greater than zero when set",
                ));
            }
        }

        // 同一事件最多 16 条；动作参数必须齐全。
        let mut per_event = std::collections::HashMap::new();
        for (index, hook) in self.events.iter().enumerate() {
            let field = format!("hooks.events[{index}]");
            let count = per_event
                .entry(format!("{:?}", hook.event))
                .or_insert(0usize);
            *count += 1;
            if *count > 16 {
                errors.push(ConfigError::new(
                    format!("{field}.event"),
                    "at most 16 hooks may target the same event",
                ));
            }
            match hook.action {
                HookAction::Block => {
                    if hook.reason.trim().is_empty() {
                        errors.push(ConfigError::new(
                            format!("{field}.reason"),
                            "is required when action = \"block\"",
                        ));
                    }
                }
                HookAction::Rewrite => {
                    if hook.pattern.is_empty() {
                        errors.push(ConfigError::new(
                            format!("{field}.pattern"),
                            "is required when action = \"rewrite\"",
                        ));
                    } else if let Err(error) = regex::Regex::new(&hook.pattern) {
                        errors.push(ConfigError::new(
                            format!("{field}.pattern"),
                            format!("invalid regex: {error}"),
                        ));
                    }
                }
                HookAction::InjectContext => {
                    if hook.path.is_empty() && hook.text.is_empty() {
                        errors.push(ConfigError::new(
                            field.to_string(),
                            "inject_context requires \"path\" or \"text\"",
                        ));
                    }
                }
                HookAction::Log | HookAction::Notify => {}
            }
            if !matches!(hook.scope.as_str(), "model" | "all") {
                errors.push(ConfigError::new(
                    format!("{field}.scope"),
                    "must be \"model\" or \"all\"",
                ));
            }
            // 动作与事件的组合是否可用，见设计 §7.5 的表格。
            if !action_allowed_on(hook.event, hook.action) {
                errors.push(ConfigError::new(
                    format!("{field}.action"),
                    format!("{:?} is not allowed on {:?}", hook.action, hook.event),
                ));
            }
        }
    }
}

/// 设计 §7.5 的事件 / 动作矩阵。
fn action_allowed_on(event: HookEvent, action: HookAction) -> bool {
    use HookAction::*;
    use HookEvent::*;
    matches!(
        (event, action),
        (SessionStart, InjectContext)
            | (SessionStart, Log)
            | (SessionEnd, Log)
            | (SessionEnd, Notify)
            | (PreCompact, Log)
            | (PreCompact, InjectContext)
            | (PostCompact, InjectContext)
            | (PostCompact, Log)
            | (MessageReceived, InjectContext)
            | (MessageReceived, Log)
            | (MessageReceived, Block)
            | (BeforeSend, Block)
            | (BeforeSend, Rewrite)
            | (BeforeSend, Log)
            | (Stop, Log)
            | (ConfigChange, Log)
    )
}

