//! monitor 定义：配置与工具参数到运行时规格的唯一转换。
//!
//! - 配置文件 `[[monitor]]`：[`load_config_monitors`]，来源 [`MonitorOrigin::Config`]；
//! - `Monitor add`：[`runtime_config`] 把工具参数按同一个 [`MonitorConfig`] 严格解析（未知键
//!   报错），套上运行时的保守默认值，再过与配置文件相同的校验，来源 `Runtime`；
//! - 内置唤醒预设（`app::assembly`）：来源 `Preset`，删除后会被记住。
//!
//! 两条路径最后都经 [`MonitorSpec::from_config`] 转成 [`MonitorSpec`]。

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use super::{InternalKind, MonitorEvent, MonitorManager, Timer, matches};
use crate::config::{
    BacklogMode, CatchUp, DeliverAs, DeliverConfig, LifecycleConfig, MonitorConfig, MonitorSource,
    OnMissing, QqSource,
};
use crate::qq::event::QqEvent;
use crate::qq::format;

/// 运行时 monitor 默认的投递次数上限。
const RUNTIME_MAX_EVENTS: u64 = 50;
/// 运行时 monitor 默认的有效期。
const RUNTIME_EXPIRES: &str = "2h";

/// monitor 从哪里来；决定 `list` 的显示，以及删除后重启时是否还会出现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorOrigin {
    /// 配置文件 `[[monitor]]`：每次启动按配置重建。
    Config,
    /// `Monitor add`：只在内存里，重启即消失。
    Runtime,
    /// 内置唤醒预设：删除后写进 `meta`，重启不再安装。
    Preset,
}

impl MonitorOrigin {
    /// `Monitor list` 里显示的名字。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Runtime => "runtime",
            Self::Preset => "preset",
        }
    }
}

/// 一个 monitor 的完整定义；`MonitorManager::add` 的唯一参数。
///
/// 配置文件与 `Monitor add` 都经 [`MonitorSpec::from_config`] 构造；唤醒预设用具名字面量
/// （它需要配置里不允许的 `batch_window = 0` 与很大的 `max_events`）。
pub struct MonitorSpec {
    /// 唯一名字；重复时 `add` 报错。
    pub name: String,
    /// `qq` / `timer` / `internal:<kind>` / `stream`。
    pub source: String,
    /// 定时源；`None` 表示事件驱动。
    pub timer: Option<Timer>,
    /// 谓词表达式；`None` 匹配该 source 的全部事件。
    pub when: Option<String>,
    /// `deliver_as = Prompt` 时的模板，支持 `{{event.text}}` / `{{event.chat}}` / `{{count}}`。
    pub prompt: Option<String>,
    /// 投递形式：原始事件、指令，或把 QQ 消息交给 ChatQueue（`chat`）。
    pub deliver_as: DeliverAs,
    /// 投递次数上限；达到后停用并通知。
    pub max_events: u64,
    /// 有效时长；`None` 表示不过期。
    pub expires: Option<Duration>,
    /// 投递一次后停用（配合定时源）。
    pub one_shot: bool,
    /// 两次投递的最小间隔。
    pub min_interval: Duration,
    /// 只在 agent 空闲时投递。
    pub idle_only: bool,
    /// 批处理窗口；0 表示逐个投递。
    pub batch_window: Duration,
    /// 投递目标；`None` 表示主代理。
    pub target: Option<String>,
    /// 定时源错过触发时的处理方式。
    pub catch_up: CatchUp,
    /// 是否把投递抄送主代理。
    pub cc: bool,
    /// 目标不存在时：丢弃 / 排队 / 新建子代理。
    pub on_missing: OnMissing,
    /// 从哪里来：配置文件、`Monitor add` 或内置预设。
    pub origin: MonitorOrigin,
}

