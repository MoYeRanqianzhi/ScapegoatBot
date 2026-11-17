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

/// 原子地替换配置文件的内容：临时文件 + fsync + rename，并沿用原文件的权限。
///
/// `sbot.toml` 可能被收紧到 `0600`（MCP 服务器的 `env` 里有 token）；临时文件按默认权限创建
/// 再 rename，会悄悄放宽它。Unix 上临时文件一开始就只给属主读写，写完再复制原文件的权限。
///
/// 调用方负责串行化（运行中的进程里是配置句柄的写锁）：临时文件名只按进程区分，而且
/// “读出来、改、写回去”必须是一步，否则后写的会盖掉先写的改动。
pub(crate) fn replace_file(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;

    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let written = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        if let Ok(metadata) = std::fs::metadata(path) {
            std::fs::set_permissions(&tmp, metadata.permissions())?;
        }
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        // 没换成：不留下临时文件（里面是完整的配置）。
        let _ = std::fs::remove_file(&tmp);
    }
    written
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
    /// 加载时进程环境里的 `SBOT__*` 变量（见 [`env_overrides`]）。
    ///
    /// `/config` 写入重新解析文件时套用的是这一份，而不是再读一次进程环境：两处得到的是同一份
    /// 配置，解析也不依赖进程级的可变状态。
    pub overrides: Vec<(String, String)>,
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

        let overrides = env_overrides();
        let config = Self::parse(&text, &overrides).map_err(|message| ConfigLoadError::Parse {
            path: path.clone(),
            message,
        })?;

        config.validate_shape().map_err(ConfigLoadError::Invalid)?;

        let dir = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        Ok(LoadedConfig {
            config,
            path,
            dir,
            overrides,
        })
    }

    /// 从配置文件的文本得到配置：解析、套上环境变量覆盖、反序列化。不做校验。
    ///
    /// 启动加载与 `/config` 写入共用这一步，两处得到的是同一份配置。各做各的话，agent 改
    /// 一次配置，环境变量设的值就悄悄退回文件里的值，重启之后又变回来。
    ///
    /// `overrides` 是 `SBOT__*` 变量：启动时来自进程环境，之后用 [`LoadedConfig::overrides`] 里
    /// 存下的那一份。
    pub fn parse(text: &str, overrides: &[(String, String)]) -> Result<Self, String> {
        let mut table: toml::Table = toml::from_str(text).map_err(|error| error.to_string())?;
        apply_overrides(&mut table, overrides.iter().cloned());
        toml::Value::Table(table)
            .try_into()
            .map_err(|error: toml::de::Error| error.to_string())
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

/// 进程环境里的 `SBOT__*` 变量。
///
/// 名字不是合法 Unicode 的变量不可能是我们的，跳过；值按有损转换取——`std::env::vars` 遇到
/// 任何一个非 Unicode 的变量（哪怕与 sbot 无关）都会 panic，这里不用它。
pub fn env_overrides() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.into_string().ok()?;
            key.starts_with("SBOT__")
                .then(|| (key, value.to_string_lossy().into_owned()))
        })
        .collect()
}

/// 把 `SBOT__A__B=value` 形式的变量注入 TOML 表；别的名字忽略。
///
/// 这是可选能力：普通部署直接把值写进 `sbot.toml`。
fn apply_overrides(table: &mut toml::Table, vars: impl Iterator<Item = (String, String)>) {
    let mut overrides: Vec<(Vec<String>, String)> = vars
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

    if overrides.is_empty() {
        return;
    }
    // 环境变量的值都是字符串，要变成哪种 TOML 值看这一项本来是什么类型：默认配置里它是
    // 字符串就原样用，否则按字面量解析。
    let defaults = toml::Value::try_from(SbotConfig::default()).ok();
    for (parts, raw) in overrides {
        let default = defaults.as_ref().and_then(|defaults| {
            parts
                .iter()
                .try_fold(defaults, |value, part| value.get(part))
        });
        insert_path(table, &parts, override_value(&raw, default));
    }
}

/// 一个环境变量的值对应的 TOML 值；`default` 是这一项在默认配置里的值（没有就是 `None`）。
///
/// - 这一项是字符串：原样当字符串。`SBOT__AGENT__MASTER=123456` 是 QQ 号写成的字符串，
///   不能因为长得像数字就变成整数。
/// - 其余：按 TOML 字面量解析（`32`、`true`、`[1, 2]`、`{ a = 1 }`）。解析不了的当作
///   字符串，类型不对由后面的反序列化报出来，报错里带着键名。
fn override_value(raw: &str, default: Option<&toml::Value>) -> toml::Value {
    if matches!(default, Some(toml::Value::String(_))) {
        return toml::Value::String(raw.to_owned());
    }
    parse_literal(raw).unwrap_or_else(|| toml::Value::String(raw.to_owned()))
}

