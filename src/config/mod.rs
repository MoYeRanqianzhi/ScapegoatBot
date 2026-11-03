//! `sbot.toml` 的完整 schema、加载与启动校验。
//!
//! 设计依据：`.agents/plan/2026-10-28-rebuild-design.md` §9.1 / §9.2 / §9.6。
//!
//! 三条硬规则：
//! 1. **严格 schema**：`version = 1`，未知字段直接报错，所有字段都有显式默认值。
//! 2. **零凭据**：本文件不出现任何 key/token；凭据只在 `auth.json`（见 `config::auth`）。
//! 3. **可解释错误**：每条错误带字段路径（如 `security.guard.threshold`），一次性列出全部问题。
//!
//! 校验分两层：
//! - [`SbotConfig::validate_shape`]：只依赖配置文件本身，启动前即可运行；
//! - [`SbotConfig::validate_with_models`]：需要 `models.json`（模型是否存在、effort 是否支持）。

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod auth;
pub mod models;
pub mod secret;

pub use auth::AuthFile;
pub use models::ModelsFile;

/// 配置版本；不匹配直接拒绝启动。
pub const CONFIG_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// 错误类型
// ---------------------------------------------------------------------------

/// 单条校验错误：字段路径 + 说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// 配置内的字段路径，例如 `agent.thinking_level`。
    pub field: String,
    /// 人类可读说明（英文，便于跨语言协作）。
    pub message: String,
}

impl ConfigError {
    fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

/// 一次性报告全部校验错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigErrors(pub Vec<ConfigError>);

impl fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str("\n")?;
            }
            write!(f, "  - {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigErrors {}

/// 加载失败：读文件、解析、校验三类。
#[derive(Debug, thiserror::Error)]
pub enum ConfigLoadError {
    #[error("cannot read config file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse config file {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("config validation failed:\n{0}")]
    Invalid(ConfigErrors),
}

// ---------------------------------------------------------------------------
// 顶层结构
// ---------------------------------------------------------------------------

/// `sbot.toml` 的根结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SbotConfig {
    /// 必须为 1。
    pub version: u32,
    /// `models.json` 路径；`~/.sbot` 指本实例根目录。
    pub models_file: String,
    /// `auth.json` 路径。
    pub auth_file: String,
    /// 进程间通信端点。
    pub ipc: IpcConfig,
    /// 主代理身份与模型选择。
    pub agent: AgentConfig,
    /// NapCat 连接。
    pub napcat: NapcatConfig,
    /// 消息准入门（拒收，不是静音）。
    pub filter: FilterConfig,
    /// 上下文作用域。
    pub session: SessionConfig,
    /// 回合队列与去抖。
    pub queue: QueueConfig,
    /// 存储与配额。
    pub storage: StorageConfig,
    /// 工具预算。
    pub tools: ToolsConfig,
    /// MCP（可选，默认关闭）。
    pub mcp: McpConfig,
    /// 记忆。
    pub memory: MemoryConfig,
    /// 技能。
    pub skills: SkillsConfig,
    /// 风格画像。
    pub style: StyleConfig,
    /// 子代理。
    pub swarm: SwarmConfig,
    /// 监视器；配置文件里声明的是长期常驻。
    pub monitor: Vec<MonitorConfig>,
    /// 周期安全检查与输入/输出闸。
    pub security: SecurityConfig,
    /// 成本与频率上限。
    pub budget: BudgetConfig,
    /// API 门禁。
    pub gate: GateConfig,
    /// 压缩。
    pub compaction: CompactionConfig,
    /// 上下文与事件 hook。
    pub hooks: HooksConfig,
}

impl Default for SbotConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            models_file: "~/.sbot/models.json".to_owned(),
            auth_file: "~/.sbot/auth.json".to_owned(),
            ipc: IpcConfig::default(),
            agent: AgentConfig::default(),
            napcat: NapcatConfig::default(),
            filter: FilterConfig::default(),
            session: SessionConfig::default(),
            queue: QueueConfig::default(),
            storage: StorageConfig::default(),
            tools: ToolsConfig::default(),
            mcp: McpConfig::default(),
            memory: MemoryConfig::default(),
            skills: SkillsConfig::default(),
            style: StyleConfig::default(),
            swarm: SwarmConfig::default(),
            monitor: Vec::new(),
            security: SecurityConfig::default(),
            budget: BudgetConfig::default(),
            gate: GateConfig::default(),
            compaction: CompactionConfig::default(),
            hooks: HooksConfig::default(),
        }
    }
}

/// 加载后的配置：保留路径信息，供相对路径解析与写回使用。
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// 解析并校验过的配置。
    pub config: SbotConfig,
    /// 配置文件绝对路径。
    pub path: PathBuf,
    /// 配置文件所在目录；相对路径以此为基准。
    pub dir: PathBuf,
}

