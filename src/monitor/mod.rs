//! 监视器：事件流到消息的订阅。
//!
//! 设计依据：设计 §5.11。
//!
//! 结构：`{source, when?, deliver, lifecycle?}`。
//! - source：`qq` / `timer` / `internal`；`stream` 只允许写在配置文件里；
//! - when：谓词子集（`event.mentions_self`、`event.type == 'message'`、
//!   `event.chat == 'group:123'`、`event.text contains 'x'`，用 `and` 组合）；
//! - deliver：投递给主代理，以 `event` 或 `prompt` 形式；
//! - lifecycle：`max_events`、`expires`、`one_shot`、`min_interval_secs`。
//!
//! 运行时创建的 monitor 只在内存里，重启即消失；配置文件里的才是长期常驻。
//! 静默不等于正常：谓词求值失败、到期、达到上限都会产生一条系统消息。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::qq::event::QqEvent;

/// 投递内容。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorDelivery {
    pub name: String,
    pub source: String,
    /// `true` 表示作为指令投递（`as = "prompt"`）。
    pub as_prompt: bool,
    pub text: String,
    /// 系统通知（到期、上限、谓词失败）——不来自任何用户。
    pub system: bool,
}

/// 投递端；由 runtime 提供。
#[async_trait::async_trait]
pub trait MonitorSink: Send + Sync {
    async fn deliver(&self, delivery: MonitorDelivery);
}

/// 事件视图。
#[derive(Debug, Clone, Default)]
pub struct MonitorEvent {
    pub kind: String,
    pub chat: String,
    pub sender: Option<u64>,
    pub text: String,
    pub mentions_self: bool,
}

impl MonitorEvent {
    /// 从 QQ 事件构造；非消息/通知/请求返回 `None`。
    pub fn from_qq(event: &QqEvent) -> Option<Self> {
        match event {
            QqEvent::Message(message) => Some(Self {
                kind: "message".to_owned(),
                chat: message.chat.to_string(),
                sender: message.sender.qq,
                text: message.plain_text(),
                mentions_self: message
                    .segments
                    .iter()
                    .any(|segment| matches!(segment, crate::qq::event::Segment::At { .. })),
            }),
            QqEvent::Notice(notice) => Some(Self {
                kind: format!("notice:{}", notice.kind),
                chat: notice.chat.map(|chat| chat.to_string()).unwrap_or_default(),
                sender: notice.actor,
                text: String::new(),
                mentions_self: false,
            }),
            QqEvent::Request(request) => Some(Self {
                kind: format!("request:{}", request.kind),
                chat: request
                    .group
                    .map(|group| format!("group:{group}"))
                    .unwrap_or_default(),
                sender: request.actor,
                text: request.comment.clone(),
                mentions_self: false,
            }),
            _ => None,
        }
    }
}