impl MonitorSpec {
    /// 把一份（已校验的）`MonitorConfig` 转成运行时规格。
    ///
    /// 校验在调用之前做（`validate_monitors` / [`runtime_config`]）；这里仍然对定时器、内部
    /// 种类与有效期报错而不是忽略：绕过校验的输入不能变成一个永远不触发的 monitor。
    pub fn from_config(config: &MonitorConfig, origin: MonitorOrigin) -> Result<Self> {
        let name = &config.name;
        let (source, timer) = match config.source.kind() {
            Some("qq") => ("qq".to_owned(), None),
            Some("timer") => {
                let timer = config
                    .source
                    .timer
                    .as_ref()
                    .map(Timer::from_source)
                    .transpose()
                    .map_err(|error| anyhow::anyhow!("monitor {name:?}: {error}"))?;
                ("timer".to_owned(), timer)
            }
            Some("internal") => {
                let raw = config
                    .source
                    .internal
                    .as_ref()
                    .map(|internal| internal.kind.as_str())
                    .unwrap_or_default();
                let kind = InternalKind::parse(raw).ok_or_else(|| {
                    anyhow::anyhow!("monitor {name:?}: {}", InternalKind::expected())
                })?;
                (kind.source(), None)
            }
            Some("stream") => ("stream".to_owned(), None),
            _ => anyhow::bail!(
                "monitor {name:?}: source must set exactly one of qq, timer, stream or internal"
            ),
        };
        // `0`（任意单位）都表示不过期；写错的时长报错。
        let expires = match crate::config::parse_duration_secs(&config.lifecycle.expires) {
            Some(0) => None,
            Some(seconds) => Some(Duration::from_secs(seconds)),
            None => anyhow::bail!(
                "monitor {name:?}: invalid expires {:?}",
                config.lifecycle.expires
            ),
        };
        let deliver_as = config.deliver.as_;
        Ok(Self {
            name: name.clone(),
            source,
            timer,
            when: config.when.clone(),
            prompt: (deliver_as == DeliverAs::Prompt).then(|| config.deliver.prompt.clone()),
            deliver_as,
            max_events: config.lifecycle.max_events,
            expires,
            one_shot: config.lifecycle.one_shot,
            min_interval: Duration::from_secs(config.lifecycle.min_interval_secs),
            idle_only: config.lifecycle.idle_only,
            batch_window: Duration::from_millis(config.deliver.batch_window_ms),
            target: (config.deliver.to != "main").then(|| config.deliver.to.clone()),
            catch_up: config.lifecycle.catch_up,
            cc: config.deliver.cc,
            on_missing: config.on_missing,
            origin,
        })
    }
}

/// `Monitor add` 的参数（不含 `op` / `name`）：字段与 `[[monitor]]` 相同。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeArgs {
    source: MonitorSource,
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    on_missing: OnMissing,
    #[serde(default)]
    deliver: DeliverConfig,
    #[serde(default)]
    lifecycle: RuntimeLifecycle,
}

/// 运行时 monitor 的生命周期。默认比配置文件保守（只在空闲时投递、50 次、2 小时）；
/// 没有 `catch_up`：运行时 monitor 重启即消失，没有错过的触发可补。
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RuntimeLifecycle {
    idle_only: bool,
    min_interval_secs: u64,
    max_events: u64,
    expires: String,
    one_shot: bool,
}

impl Default for RuntimeLifecycle {
    fn default() -> Self {
        Self {
            idle_only: true,
            min_interval_secs: 0,
            max_events: RUNTIME_MAX_EVENTS,
            expires: RUNTIME_EXPIRES.to_owned(),
            one_shot: false,
        }
    }
}

/// 把 `Monitor add` 的参数解析成 `MonitorConfig`，并过与配置文件相同的校验。
///
/// 只能写在配置文件里的：`stream` 源（外部网络）与 `qq.backlog`（创建时补投 inbox）。
pub fn runtime_config(name: &str, args: serde_json::Value) -> Result<MonitorConfig, String> {
    let args: RuntimeArgs =
        serde_json::from_value(args).map_err(|error| format!("invalid monitor: {error}"))?;
    if args.source.stream.is_some() {
        return Err(
            "A stream source is config-file only; `Monitor add` refuses external network sources."
                .to_owned(),
        );
    }
    if matches!(args.source.qq, Some(QqSource::Detailed(_))) {
        return Err(
            "qq.backlog is config-file only; use {\"qq\": true} and read /inbox for the backlog."
                .to_owned(),
        );
    }
    let lifecycle = args.lifecycle;
    let config = MonitorConfig {
        name: name.to_owned(),
        enabled: true,
        source: args.source,
        when: args.when,
        on_missing: args.on_missing,
        deliver: args.deliver,
        lifecycle: LifecycleConfig {
            idle_only: lifecycle.idle_only,
            min_interval_secs: lifecycle.min_interval_secs,
            max_events: lifecycle.max_events,
            expires: lifecycle.expires,
            one_shot: lifecycle.one_shot,
            catch_up: CatchUp::Coalesce,
        },
    };
    crate::config::check_monitor(&config)?;
    Ok(config)
}