impl SbotConfig {
    /// 从磁盘加载并做形状校验。
    ///
    /// 环境变量覆盖在解析前注入：`SBOT__AGENT__MODEL=...` 表示 `agent.model`。
    pub fn load(path: impl AsRef<Path>) -> Result<LoadedConfig, ConfigLoadError> {
        let path = path.as_ref().to_path_buf();
        let text = std::fs::read_to_string(&path).map_err(|source| ConfigLoadError::Io {
            path: path.clone(),
            source,
        })?;

        let mut table: toml::Table =
            toml::from_str(&text).map_err(|error| ConfigLoadError::Parse {
                path: path.clone(),
                message: error.to_string(),
            })?;
        apply_env_overrides(&mut table);

        let config: SbotConfig =
            toml::Value::Table(table)
                .try_into()
                .map_err(|error: toml::de::Error| ConfigLoadError::Parse {
                    path: path.clone(),
                    message: error.to_string(),
                })?;

        config.validate_shape().map_err(ConfigLoadError::Invalid)?;

        let dir = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        Ok(LoadedConfig { config, path, dir })
    }

    /// 不依赖 `models.json` 的形状校验。
    pub fn validate_shape(&self) -> Result<(), ConfigErrors> {
        let mut errors = Vec::new();

        if self.version != CONFIG_VERSION {
            errors.push(ConfigError::new(
                "version",
                format!("must be {CONFIG_VERSION}, got {}", self.version),
            ));
        }

        self.ipc.validate(&mut errors);
        self.agent.validate(&mut errors);
        self.filter.validate(&mut errors);
        self.session.validate(&mut errors);
        self.queue.validate(&mut errors);
        self.storage.validate(&mut errors);
        self.tools.validate(&mut errors);
        self.memory.validate(&mut errors);
        self.skills.validate(&mut errors);
        self.style.validate(&mut errors);
        self.swarm.validate(&mut errors);
        self.mcp.validate(&mut errors);
        self.security.validate(&mut errors);
        self.budget.validate(&mut errors, &self.compaction);
        self.gate.validate(&mut errors);
        self.compaction.validate(&mut errors, &self.agent);
        self.hooks.validate(&mut errors);
        validate_monitors(&self.monitor, &mut errors);

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigErrors(errors))
        }
    }

    /// 需要模型清单的校验：模型存在性、effort 支持、输出上限。
    pub fn validate_with_models(&self, models: &ModelsFile) -> Result<(), ConfigErrors> {
        let mut errors = Vec::new();
        self.agent.validate_with_models(models, &mut errors);
        if !self.security.review_model.trim().is_empty() {
            check_available(
                models,
                &self.agent.available_models,
                &self.security.review_model,
                "security.review_model",
                &mut errors,
            );
        }
        if !self.memory.review_model.trim().is_empty() {
            check_available(
                models,
                &self.agent.available_models,
                &self.memory.review_model,
                "memory.review_model",
                &mut errors,
            );
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigErrors(errors))
        }
    }
}

/// 把 `SBOT__A__B=value` 注入 TOML 表。
///
/// 值先尝试按 TOML 字面量解析（支持数字、布尔、数组），失败则当作字符串。
/// 这是可选能力：普通部署直接把值写进 `sbot.toml`。
fn apply_env_overrides(table: &mut toml::Table) {
    let mut overrides: Vec<(Vec<String>, String)> = std::env::vars()
        .filter_map(|(key, value)| {
            let rest = key.strip_prefix("SBOT__")?;
            let parts: Vec<String> = rest
                .split("__")
                .filter(|part| !part.is_empty())
                .map(|part| part.to_ascii_lowercase())
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some((parts, value))
            }
        })
        .collect();

    // 排序保证同一组的注入顺序稳定，便于测试与排错。
    overrides.sort_by(|a, b| a.0.cmp(&b.0));

    for (parts, raw) in overrides {
        let value = raw
            .parse::<toml::Value>()
            .unwrap_or_else(|_| toml::Value::String(raw.clone()));
        insert_path(table, &parts, value);
    }
}

/// 按路径写进嵌套表，缺失的中间层自动创建。
fn insert_path(table: &mut toml::Table, parts: &[String], value: toml::Value) {
    let Some((head, tail)) = parts.split_first() else {
        return;
    };
    if tail.is_empty() {
        table.insert(head.clone(), value);
        return;
    }
    let entry = table
        .entry(head.clone())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let toml::Value::Table(child) = entry {
        insert_path(child, tail, value);
    }
}

/// 解析时长字符串：`0`、`90s`、`10m`、`2h`、`30d`、`1w`。
///
/// 返回秒数；无法解析返回 `None`。`0` 专门表示“不过期/不限制”。
pub fn parse_duration_secs(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw == "0" {
        return Some(0);
    }
    let (digits, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit())?);
    if digits.is_empty() {
        return None;
    }
    let amount: u64 = digits.parse().ok()?;
    let factor = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return None,
    };
    amount.checked_mul(factor)
}

mod sections;
pub use sections::*;

#[cfg(test)]
mod tests {
    use super::*;

    /// 最小可用配置：默认值即可通过形状校验。
    #[test]
    fn defaults_validate() {
        SbotConfig::default().validate_shape().unwrap();
    }

