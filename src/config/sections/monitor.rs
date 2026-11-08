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
    /// 一次性本地时间（`YYYY-MM-DD HH:MM:SS`）。
    pub at: Option<String>,
    /// 每次触发随机推迟的上限（秒）；不设或 0 = 准点。最多 3600，`every` 时必须短于周期。
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
pub(in crate::config) fn validate_monitors(
    monitors: &[MonitorConfig],
    errors: &mut Vec<ConfigError>,
) {
    let mut names = HashSet::new();
    for (index, monitor) in monitors.iter().enumerate() {
        let field = format!("monitor[{index}]");

        non_empty(errors, format!("{field}.name"), &monitor.name);
        if !monitor.name.trim().is_empty() && !names.insert(monitor.name.as_str()) {
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

        // 与运行时同一个解析器：`at` 的时分范围、cron 的语法与“永远不会匹配”都在这里报错。
        if let Some(timer) = &monitor.source.timer
            && let Err(error) = crate::monitor::Timer::parse(
                timer.cron.as_deref(),
                timer.every.as_deref(),
                timer.at.as_deref(),
                timer.jitter_secs,
            )
        {
            errors.push(ConfigError::new(format!("{field}.source.timer"), error));
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

        non_empty(errors, format!("{field}.deliver.to"), &monitor.deliver.to);
        if monitor.deliver.to != "main"
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

        non_zero(
            errors,
            format!("{field}.deliver.batch_window_ms"),
            monitor.deliver.batch_window_ms,
        );

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

        non_zero(
            errors,
            format!("{field}.lifecycle.max_events"),
            monitor.lifecycle.max_events,
        );
    }
}