/// 从配置构造长期常驻 monitor。
pub async fn load_config_monitors(
    manager: &Arc<MonitorManager>,
    monitors: &[crate::config::MonitorConfig],
    // 用于 `backlog = "inbox"` 的首次补投；`None` 时跳过。
    messages: Option<&Arc<crate::storage::messages::MessageStore>>,
) -> Result<()> {
    for monitor in monitors.iter().filter(|monitor| monitor.enabled) {
        manager
            .add(MonitorSpec::from_config(monitor, MonitorOrigin::Config)?)
            .await?;
        // 外部流：添加之后再连接，连接任务拿到的是已注册的 monitor。
        if let Some(stream) = &monitor.source.stream {
            manager
                .attach_stream(&monitor.name, stream.url.clone(), stream.kind)
                .await;
        }

        // `backlog = "inbox"`：创建时把匹配的未读消息补投一次；
        // 只有真正投递出去的条目才标记已读。
        let wants_backlog = matches!(
            monitor.source.qq,
            Some(QqSource::Detailed(ref config)) if config.backlog == BacklogMode::Inbox
        );
        if wants_backlog && let Some(messages) = messages {
            let mut lines = Vec::new();
            for message in messages.unread(None).unwrap_or_default() {
                // 与实时事件同一套视图（`@` 判定、主人消息），补投与实时投递结果一致。
                let Some(mut event) = MonitorEvent::from_qq(
                    &QqEvent::Message(message.clone()),
                    manager.self_id.load(Ordering::Relaxed),
                ) else {
                    continue;
                };
                let owner_qq = manager.owner_qq.load(Ordering::Relaxed);
                event.owner_intent = owner_qq != 0 && event.sender == Some(owner_qq);
                if !matches(monitor.when.as_deref(), &event).unwrap_or(false) {
                    continue;
                }
                lines.push(format::render_inbound(&message));
                let _ = messages.mark_read(message.chat, &[message.message_id]);
            }
            if !lines.is_empty() {
                manager
                    .deliver_text(&monitor.name, lines.join("\n\n"))
                    .await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_args_use_the_config_schema_with_bounded_defaults() {
        let config = runtime_config(
            "t",
            serde_json::json!({"source": {"timer": {"every": "10m"}}}),
        )
        .unwrap();
        assert!(config.lifecycle.idle_only);
        assert_eq!(config.lifecycle.max_events, RUNTIME_MAX_EVENTS);
        let spec = MonitorSpec::from_config(&config, MonitorOrigin::Runtime).unwrap();
        assert_eq!(spec.expires, Some(Duration::from_secs(2 * 3600)));
        assert!(spec.timer.is_some());
        assert_eq!(spec.origin, MonitorOrigin::Runtime);

        // 与配置文件同一种写法。
        let internal = runtime_config(
            "i",
            serde_json::json!({"source": {"internal": {"kind": "security"}}}),
        )
        .unwrap();
        let spec = MonitorSpec::from_config(&internal, MonitorOrigin::Runtime).unwrap();
        assert_eq!(spec.source, "internal:security");

        // `expires = "0"` 不过期。
        let forever = runtime_config(
            "f",
            serde_json::json!({"source": {"qq": true}, "lifecycle": {"expires": "0"}}),
        )
        .unwrap();
        let spec = MonitorSpec::from_config(&forever, MonitorOrigin::Runtime).unwrap();
        assert_eq!(spec.expires, None);
    }

    #[test]
    fn runtime_args_reject_what_they_cannot_honour() {
        for (args, needle) in [
            // 运行时 monitor 重启即消失，没有补投可言。
            (
                serde_json::json!({"source": {"timer": {"every": "10m"}}, "lifecycle": {"catch_up": "skip"}}),
                "catch_up",
            ),
            (
                serde_json::json!({"source": {"stream": {"url": "wss://x"}}}),
                "config-file",
            ),
            (
                serde_json::json!({"source": {"qq": {"backlog": "inbox"}}}),
                "config-file",
            ),
            (
                serde_json::json!({"source": {"qq": true}, "deliver": {"to": "main", "bogus": 1}}),
                "bogus",
            ),
            (serde_json::json!({"source": {"qq": false}}), "qq = false"),
            (serde_json::json!({"source": {}}), "exactly one"),
            (
                serde_json::json!({"source": {"qq": true}, "when": "event.bogus"}),
                "unsupported",
            ),
            (
                serde_json::json!({"source": {"timer": {"every": "10m"}}, "deliver": {"as": "chat"}}),
                "requires a qq source",
            ),
            (
                serde_json::json!({"source": {"qq": true}, "deliver": {"as": "prompt"}}),
                "deliver.prompt",
            ),
            (
                serde_json::json!({"source": {"qq": true}, "lifecycle": {"expires": "soon"}}),
                "expires",
            ),
        ] {
            let error = runtime_config("x", args.clone()).unwrap_err();
            assert!(error.contains(needle), "{args}: {error}");
        }
    }
}