    #[test]
    fn unknown_field_is_rejected() {
        let text = "version = 1\n[agent]\nunknown_key = 1\n";
        let table: toml::Table = toml::from_str(text).unwrap();
        let result: Result<SbotConfig, _> = toml::Value::Table(table).try_into();
        assert!(result.is_err());
    }

    #[test]
    fn version_mismatch_fails() {
        let config = SbotConfig {
            version: 2,
            ..SbotConfig::default()
        };
        let errors = config.validate_shape().unwrap_err();
        assert!(errors.0.iter().any(|e| e.field == "version"));
    }

    #[test]
    fn bad_scope_is_rejected() {
        let mut config = SbotConfig::default();
        config.agent.send_scopes = vec!["group:0".to_owned(), "friend:1".to_owned()];
        let errors = config.validate_shape().unwrap_err();
        assert_eq!(
            errors
                .0
                .iter()
                .filter(|e| e.field == "agent.send_scopes")
                .count(),
            2
        );
    }

    #[test]
    fn guard_threshold_out_of_range() {
        let mut config = SbotConfig::default();
        config.security.guard.threshold = 0.2;
        let errors = config.validate_shape().unwrap_err();
        assert!(
            errors
                .0
                .iter()
                .any(|e| e.field == "security.guard.threshold")
        );
    }

    #[test]
    fn timer_every_must_be_at_least_one_minute() {
        let text = r#"
version = 1
[[monitor]]
name = "fast"
source = { timer = { every = "10s" } }
deliver = { to = "main", as = "event" }
"#;
        let table: toml::Table = toml::from_str(text).unwrap();
        let config: SbotConfig = toml::Value::Table(table).try_into().unwrap();
        let errors = config.validate_shape().unwrap_err();
        assert!(
            errors
                .0
                .iter()
                .any(|e| e.field == "monitor[0].source.timer.every")
        );
    }

    #[test]
    fn monitor_requires_exactly_one_source() {
        let text = r#"
version = 1
[[monitor]]
name = "both"
source = { qq = true, timer = { every = "10m" } }
deliver = { to = "main", as = "event" }
"#;
        let table: toml::Table = toml::from_str(text).unwrap();
        let config: SbotConfig = toml::Value::Table(table).try_into().unwrap();
        let errors = config.validate_shape().unwrap_err();
        assert!(errors.0.iter().any(|e| e.field == "monitor[0].source"));
    }

    #[test]
    fn prompt_delivery_requires_template() {
        let text = r#"
version = 1
[[monitor]]
name = "beat"
source = { timer = { every = "10m" } }
deliver = { to = "main", as = "prompt" }
"#;
        let table: toml::Table = toml::from_str(text).unwrap();
        let config: SbotConfig = toml::Value::Table(table).try_into().unwrap();
        let errors = config.validate_shape().unwrap_err();
        assert!(
            errors
                .0
                .iter()
                .any(|e| e.field == "monitor[0].deliver.prompt")
        );
    }

    #[test]
    fn duration_parser_handles_units() {
        assert_eq!(parse_duration_secs("0"), Some(0));
        assert_eq!(parse_duration_secs("90s"), Some(90));
        assert_eq!(parse_duration_secs("10m"), Some(600));
        assert_eq!(parse_duration_secs("2h"), Some(7_200));
        assert_eq!(parse_duration_secs("30d"), Some(2_592_000));
        assert_eq!(parse_duration_secs("1w"), Some(604_800));
        assert_eq!(parse_duration_secs("10"), None);
        assert_eq!(parse_duration_secs("abc"), None);
        assert_eq!(parse_duration_secs(""), None);
    }

    #[test]
    fn env_override_injects_nested_value() {
        // 用不常出现的键，避免与宿主环境冲突。
        unsafe { std::env::set_var("SBOT__AGENT__THINKING_LEVEL", "low") };
        let mut table: toml::Table = toml::from_str("version = 1").unwrap();
        apply_env_overrides(&mut table);
        unsafe { std::env::remove_var("SBOT__AGENT__THINKING_LEVEL") };
        let config: SbotConfig = toml::Value::Table(table).try_into().unwrap();
        assert_eq!(config.agent.thinking_level, "low");
    }

    #[test]
    fn default_hooks_cover_owner_memory_style() {
        let hooks = HooksConfig::default();
        let kinds: Vec<_> = hooks.context.iter().map(|h| h.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ContextKind::Owner,
                ContextKind::User,
                ContextKind::Memory,
                ContextKind::Style
            ]
        );
        hooks.validate(&mut Vec::new());
    }

    #[test]
    fn rewrite_hook_requires_valid_regex() {
        let mut hooks = HooksConfig::default();
        hooks.events.push(EventHookConfig {
            event: HookEvent::BeforeSend,
            action: HookAction::Rewrite,
            pattern: "(".to_owned(),
            ..EventHookConfig::default()
        });
        let mut errors = Vec::new();
        hooks.validate(&mut errors);
        assert!(errors.iter().any(|e| e.field.contains("pattern")));
    }
}
