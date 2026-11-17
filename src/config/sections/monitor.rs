use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::{ConfigError, non_empty, non_zero};
use crate::config::parse_duration_secs;

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
    /// 每天的本地时刻（`HH:MM`）；一次性提醒配合 `lifecycle.one_shot = true`。
    pub at: Option<String>,
    /// 每次触发随机推迟的上限（秒）；不设或 0 = 准点。最多 3600，`every` 时必须短于周期。
    pub jitter_secs: Option<u64>,
}

/// 外部流源（只能由主人写进配置文件），实现见 `monitor/stream.rs`。
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
    /// 原样投递事件（`[monitor 名字 | source | chat]` 信封），跑一个 monitor 回合。
    #[default]
    Event,
    /// 投递渲染后的指令（`prompt` 字段），跑一个 monitor 回合。
    Prompt,
    /// 把原始 QQ 消息交给 ChatQueue，按普通会话回合处理（去抖、并发、`[queue].mode` 都
    /// 生效）；只用于 `qq` 源，`batch_window_ms` 不生效。内置唤醒预设就是这种形式。
    Chat,
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
pub(in crate::config) fn validate_monitors(
    monitors: &[MonitorConfig],
    errors: &mut Vec<ConfigError>,
) {
    let mut names = HashSet::new();
    for (index, monitor) in monitors.iter().enumerate() {
        let field = format!("monitor[{index}]");
        if !monitor.name.trim().is_empty() && !names.insert(monitor.name.as_str()) {
            errors.push(ConfigError::new(
                format!("{field}.name"),
                format!("duplicate monitor name {:?}", monitor.name),
            ));
        }
        validate_monitor(&field, monitor, errors);
    }
}

/// 校验运行时创建的单个 monitor（`Monitor add`）：与配置文件同一套规则，错误合成一段文字。
pub(crate) fn check_monitor(monitor: &MonitorConfig) -> Result<(), String> {
    let mut errors = Vec::new();
    validate_monitor("monitor", monitor, &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "))
    }
}

/// 校验一个 monitor；`field` 是错误信息里的位置前缀。
fn validate_monitor(field: &str, monitor: &MonitorConfig, errors: &mut Vec<ConfigError>) {
    non_empty(errors, format!("{field}.name"), &monitor.name);

    let Some(kind) = monitor.source.kind() else {
        errors.push(ConfigError::new(
            format!("{field}.source"),
            "must set exactly one of \"qq\", \"timer\", \"stream\" or \"internal\"",
        ));
        return;
    };
    validate_source(field, &monitor.source, errors);

    if let Some(when) = &monitor.when {
        if when.trim().is_empty() {
            errors.push(ConfigError::new(
                format!("{field}.when"),
                "must not be empty; omit the field to match everything",
            ));
        } else if let Err(error) = crate::monitor::validate_when(when) {
            errors.push(ConfigError::new(format!("{field}.when"), error));
        }
    }

    validate_delivery(field, &monitor.deliver, kind, errors);
    validate_lifecycle(field, &monitor.lifecycle, kind, errors);
}

/// `source` 里各种源自己的字段；“恰好设置一种源”由调用方先检查。
fn validate_source(field: &str, source: &MonitorSource, errors: &mut Vec<ConfigError>) {
    // `qq = false` 以前会被当成“已设置 qq 源”而照常订阅；关掉 monitor 用 `enabled = false`。
    if matches!(source.qq, Some(QqSource::Enabled(false))) {
        errors.push(ConfigError::new(
            format!("{field}.source.qq"),
            "qq = false is not a source; use enabled = false to turn the monitor off",
        ));
    }

    // 与运行时同一个解析器：`at` 的时分范围、cron 的语法与“永远不会匹配”都在这里报错。
    if let Some(timer) = &source.timer
        && let Err(error) = crate::monitor::Timer::from_source(timer)
    {
        errors.push(ConfigError::new(format!("{field}.source.timer"), error));
    }

    // WebSocket 只接受 ws(s)://，SSE 只接受 http(s)://：写反了连接会一直失败。
    if let Some(stream) = &source.stream {
        let (schemes, expected) = match stream.kind {
            StreamKind::Websocket => (["ws://", "wss://"], "a ws:// or wss:// URL"),
            StreamKind::Sse => (["http://", "https://"], "an http:// or https:// URL"),
        };
        if !schemes.iter().any(|scheme| stream.url.starts_with(scheme)) {
            errors.push(ConfigError::new(
                format!("{field}.source.stream.url"),
                format!("must be {expected} for kind = {:?}", stream.kind),
            ));
        }
    }

    if let Some(internal) = &source.internal
        && crate::monitor::InternalKind::parse(&internal.kind).is_none()
    {
        errors.push(ConfigError::new(
            format!("{field}.source.internal.kind"),
            "must be one of \"agent\", \"security\", \"budget\" or \"storage\"",
        ));
    }
}

/// `deliver`：投给谁、以什么形式。`kind` 是源的种类（`MonitorSource::kind`）。
fn validate_delivery(
    field: &str,
    deliver: &DeliverConfig,
    kind: &str,
    errors: &mut Vec<ConfigError>,
) {
    non_empty(errors, format!("{field}.deliver.to"), &deliver.to);
    if deliver.to != "main"
        && let Some(name) = deliver.to.strip_prefix("agent:")
        && name.is_empty()
    {
        errors.push(ConfigError::new(
            format!("{field}.deliver.to"),
            "\"agent:\" requires a name",
        ));
    }

    if deliver.as_ == DeliverAs::Prompt && deliver.prompt.trim().is_empty() {
        errors.push(ConfigError::new(
            format!("{field}.deliver.prompt"),
            "is required when deliver.as = \"prompt\"",
        ));
    }

    non_zero(
        errors,
        format!("{field}.deliver.batch_window_ms"),
        deliver.batch_window_ms,
    );

    // `as = "chat"` 把原始 QQ 消息交给主代理的 ChatQueue：其他源没有消息可交，
    // 投给子代理时也没有会话队列可用。
    if deliver.as_ == DeliverAs::Chat {
        if kind != "qq" {
            errors.push(ConfigError::new(
                format!("{field}.deliver.as"),
                "\"chat\" requires a qq source",
            ));
        }
        if deliver.to != "main" {
            errors.push(ConfigError::new(
                format!("{field}.deliver.as"),
                "\"chat\" requires deliver.to = \"main\"",
            ));
        }
    }
}

/// `lifecycle`：过期时间、一次性与事件上限。`kind` 是源的种类。
fn validate_lifecycle(
    field: &str,
    lifecycle: &LifecycleConfig,
    kind: &str,
    errors: &mut Vec<ConfigError>,
) {
    if parse_duration_secs(&lifecycle.expires).is_none() {
        errors.push(ConfigError::new(
            format!("{field}.lifecycle.expires"),
            format!("invalid duration {:?}", lifecycle.expires),
        ));
    }

    if lifecycle.one_shot && kind != "timer" {
        errors.push(ConfigError::new(
            format!("{field}.lifecycle.one_shot"),
            "one_shot requires a timer source",
        ));
    }

    non_zero(
        errors,
        format!("{field}.lifecycle.max_events"),
        lifecycle.max_events,
    );
}