/// 把 `raw` 当作一个 TOML 值来解析；不是单个值时返回 `None`。
///
/// TOML 只能解析整份文档，所以套成 `value = <raw>` 再取出来。解析出来必须恰好是这一个键：
/// 值里带换行再跟一行 `other = 1` 的写法不是字面量。
fn parse_literal(raw: &str) -> Option<toml::Value> {
    let mut document: toml::Table = toml::from_str(&format!("value = {raw}")).ok()?;
    if document.len() != 1 {
        return None;
    }
    document.remove("value")
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

    /// 目录里的文件名，排好序。
    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// 替换内容后目录里只有配置文件本身；换不成（目标是个目录）时报错，也不留下临时文件。
    #[test]
    fn replace_file_swaps_the_content_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sbot.toml");
        std::fs::write(&path, "version = 1\n").unwrap();
        replace_file(&path, "version = 1\n[agent]\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "version = 1\n[agent]\n"
        );
        assert_eq!(file_names(dir.path()), vec!["sbot.toml"]);

        let blocked = dir.path().join("blocked.toml");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("keep"), "x").unwrap();
        assert!(replace_file(&blocked, "version = 1\n").is_err());
        assert_eq!(file_names(dir.path()), vec!["blocked.toml", "sbot.toml"]);
    }

    /// 收紧过权限的配置文件（MCP 的 `env` 里有 token）写回后权限不变。
    #[cfg(unix)]
    #[test]
    fn replace_file_keeps_the_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sbot.toml");
        std::fs::write(&path, "version = 1\n").unwrap();
        for mode in [0o600, 0o640] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            replace_file(&path, "version = 1\n").unwrap();
            let actual = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(actual, mode);
        }
    }

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
                .any(|e| e.field == "monitor[0].source.timer" && e.message.contains("60s"))
        );
    }

    #[test]
    fn timer_jitter_is_validated_with_the_schedule() {
        let text = r#"
version = 1
[[monitor]]
name = "beat"
source = { timer = { every = "2m", jitter_secs = 120 } }
deliver = { to = "main", as = "event" }
"#;
        let table: toml::Table = toml::from_str(text).unwrap();
        let config: SbotConfig = toml::Value::Table(table).try_into().unwrap();
        let errors = config.validate_shape().unwrap_err();
        assert!(
            errors
                .0
                .iter()
                .any(|e| e.field == "monitor[0].source.timer" && e.message.contains("jitter_secs"))
        );
        let fine = text.replace("jitter_secs = 120", "jitter_secs = 30");
        let table: toml::Table = toml::from_str(&fine).unwrap();
        let config: SbotConfig = toml::Value::Table(table).try_into().unwrap();
        config.validate_shape().unwrap();
    }

    #[test]
    fn stream_url_scheme_must_match_its_kind() {
        let text = r#"
version = 1
[[monitor]]
name = "ci"
source = { stream = { url = "https://ci.example.com/events", kind = "websocket" } }
deliver = { to = "main", as = "event" }
"#;
        let table: toml::Table = toml::from_str(text).unwrap();
        let config: SbotConfig = toml::Value::Table(table).try_into().unwrap();
        let errors = config.validate_shape().unwrap_err();
        assert!(
            errors
                .0
                .iter()
                .any(|e| e.field == "monitor[0].source.stream.url")
        );
        let sse = text.replace("\"websocket\"", "\"sse\"");
        let table: toml::Table = toml::from_str(&sse).unwrap();
        let config: SbotConfig = toml::Value::Table(table).try_into().unwrap();
        config.validate_shape().unwrap();
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

    /// 把一组 `SBOT__…` 变量套到 `version = 1` 上，得到配置。
    fn with_overrides(vars: &[(&str, &str)]) -> Result<SbotConfig, String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        SbotConfig::parse("version = 1", &vars)
    }

    /// 环境变量的值都是字符串；每一项要按它在配置里的类型变成对应的 TOML 值。
    #[test]
    fn env_overrides_take_the_type_of_the_setting() {
        let config = with_overrides(&[
            ("SBOT__AGENT__THINKING_LEVEL", "low"),
            ("SBOT__AGENT__MAX_STEPS", "32"),
            ("SBOT__FILTER__REQUIRE_MENTION", "true"),
            ("SBOT__FILTER__IGNORE_GROUPS", "[1, 2]"),
            ("SBOT__NAPCAT__URL", "ws://127.0.0.1:3002"),
            // 长得像数字、布尔的字符串项仍然是字符串。
            ("SBOT__AGENT__MASTER", "123456"),
            ("SBOT__AGENT__OWNER_NAME", "true"),
            // 别的环境变量不相干。
            ("PATH", "/usr/bin"),
            ("SBOT_HOME", "/srv/sbot"),
        ])
        .unwrap();
        assert_eq!(config.agent.thinking_level, "low");
        assert_eq!(config.agent.max_steps, 32);
        assert!(config.filter.require_mention);
        assert_eq!(config.filter.ignore_groups, [1, 2]);
        assert_eq!(config.napcat.url, "ws://127.0.0.1:3002");
        assert_eq!(config.agent.master, "123456");
        assert_eq!(config.agent.owner_name, "true");
    }

    /// 类型不对、键不存在都是加载错误，报错里指得出是哪一项；不会被悄悄忽略。
    #[test]
    fn a_bad_env_override_is_a_load_error() {
        let error = with_overrides(&[("SBOT__AGENT__MAX_STEPS", "many")])
            .unwrap_err()
            .to_string();
        assert!(error.contains("max_steps"), "{error}");
        let error = with_overrides(&[("SBOT__AGENT__NO_SUCH_KEY", "1")])
            .unwrap_err()
            .to_string();
        assert!(error.contains("no_such_key"), "{error}");
    }

    /// 值里夹带换行与别的键不算一个字面量：整个值当作字符串，不会多出一项配置。
    #[test]
    fn an_override_value_cannot_smuggle_another_key() {
        assert_eq!(parse_literal("32"), Some(toml::Value::Integer(32)));
        assert_eq!(parse_literal("1\nother = 2"), None);
        assert_eq!(parse_literal("not a literal"), None);
        assert_eq!(
            override_value("1\nother = 2", None),
            toml::Value::String("1\nother = 2".to_owned())
        );
    }

    /// 文件里写了的项也被覆盖；没有覆盖时就是文件里的值。
    #[test]
    fn overrides_win_over_the_file() {
        let text = "version = 1\n[agent]\nowner_name = \"from-file\"\n";
        let vars = [("SBOT__AGENT__OWNER_NAME".to_owned(), "from-env".to_owned())];
        assert_eq!(
            SbotConfig::parse(text, &vars).unwrap().agent.owner_name,
            "from-env"
        );
        assert_eq!(
            SbotConfig::parse(text, &[]).unwrap().agent.owner_name,
            "from-file"
        );
    }

    /// 只取 `SBOT__` 开头的变量；`SBOT_HOME` 这样的不算。
    #[test]
    fn env_overrides_only_collects_prefixed_names() {
        assert!(
            env_overrides()
                .iter()
                .all(|(key, _)| key.starts_with("SBOT__"))
        );
    }

    #[test]
    fn retired_context_hooks_fail_with_a_migration_hint() {
        let config: SbotConfig =
            toml::from_str("[[hooks.context]]\nname = \"soul\"\npath = \"/SOUL.md\"\n").unwrap();
        let errors = config.validate_shape().unwrap_err();
        assert!(
            errors
                .0
                .iter()
                .any(|error| error.field == "hooks.context" && error.message.contains("removed"))
        );
    }

    #[test]
    fn retired_skills_shared_dir_fails_with_a_migration_hint() {
        let config: SbotConfig =
            toml::from_str("[skills]\nshared_dir = \"~/.sbot/skills\"\n").unwrap();
        let errors = config.validate_shape().unwrap_err();
        assert!(
            errors.0.iter().any(
                |error| error.field == "skills.shared_dir" && error.message.contains("removed")
            )
        );
        // 序列化（/config 视图、persist）不会把它写回去。
        assert!(!toml::to_string(&config).unwrap().contains("shared_dir"));
    }

    #[test]
    fn retired_hook_keys_and_bad_hook_fields_are_reported() {
        let text = r#"
version = 1
[[hooks.events]]
event = "before_send"
action = "log"
scope = "all"
match = { tool_name = "API" }

[[hooks.events]]
event = "session_end"
action = "notify"
notify_channel = "email"

[[hooks.events]]
event = "session_start"
action = "inject_context"
path = "relative.md"
"#;
        let table: toml::Table = toml::from_str(text).unwrap();
        let config: SbotConfig = toml::Value::Table(table).try_into().unwrap();
        let errors = config.validate_shape().unwrap_err();
        for field in [
            "hooks.events[0].scope",
            "hooks.events[0].match.tool_name",
            "hooks.events[1].notify_channel",
            "hooks.events[2].path",
        ] {
            assert!(
                errors.0.iter().any(|e| e.field == field),
                "{field}: {errors:?}"
            );
        }
        // 序列化不会把退役的键写回去（`receive_scopes` 之类的同名子串不算）。
        let written = toml::to_string(&config).unwrap();
        assert!(
            !written.contains("\nscope =") && !written.contains("tool_name"),
            "{written}"
        );
    }

    #[test]
    fn retired_memory_topics_fails_with_a_migration_hint() {
        let config: SbotConfig = toml::from_str("[memory]\ntopics = true\n").unwrap();
        let errors = config.validate_shape().unwrap_err();
        assert!(
            errors
                .0
                .iter()
                .any(|error| error.field == "memory.topics" && error.message.contains("removed"))
        );
        assert!(!toml::to_string(&config).unwrap().contains("topics"));
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