/// 谓词求值；错误由调用方转成系统通知，绝不静默失败。
pub fn matches(when: Option<&str>, event: &MonitorEvent) -> Result<bool, String> {
    let Some(when) = when else {
        return Ok(true);
    };
    for term in when.split(" and ") {
        let term = term.trim();
        if term.is_empty() {
            continue;
        }
        if !match_term(term, event)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn match_term(term: &str, event: &MonitorEvent) -> Result<bool, String> {
    if term == "event.mentions_self" {
        return Ok(event.mentions_self);
    }
    if let Some(value) = term.strip_prefix("event.type == ") {
        return Ok(event.kind == unquote(value));
    }
    if let Some(value) = term.strip_prefix("event.chat == ") {
        return Ok(event.chat == unquote(value));
    }
    if let Some(value) = term.strip_prefix("event.sender == ") {
        return Ok(event.sender.map(|sender| sender.to_string()).as_deref() == Some(unquote(value)));
    }
    if let Some(rest) = term.strip_prefix("event.text contains ") {
        return Ok(event.text.contains(unquote(rest)));
    }
    Err(format!("unsupported predicate term {term:?}"))
}

fn unquote(value: &str) -> &str {
    value.trim().trim_matches(|c| c == '\'' || c == '"')
}

/// 定时源。
#[derive(Debug, Clone)]
pub enum TimerSpec {
    Every(Duration),
    At { hour: u32, minute: u32 },
    Cron(String),
}

/// 运行时 monitor。
struct Monitor {
    name: String,
    source: String,
    when: Option<String>,
    prompt: Option<String>,
    as_prompt: bool,
    min_interval: Duration,
    max_events: u64,
    expires_at: Option<DateTime<Local>>,
    one_shot: bool,
    delivered: AtomicU64,
    last_delivery: Mutex<Option<std::time::Instant>>,
    cancelled: AtomicBool,
    origin: &'static str,
}

/// 管理器。
pub struct MonitorManager {
    monitors: RwLock<HashMap<String, Arc<Monitor>>>,
    sink: Arc<dyn MonitorSink>,
}

impl MonitorManager {
    pub fn new(sink: Arc<dyn MonitorSink>) -> Arc<Self> {
        Arc::new(Self {
            monitors: RwLock::new(HashMap::new()),
            sink,
        })
    }

    /// 新增一个 monitor。运行时创建默认有界（2h、50 次）由调用方给出。
    #[allow(clippy::too_many_arguments)]
    pub async fn add(
        &self,
        name: String,
        source: String,
        timer: Option<TimerSpec>,
        when: Option<String>,
        prompt: Option<String>,
        as_prompt: bool,
        max_events: u64,
        expires: Option<Duration>,
        one_shot: bool,
        min_interval: Duration,
    ) -> Result<()> {
        {
            let mut monitors = self.monitors.write().await;
            if monitors.contains_key(&name) {
                anyhow::bail!("monitor {name:?} already exists");
            }
            let monitor = Arc::new(Monitor {
                name: name.clone(),
                source,
                when,
                prompt,
                as_prompt,
                min_interval,
                max_events,
                expires_at: expires.and_then(|duration| {
                    chrono::Duration::from_std(duration)
                        .ok()
                        .map(|duration| Local::now() + duration)
                }),
                one_shot,
                delivered: AtomicU64::new(0),
                last_delivery: Mutex::new(None),
                cancelled: AtomicBool::new(false),
                origin: "runtime",
            });
            monitors.insert(name.clone(), Arc::clone(&monitor));
            drop(monitors);
            if let Some(timer) = timer {
                spawn_timer(monitor, Arc::clone(&self.sink), timer);
            }
        }
        debug!(name, "monitor added");
        Ok(())
    }

    /// 删除并停止投递。
    pub async fn remove(&self, name: &str) -> bool {
        let removed = self.monitors.write().await.remove(name);
        if let Some(monitor) = removed {
            monitor.cancelled.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// 列出 monitor。
    pub async fn list(&self) -> Vec<serde_json::Value> {
        let monitors = self.monitors.read().await;
        let mut out: Vec<serde_json::Value> = monitors
            .values()
            .map(|monitor| {
                serde_json::json!({
                    "name": monitor.name,
                    "source": monitor.source,
                    "delivered": monitor.delivered.load(Ordering::Relaxed),
                    "max_events": monitor.max_events,
                    "one_shot": monitor.one_shot,
                    "origin": monitor.origin,
                    "expires_at": monitor.expires_at.map(|time| time.to_rfc3339()),
                })
            })
            .collect();
        out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        out
    }

    /// 手动触发一次。
    pub async fn fire(&self, name: &str) -> Result<()> {
        let monitor = {
            let monitors = self.monitors.read().await;
            monitors.get(name).cloned()
        };
        let Some(monitor) = monitor else {
            anyhow::bail!("monitor {name:?} does not exist");
        };
        deliver_once(&monitor, &*self.sink, None, 0).await;
        Ok(())
    }

    /// 喂入 QQ 事件；匹配的 monitor 各投递一次。
    pub async fn on_qq_event(&self, event: &QqEvent) {
        let Some(view) = MonitorEvent::from_qq(event) else {
            return;
        };
        let monitors: Vec<Arc<Monitor>> = {
            let monitors = self.monitors.read().await;
            monitors
                .values()
                .filter(|monitor| monitor.source == "qq")
                .cloned()
                .collect()
        };
        for monitor in monitors {
            match matches(monitor.when.as_deref(), &view) {
                Ok(true) => deliver_once(&monitor, &*self.sink, Some(&view), 1).await,
                Ok(false) => {}
                Err(error) => {
                    warn!(%error, monitor = %monitor.name, "monitor predicate failed");
                    self.sink
                        .deliver(MonitorDelivery {
                            name: monitor.name.clone(),
                            source: "internal".to_owned(),
                            as_prompt: false,
                            text: format!(
                                "system: monitor {:?} has an invalid `when` expression: {error}",
                                monitor.name
                            ),
                            system: true,
                        })
                        .await;
                }
            }
        }
    }
}

/// 投递一次：检查取消、到期、上限、冷却，然后写入 sink。
async fn deliver_once(
    monitor: &Monitor,
    sink: &dyn MonitorSink,
    event: Option<&MonitorEvent>,
    count: u64,
) {
    if monitor.cancelled.load(Ordering::SeqCst) {
        return;
    }
    if monitor
        .expires_at
        .is_some_and(|deadline| Local::now() > deadline)
    {
        monitor.cancelled.store(true, Ordering::SeqCst);
        sink.deliver(MonitorDelivery {
            name: monitor.name.clone(),
            source: monitor.source.clone(),
            as_prompt: false,
            text: format!(
                "system: monitor {:?} expired and was disabled",
                monitor.name
            ),
            system: true,
        })
        .await;
        return;
    }
    if monitor.delivered.load(Ordering::Relaxed) >= monitor.max_events {
        monitor.cancelled.store(true, Ordering::SeqCst);
        sink.deliver(MonitorDelivery {
            name: monitor.name.clone(),
            source: monitor.source.clone(),
            as_prompt: false,
            text: format!(
                "system: monitor {:?} reached its event limit and was disabled",
                monitor.name
            ),
            system: true,
        })
        .await;
        return;
    }
    {
        let mut last = monitor.last_delivery.lock().expect("monitor lock");
        if let Some(previous) = *last {
            if previous.elapsed() < monitor.min_interval {
                return;
            }
        }
        *last = Some(std::time::Instant::now());
    }

    let text = match (&monitor.prompt, event) {
        (Some(template), Some(event)) => template
            .replace("{{event.text}}", &event.text)
            .replace("{{event.chat}}", &event.chat)
            .replace("{{count}}", &count.to_string()),
        (Some(template), None) => template.clone(),
        (None, Some(event)) => format!(
            "[monitor {} | {} | {}]\n{}",
            monitor.name, monitor.source, event.chat, event.text
        ),
        (None, None) => format!(
            "[monitor {} | {}] fired manually",
            monitor.name, monitor.source
        ),
    };
    monitor.delivered.fetch_add(1, Ordering::Relaxed);
    sink.deliver(MonitorDelivery {
        name: monitor.name.clone(),
        source: monitor.source.clone(),
        as_prompt: monitor.as_prompt,
        text,
        system: false,
    })
    .await;

    if monitor.one_shot {
        monitor.cancelled.store(true, Ordering::SeqCst);
    }
}

/// 启动定时循环。
fn spawn_timer(monitor: Arc<Monitor>, sink: Arc<dyn MonitorSink>, timer: TimerSpec) {
    tokio::spawn(async move {
        loop {
            if monitor.cancelled.load(Ordering::SeqCst) {
                break;
            }
            let delay = match &timer {
                TimerSpec::Every(period) => *period,
                TimerSpec::At { hour, minute } => match next_local_time(*hour, *minute) {
                    Some(duration) => duration,
                    None => break,
                },
                TimerSpec::Cron(expression) => {
                    match CronSchedule::parse(expression)
                        .and_then(|cron| cron.next_delay(Local::now()))
                    {
                        Ok(duration) => duration,
                        Err(error) => {
                            sink.deliver(MonitorDelivery {
                                name: monitor.name.clone(),
                                source: "internal".to_owned(),
                                as_prompt: false,
                                text: format!(
                                    "system: monitor {:?} has an invalid cron expression: {error}",
                                    monitor.name
                                ),
                                system: true,
                            })
                            .await;
                            break;
                        }
                    }
                }
            };
            tokio::time::sleep(delay).await;
            if monitor.cancelled.load(Ordering::SeqCst) {
                break;
            }
            deliver_once(&monitor, &*sink, None, 1).await;
            if monitor.one_shot
                || monitor.cancelled.load(Ordering::SeqCst)
                || matches!(timer, TimerSpec::At { .. })
            {
                break;
            }
        }
    });
}

fn next_local_time(hour: u32, minute: u32) -> Option<Duration> {
    let now = Local::now();
    let mut target = Local
        .with_ymd_and_hms(now.year(), now.month(), now.day(), hour, minute, 0)
        .single()?;
    if target <= now {
        target += chrono::Duration::days(1);
    }
    (target - now).to_std().ok()
}

/// 5 字段 cron（本地时区）：分 时 日 月 周。
#[derive(Debug, Clone)]
pub struct CronSchedule {
    minutes: Vec<u32>,
    hours: Vec<u32>,
    days: Vec<u32>,
    months: Vec<u32>,
    weekdays: Vec<u32>,
}

impl CronSchedule {
    pub fn parse(expression: &str) -> Result<Self, String> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        if fields.len() != 5 {
            return Err("cron needs 5 fields: minute hour day month weekday".to_owned());
        }
        Ok(Self {
            minutes: parse_field(fields[0], 0, 59)?,
            hours: parse_field(fields[1], 0, 23)?,
            days: parse_field(fields[2], 1, 31)?,
            months: parse_field(fields[3], 1, 12)?,
            weekdays: parse_field(fields[4], 0, 7)?
                .into_iter()
                .map(|day| if day == 7 { 0 } else { day })
                .collect(),
        })
    }

    /// 距离下一次匹配还有多久（最多向前看一年）。
    pub fn next_delay(&self, from: DateTime<Local>) -> Result<Duration, String> {
        let mut candidate = (from + chrono::Duration::minutes(1))
            .with_second(0)
            .and_then(|time| time.with_nanosecond(0))
            .ok_or_else(|| "cannot normalize time".to_owned())?;
        for _ in 0..(366 * 24 * 60) {
            if self.matches(candidate) {
                return (candidate - from)
                    .to_std()
                    .map_err(|error| error.to_string());
            }
            candidate += chrono::Duration::minutes(1);
        }
        Err("cron never matches within a year".to_owned())
    }

    fn matches(&self, time: DateTime<Local>) -> bool {
        let weekday = time.weekday().num_days_from_sunday();
        self.minutes.contains(&time.minute())
            && self.hours.contains(&time.hour())
            && self.days.contains(&time.day())
            && self.months.contains(&time.month())
            && self.weekdays.contains(&weekday)
    }
}

/// 解析 cron 字段：`*`、`*/n`、`a-b`、`a-b/n`、逗号列表。
fn parse_field(field: &str, min: u32, max: u32) -> Result<Vec<u32>, String> {
    let mut values = Vec::new();
    for part in field.split(',') {
        let (range_part, step) = match part.split_once('/') {
            Some((range, step)) => (
                range,
                step.parse::<u32>()
                    .map_err(|_| format!("invalid step in {part:?}"))?,
            ),
            None => (part, 1),
        };
        if step == 0 {
            return Err("cron step must be greater than zero".to_owned());
        }
        let (start, end) = if range_part == "*" {
            (min, max)
        } else if let Some((start, end)) = range_part.split_once('-') {
            (
                start
                    .parse::<u32>()
                    .map_err(|_| format!("invalid range start in {part:?}"))?,
                end.parse::<u32>()
                    .map_err(|_| format!("invalid range end in {part:?}"))?,
            )
        } else {
            let value = range_part
                .parse::<u32>()
                .map_err(|_| format!("invalid value in {part:?}"))?;
            (value, value)
        };
        if start < min || end > max || start > end {
            return Err(format!("cron value out of range in {part:?}"));
        }
        let mut value = start;
        while value <= end {
            values.push(value);
            value += step;
        }
    }
    values.sort_unstable();
    values.dedup();
    if values.is_empty() {
        return Err("cron field is empty".to_owned());
    }
    Ok(values)
}

/// 从配置构造长期常驻 monitor。
pub async fn load_config_monitors(
    manager: &Arc<MonitorManager>,
    monitors: &[crate::config::MonitorConfig],
) -> Result<()> {
    for monitor in monitors.iter().filter(|monitor| monitor.enabled) {
        let (source, timer) = if monitor.source.qq.is_some() {
            ("qq".to_owned(), None)
        } else if let Some(timer) = &monitor.source.timer {
            let spec = if let Some(every) = &timer.every {
                let seconds =
                    crate::config::parse_duration_secs(every).context("timer.every is invalid")?;
                TimerSpec::Every(Duration::from_secs(seconds))
            } else if let Some(at) = &timer.at {
                parse_at(at).context("timer.at is invalid")?
            } else if let Some(cron) = &timer.cron {
                CronSchedule::parse(cron).map_err(|error| anyhow::anyhow!(error))?;
                TimerSpec::Cron(cron.clone())
            } else {
                continue;
            };
            ("timer".to_owned(), Some(spec))
        } else if let Some(internal) = &monitor.source.internal {
            (format!("internal:{}", internal.kind), None)
        } else if monitor.source.stream.is_some() {
            ("stream".to_owned(), None)
        } else {
            continue;
        };

        let expires = match monitor.lifecycle.expires.as_str() {
            "0" => None,
            raw => crate::config::parse_duration_secs(raw).map(Duration::from_secs),
        };
        manager
            .add(
                monitor.name.clone(),
                source,
                timer,
                monitor.when.clone(),
                if monitor.deliver.as_ == crate::config::DeliverAs::Prompt {
                    Some(monitor.deliver.prompt.clone())
                } else {
                    None
                },
                monitor.deliver.as_ == crate::config::DeliverAs::Prompt,
                monitor.lifecycle.max_events,
                expires,
                monitor.lifecycle.one_shot,
                Duration::from_secs(monitor.lifecycle.min_interval_secs),
            )
            .await?;
    }
    Ok(())
}

fn parse_at(raw: &str) -> Result<TimerSpec> {
    let (hour, minute) = raw.split_once(':').context("timer.at must be HH:MM")?;
    Ok(TimerSpec::At {
        hour: hour.parse().context("invalid hour")?,
        minute: minute.parse().context("invalid minute")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingSink {
        deliveries: Mutex<Vec<MonitorDelivery>>,
    }

    #[async_trait::async_trait]
    impl MonitorSink for RecordingSink {
        async fn deliver(&self, delivery: MonitorDelivery) {
            self.deliveries.lock().unwrap().push(delivery);
        }
    }

    fn event(chat: &str, text: &str, mentions: bool) -> MonitorEvent {
        MonitorEvent {
            kind: "message".to_owned(),
            chat: chat.to_owned(),
            sender: Some(10001),
            text: text.to_owned(),
            mentions_self: mentions,
        }
    }

    #[test]
    fn predicates_support_the_catalog_subset() {
        let event = event("group:123", "报名了", true);
        assert!(matches(None, &event).unwrap());
        assert!(matches(Some("event.mentions_self"), &event).unwrap());
        assert!(matches(Some("event.type == 'message'"), &event).unwrap());
        assert!(matches(Some("event.chat == 'group:123'"), &event).unwrap());
        assert!(matches(Some("event.text contains '报名'"), &event).unwrap());
        assert!(
            matches(
                Some("event.mentions_self and event.chat == 'group:123'"),
                &event
            )
            .unwrap()
        );
        assert!(!matches(Some("event.chat == 'private:1'"), &event).unwrap());
        assert!(matches(Some("event.nonexistent"), &event).is_err());
    }

    #[test]
    fn cron_parser_handles_ranges_steps_and_lists() {
        let schedule = CronSchedule::parse("*/15 9-11 * * 1-5").unwrap();
        assert_eq!(schedule.minutes, vec![0, 15, 30, 45]);
        assert_eq!(schedule.hours, vec![9, 10, 11]);
        assert_eq!(schedule.weekdays, vec![1, 2, 3, 4, 5]);
        assert!(CronSchedule::parse("* * *").is_err());
        assert!(CronSchedule::parse("60 * * * *").is_err());
    }

    #[test]
    fn cron_next_delay_is_within_a_minute() {
        let schedule = CronSchedule::parse("* * * * *").unwrap();
        assert!(schedule.next_delay(Local::now()).unwrap() <= Duration::from_secs(60));
    }

    #[tokio::test]
    async fn qq_monitor_delivers_matching_events_only() {
        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        manager
            .add(
                "at-me".to_owned(),
                "qq".to_owned(),
                None,
                Some("event.mentions_self".to_owned()),
                None,
                false,
                100,
                None,
                false,
                Duration::ZERO,
            )
            .await
            .unwrap();

        let raw = serde_json::json!({
            "post_type": "message",
            "message_type": "group",
            "self_id": 1,
            "group_id": 123,
            "user_id": 2,
            "message_id": 3,
            "time": 1793304000,
            "sender": {"nickname": "n"},
            "message": [{"type": "text", "data": {"text": "hi"}}, {"type": "at", "data": {"qq": "1"}}]
        });
        let event = crate::qq::event::normalize("message.group.normal", &raw);
        manager.on_qq_event(&event).await;
        manager.on_qq_event(&event).await;

        let deliveries = sink.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 2);
        assert!(deliveries[0].text.contains("hi"));
    }

    #[tokio::test]
    async fn max_events_disables_and_notifies() {
        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        manager
            .add(
                "once".to_owned(),
                "qq".to_owned(),
                None,
                None,
                None,
                false,
                1,
                None,
                false,
                Duration::ZERO,
            )
            .await
            .unwrap();
        manager.fire("once").await.unwrap();
        manager.fire("once").await.unwrap();
        let deliveries = sink.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 2);
        assert!(deliveries[1].system);
        assert!(deliveries[1].text.contains("event limit"));
    }

    #[tokio::test]
    async fn remove_and_list() {
        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        manager
            .add(
                "m".to_owned(),
                "qq".to_owned(),
                None,
                None,
                None,
                false,
                10,
                None,
                false,
                Duration::ZERO,
            )
            .await
            .unwrap();
        assert_eq!(manager.list().await.len(), 1);
        assert!(manager.remove("m").await);
        assert!(!manager.remove("m").await);
        assert!(manager.list().await.is_empty());
    }
}
