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

use crate::config::CatchUp;
use crate::qq::event::QqEvent;
use crate::qq::format;
use crate::storage::Database;

/// 单个 monitor 同时等待投递的任务上限。
///
/// `idle_only` 的任务最多等 10 分钟；事件洪泛时不能无限 spawn。
const MAX_PENDING_DELIVERIES: usize = 8;

/// 投递内容。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorDelivery {
    pub name: String,
    pub source: String,
    /// `None` = 主代理；`Some("sa-…")` = 绑定的子代理实例；`Some("agent:<name>")` = 按定义名。
    pub target: Option<String>,
    /// `true` 表示作为指令投递（`as = "prompt"`）。
    pub as_prompt: bool,
    pub text: String,
    /// 系统通知（到期、上限、谓词失败）——不来自任何用户。
    pub system: bool,
    /// 事件来自主人消息；投递后回合拥有 owner 证据。
    #[serde(default)]
    pub owner_intent: bool,
    /// 目标不存在时的处理方式。
    pub on_missing: crate::config::OnMissing,
    /// QQ 消息唤醒时携带原消息，交给 ChatQueue 走统一去抖/并发。
    #[serde(default)]
    pub message: Option<crate::qq::event::QqMessage>,
}

/// 投递端；由 runtime 提供。
#[async_trait::async_trait]
pub trait MonitorSink: Send + Sync {
    async fn deliver(&self, delivery: MonitorDelivery);

    /// agent 是否空闲；`idle_only` 的 monitor 会等待这里返回 `true`。
    async fn is_idle(&self) -> bool {
        true
    }
}

/// 事件视图。
#[derive(Debug, Clone, Default)]
pub struct MonitorEvent {
    pub kind: String,
    pub chat: String,
    pub sender: Option<u64>,
    pub text: String,
    pub mentions_self: bool,
    /// 该事件是主人发的消息；monitor 回合可据此获得 owner 证据。
    pub owner_intent: bool,
    /// 原始 QQ 消息；用于把唤醒重新交给 ChatQueue。
    pub message: Option<crate::qq::event::QqMessage>,
}

impl MonitorEvent {
    /// 从 QQ 事件构造；非消息/通知/请求返回 `None`。
    pub fn from_qq(event: &QqEvent, self_id: u64) -> Option<Self> {
        match event {
            QqEvent::Message(message) => Some(Self {
                kind: "message".to_owned(),
                chat: message.chat.to_string(),
                sender: message.sender.qq,
                text: message.plain_text(),
                mentions_self: message.mentions_self(self_id),
                owner_intent: false,
                message: Some(message.clone()),
            }),
            QqEvent::Notice(notice) => Some(Self {
                kind: format!("notice:{}", notice.kind),
                chat: notice.chat.map(|chat| chat.to_string()).unwrap_or_default(),
                sender: notice.actor,
                text: notice
                    .to_inbox_message()
                    .map(|message| message.plain_text())
                    .unwrap_or_default(),
                mentions_self: false,
                owner_intent: false,
                message: None,
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
                owner_intent: false,
                message: None,
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
    if let Some(value) = term.strip_prefix("event.sender != ") {
        return Ok(event.sender.map(|sender| sender.to_string()).as_deref() != Some(unquote(value)));
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
    /// 投递目标；`None` 表示主代理。
    target: Option<String>,
    when: Option<String>,
    prompt: Option<String>,
    as_prompt: bool,
    min_interval: Duration,
    max_events: u64,
    expires_at: Option<DateTime<Local>>,
    one_shot: bool,
    /// 只在 agent 空闲时投递。
    idle_only: bool,
    /// 批处理窗口；0 表示逐个投递。
    batch_window: Duration,
    /// 待合并的事件文本。
    batch: Mutex<Vec<String>>,
    /// 是否已有 flush 任务在等待。
    flush_queued: AtomicBool,
    delivered: AtomicU64,
    /// 已 spawn、尚未完成的投递任务数（含等待空闲的）。
    pending_deliveries: std::sync::atomic::AtomicUsize,
    last_delivery: Mutex<Option<std::time::Instant>>,
    cancelled: AtomicBool,
    origin: &'static str,
    /// 定时源错过触发时的处理方式。
    catch_up: CatchUp,
    /// 记录上次触发时间，供重启后补投。
    db: Option<Arc<Database>>,
    /// 是否把投递抄送主代理。
    cc: bool,
    /// 目标不存在时：丢弃 / 排队 / 新建子代理。
    on_missing: crate::config::OnMissing,
}

/// 管理器。
pub struct MonitorManager {
    monitors: RwLock<HashMap<String, Arc<Monitor>>>,
    sink: Arc<dyn MonitorSink>,
    db: std::sync::RwLock<Option<Arc<Database>>>,
    self_id: AtomicU64,
    owner_qq: AtomicU64,
}

impl MonitorManager {
    pub fn new(sink: Arc<dyn MonitorSink>) -> Arc<Self> {
        Arc::new(Self {
            monitors: RwLock::new(HashMap::new()),
            sink,
            db: std::sync::RwLock::new(None),
            self_id: AtomicU64::new(0),
            owner_qq: AtomicU64::new(0),
        })
    }

    /// 登录账号；用于 `@` 判定。
    pub fn set_self_id(&self, self_id: u64) {
        self.self_id.store(self_id, Ordering::Relaxed);
    }

    /// 主人 QQ；主人消息触发的 monitor 投递拥有 owner 证据。
    pub fn set_owner_qq(&self, owner_qq: u64) {
        self.owner_qq.store(owner_qq, Ordering::Relaxed);
    }

    /// 接入数据库：定时源把上次触发时间写进 `meta`，重启后可补投。
    pub fn set_db(&self, db: Arc<Database>) {
        *self.db.write().unwrap_or_else(|e| e.into_inner()) = Some(db);
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
        idle_only: bool,
        batch_window: Duration,
        target: Option<String>,
        catch_up: CatchUp,
        cc: bool,
        on_missing: crate::config::OnMissing,
    ) -> Result<()> {
        if let Some(db) = self.db.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = db.meta_set(&format!("monitor.removed.{name}"), b"0");
        }
        {
            let mut monitors = self.monitors.write().await;
            if monitors.contains_key(&name) {
                anyhow::bail!("monitor {name:?} already exists");
            }
            let monitor = Arc::new(Monitor {
                name: name.clone(),
                source,
                target,
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
                idle_only,
                batch_window,
                batch: Mutex::new(Vec::new()),
                flush_queued: AtomicBool::new(false),
                delivered: AtomicU64::new(0),
                pending_deliveries: std::sync::atomic::AtomicUsize::new(0),
                last_delivery: Mutex::new(None),
                cancelled: AtomicBool::new(false),
                origin: "runtime",
                catch_up,
                db: self.db.read().unwrap_or_else(|e| e.into_inner()).clone(),
                cc,
                on_missing,
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
            if name.starts_with("wake-on-")
                && let Some(db) = self.db.read().unwrap_or_else(|e| e.into_inner()).as_ref()
            {
                let _ = db.meta_set(&format!("monitor.removed.{name}"), b"1");
            }
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
                    "target": monitor.target,
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

    /// 工具调用用：后台投递，避免在工具栈里同步跑回合、重入 turn_lock。
    pub async fn fire_async(&self, name: &str) -> Result<()> {
        let monitor = {
            let monitors = self.monitors.read().await;
            monitors.get(name).cloned()
        };
        let Some(monitor) = monitor else {
            anyhow::bail!("monitor {name:?} does not exist");
        };
        let sink = Arc::clone(&self.sink);
        tokio::spawn(async move {
            deliver_once(&monitor, &*sink, None, 0).await;
        });
        Ok(())
    }

    /// 以一条合成事件投递一次；用于 `backlog = "inbox"` 的首次补投。
    pub async fn deliver_text(&self, name: &str, text: String) -> Result<()> {
        let monitor = {
            let monitors = self.monitors.read().await;
            monitors.get(name).cloned()
        };
        let Some(monitor) = monitor else {
            anyhow::bail!("monitor {name:?} does not exist");
        };
        let event = MonitorEvent {
            text,
            ..MonitorEvent::default()
        };
        deliver_once(&monitor, &*self.sink, Some(&event), 1).await;
        Ok(())
    }

    /// 喂入 QQ 事件；匹配的 monitor 各投递一次。
    pub async fn on_qq_event(&self, event: &QqEvent) {
        let Some(mut view) = MonitorEvent::from_qq(event, self.self_id.load(Ordering::Relaxed))
        else {
            return;
        };
        let owner_qq = self.owner_qq.load(Ordering::Relaxed);
        view.owner_intent =
            view.kind == "message" && owner_qq != 0 && view.sender == Some(owner_qq);
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
                Ok(true) => self.enqueue(&monitor, &view).await,
                Ok(false) => {}
                Err(error) => {
                    warn!(%error, monitor = %monitor.name, "monitor predicate failed");
                    self.sink
                        .deliver(MonitorDelivery {
                            name: monitor.name.clone(),
                            source: "internal".to_owned(),
                            target: monitor.target.clone(),
                            as_prompt: false,
                            text: format!(
                                "system: monitor {:?} has an invalid `when` expression: {error}",
                                monitor.name
                            ),
                            system: true,
                            owner_intent: false,
                            message: None,
                            on_missing: monitor.on_missing,
                        })
                        .await;
                }
            }
        }
    }

    /// 按批处理窗口投递事件；窗口为 0 时立即投递。
    ///
    /// 投递在独立任务里执行，避免 `idle_only` 等待空闲时阻塞 QQ 事件处理。
    async fn enqueue(&self, monitor: &Arc<Monitor>, event: &MonitorEvent) {
        if monitor.pending_deliveries.load(Ordering::SeqCst) >= MAX_PENDING_DELIVERIES {
            debug!(
                monitor = %monitor.name,
                "too many pending deliveries; dropping this event"
            );
            return;
        }
        if monitor.batch_window.is_zero() {
            // 无批处理且无需等空闲时直接投递，保持调用方看到同步结果。
            if !monitor.idle_only {
                monitor.pending_deliveries.fetch_add(1, Ordering::SeqCst);
                deliver_once(monitor, &*self.sink, Some(event), 1).await;
                monitor.pending_deliveries.fetch_sub(1, Ordering::SeqCst);
                return;
            }
            let monitor = Arc::clone(monitor);
            let sink = Arc::clone(&self.sink);
            let event = event.clone();
            monitor.pending_deliveries.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                deliver_once(&monitor, &*sink, Some(&event), 1).await;
                monitor.pending_deliveries.fetch_sub(1, Ordering::SeqCst);
            });
            return;
        }

        monitor
            .batch
            .lock()
            .expect("monitor lock")
            .push(format!("[{}] {}", event.chat, event.text));
        if monitor.flush_queued.swap(true, Ordering::SeqCst) {
            return;
        }
        let monitor = Arc::clone(monitor);
        let sink = Arc::clone(&self.sink);
        monitor.pending_deliveries.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            tokio::time::sleep(monitor.batch_window).await;
            let texts = std::mem::take(&mut *monitor.batch.lock().expect("monitor lock"));
            monitor.flush_queued.store(false, Ordering::SeqCst);
            if texts.is_empty() {
                monitor.pending_deliveries.fetch_sub(1, Ordering::SeqCst);
                return;
            }
            let event = MonitorEvent {
                text: texts.join("\n"),
                ..MonitorEvent::default()
            };
            deliver_once(&monitor, &*sink, Some(&event), texts.len() as u64).await;
            monitor.pending_deliveries.fetch_sub(1, Ordering::SeqCst);
        });
    }
}
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
            target: monitor.target.clone(),
            as_prompt: false,
            text: format!(
                "system: monitor {:?} expired and was disabled",
                monitor.name
            ),
            system: true,
            owner_intent: false,
            message: None,
            on_missing: monitor.on_missing,
        })
        .await;
        return;
    }
    // idle_only：等到 agent 空闲再投递；最多等 10 分钟，超时放弃本次事件。
    if monitor.idle_only {
        let deadline = std::time::Instant::now() + Duration::from_secs(600);
        while !sink.is_idle().await {
            if monitor.cancelled.load(Ordering::SeqCst)
                || monitor
                    .expires_at
                    .is_some_and(|deadline| Local::now() > deadline)
                || std::time::Instant::now() > deadline
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    {
        let mut last = monitor.last_delivery.lock().expect("monitor lock");
        if let Some(previous) = *last
            && previous.elapsed() < monitor.min_interval
        {
            return;
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
    let previous = monitor.delivered.fetch_add(1, Ordering::SeqCst);
    if previous >= monitor.max_events {
        // 回滚预留；并发的两个投递只有一个能拿到最后一个名额。
        monitor.delivered.fetch_sub(1, Ordering::SeqCst);
        if !monitor.cancelled.swap(true, Ordering::SeqCst) {
            sink.deliver(MonitorDelivery {
                name: monitor.name.clone(),
                source: monitor.source.clone(),
                target: monitor.target.clone(),
                as_prompt: false,
                text: format!(
                    "system: monitor {:?} reached its event limit and was disabled",
                    monitor.name
                ),
                system: true,
                owner_intent: false,
                message: None,
                on_missing: monitor.on_missing,
            })
            .await;
        }
        return;
    }

    sink.deliver(MonitorDelivery {
        name: monitor.name.clone(),
        source: monitor.source.clone(),
        target: monitor.target.clone(),
        as_prompt: monitor.as_prompt,
        text: text.clone(),
        system: false,
        owner_intent: event.as_ref().is_some_and(|event| event.owner_intent),
        message: event.as_ref().and_then(|event| event.message.clone()),
        on_missing: monitor.on_missing,
    })
    .await;

    if monitor.cc && monitor.target.is_some() {
        sink.deliver(MonitorDelivery {
            name: monitor.name.clone(),
            source: monitor.source.clone(),
            target: None,
            as_prompt: monitor.as_prompt,
            text: format!("[cc {}] {text}", monitor.name),
            system: false,
            owner_intent: false,
            message: None,
            on_missing: crate::config::OnMissing::Queue,
        })
        .await;
    }

    if monitor.one_shot {
        monitor.cancelled.store(true, Ordering::SeqCst);
    }
}

/// 启动定时循环。
fn spawn_timer(monitor: Arc<Monitor>, sink: Arc<dyn MonitorSink>, timer: TimerSpec) {
    tokio::spawn(async move {
        let now = Local::now();
        // 重启补投：只补一次（`coalesce`），`skip` 直接放弃。
        if monitor.catch_up == CatchUp::Coalesce
            && let Some(last) = read_last_fire(monitor.db.as_ref(), &monitor.name)
        {
            let missed = missed_occurrences(&timer, last, now);
            if missed > 0 {
                let event = MonitorEvent {
                    text: format!("[timer catch-up] {missed} missed occurrence(s)"),
                    ..MonitorEvent::default()
                };
                deliver_once(&monitor, &*sink, Some(&event), missed).await;
            }
        }
        write_last_fire(monitor.db.as_ref(), &monitor.name, now);

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
                                target: monitor.target.clone(),
                                as_prompt: false,
                                text: format!(
                                    "system: monitor {:?} has an invalid cron expression: {error}",
                                    monitor.name
                                ),
                                system: true,
                                owner_intent: false,
                                message: None,
                                on_missing: monitor.on_missing,
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
            write_last_fire(monitor.db.as_ref(), &monitor.name, Local::now());
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

fn last_fire_key(name: &str) -> String {
    format!("monitor.last:{name}")
}

fn read_last_fire(db: Option<&Arc<Database>>, name: &str) -> Option<DateTime<Local>> {
    let db = db?;
    let raw = db.meta_get(&last_fire_key(name)).ok()??;
    let text = String::from_utf8(raw).ok()?;
    let timestamp: i64 = text.trim().parse().ok()?;
    DateTime::from_timestamp(timestamp, 0).map(|utc| utc.with_timezone(&Local))
}

fn write_last_fire(db: Option<&Arc<Database>>, name: &str, at: DateTime<Local>) {
    let Some(db) = db else {
        return;
    };
    let _ = db.meta_set(&last_fire_key(name), at.timestamp().to_string().as_bytes());
}

/// 计算 `last` 到 `now` 之间错过的触发次数（有上限，避免长停机扫太久）。
pub(crate) fn missed_occurrences(
    timer: &TimerSpec,
    last: DateTime<Local>,
    now: DateTime<Local>,
) -> u64 {
    if now <= last {
        return 0;
    }
    match timer {
        TimerSpec::Every(period) => {
            let seconds = (now - last).num_seconds().max(0) as u64;
            (seconds / period.as_secs().max(1)).min(1_000)
        }
        TimerSpec::At { hour, minute } => {
            let mut count = 0u64;
            let mut day = last.date_naive();
            let end = now.date_naive();
            let mut guard = 0;
            while day <= end && guard < 400 {
                if let Some(naive) = day.and_hms_opt(*hour, *minute, 0)
                    && let Some(at) = Local.from_local_datetime(&naive).single()
                    && at > last
                    && at <= now
                {
                    count += 1;
                }
                let Some(next) = day.succ_opt() else {
                    break;
                };
                day = next;
                guard += 1;
            }
            count
        }
        TimerSpec::Cron(expression) => {
            let Ok(cron) = CronSchedule::parse(expression) else {
                return 0;
            };
            let mut at = last.with_second(0).unwrap_or(last) + chrono::Duration::minutes(1);
            let mut count = 0u64;
            let mut guard = 0;
            while at <= now && guard < 1_440 {
                if cron.matches(at) {
                    count += 1;
                }
                at += chrono::Duration::minutes(1);
                guard += 1;
            }
            count
        }
    }
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
    // 用于 `backlog = "inbox"` 的首次补投；`None` 时跳过。
    messages: Option<&Arc<crate::storage::messages::MessageStore>>,
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
                monitor.lifecycle.idle_only,
                Duration::from_millis(monitor.deliver.batch_window_ms),
                if monitor.deliver.to == "main" {
                    None
                } else {
                    Some(monitor.deliver.to.clone())
                },
                monitor.lifecycle.catch_up,
                monitor.deliver.cc,
                monitor.on_missing,
            )
            .await?;

        // `backlog = "inbox"`：创建时把匹配的未读消息补投一次；
        // 只有真正投递出去的条目才标记已读。
        let wants_backlog = matches!(
            monitor.source.qq,
            Some(crate::config::QqSource::Detailed(ref config))
                if config.backlog == crate::config::BacklogMode::Inbox
        );
        if wants_backlog && let Some(messages) = messages {
            let mut lines = Vec::new();
            for message in messages.unread(None).unwrap_or_default() {
                let event = MonitorEvent {
                    kind: "message".to_owned(),
                    chat: message.chat.to_string(),
                    sender: message.sender.qq,
                    text: message.plain_text(),
                    mentions_self: message
                        .segments
                        .iter()
                        .any(|segment| matches!(segment, crate::qq::event::Segment::At { .. })),
                    owner_intent: false,
                    message: None,
                };
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

    struct RecordingSink {
        deliveries: Mutex<Vec<MonitorDelivery>>,
        idle: AtomicBool,
    }

    impl Default for RecordingSink {
        fn default() -> Self {
            Self {
                deliveries: Mutex::new(Vec::new()),
                // 默认空闲，既有测试不进入等待。
                idle: AtomicBool::new(true),
            }
        }
    }

    #[async_trait::async_trait]
    impl MonitorSink for RecordingSink {
        async fn deliver(&self, delivery: MonitorDelivery) {
            self.deliveries.lock().unwrap().push(delivery);
        }

        async fn is_idle(&self) -> bool {
            self.idle.load(Ordering::SeqCst)
        }
    }

    fn event(chat: &str, text: &str, mentions: bool) -> MonitorEvent {
        MonitorEvent {
            kind: "message".to_owned(),
            chat: chat.to_owned(),
            sender: Some(10001),
            text: text.to_owned(),
            mentions_self: mentions,
            owner_intent: false,
            message: None,
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

    #[test]
    fn catch_up_counts_missed_occurrences() {
        let last = Local.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap();
        let now = last + chrono::Duration::minutes(5);
        assert_eq!(
            missed_occurrences(&TimerSpec::Every(Duration::from_secs(60)), last, now),
            5
        );
        assert_eq!(
            missed_occurrences(&TimerSpec::Cron("* * * * *".to_owned()), last, now),
            5
        );
        // 时间倒过来不计。
        assert_eq!(
            missed_occurrences(&TimerSpec::Every(Duration::from_secs(60)), now, last),
            0
        );
        // 每天固定时刻：跨两天补两次。
        let last = Local.with_ymd_and_hms(2026, 11, 2, 10, 0, 0).unwrap();
        let now = Local.with_ymd_and_hms(2026, 11, 4, 10, 30, 0).unwrap();
        assert_eq!(
            missed_occurrences(&TimerSpec::At { hour: 9, minute: 0 }, last, now),
            2
        );
    }

    #[tokio::test]
    async fn qq_monitor_delivers_matching_events_only() {
        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        manager.set_self_id(1);
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
                false,
                Duration::ZERO,
                None,
                CatchUp::Coalesce,
                false,
                crate::config::OnMissing::Queue,
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
    async fn cc_delivers_a_copy_to_the_main_agent() {
        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        manager
            .add(
                "cced".to_owned(),
                "qq".to_owned(),
                None,
                None,
                None,
                false,
                10,
                None,
                false,
                Duration::ZERO,
                false,
                Duration::ZERO,
                Some("sa-1".to_owned()),
                CatchUp::Coalesce,
                true,
                crate::config::OnMissing::Queue,
            )
            .await
            .unwrap();
        manager.fire("cced").await.unwrap();
        let deliveries = sink.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 2);
        assert_eq!(deliveries[0].target.as_deref(), Some("sa-1"));
        assert!(deliveries[1].target.is_none());
        assert!(deliveries[1].text.contains("[cc cced]"));
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
                false,
                Duration::ZERO,
                None,
                CatchUp::Coalesce,
                false,
                crate::config::OnMissing::Queue,
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
                false,
                Duration::ZERO,
                None,
                CatchUp::Coalesce,
                false,
                crate::config::OnMissing::Queue,
            )
            .await
            .unwrap();
        assert_eq!(manager.list().await.len(), 1);
        assert!(manager.remove("m").await);
        assert!(!manager.remove("m").await);
        assert!(manager.list().await.is_empty());
    }

    #[tokio::test]
    async fn batch_window_merges_events_into_one_delivery() {
        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        manager
            .add(
                "batched".to_owned(),
                "qq".to_owned(),
                None,
                None,
                None,
                false,
                10,
                None,
                false,
                Duration::ZERO,
                false,
                Duration::from_millis(50),
                None,
                CatchUp::Coalesce,
                false,
                crate::config::OnMissing::Queue,
            )
            .await
            .unwrap();

        let first = event("group:1001", "first", false);
        let second = event("group:1001", "second", false);
        let monitor = manager
            .monitors
            .read()
            .await
            .get("batched")
            .cloned()
            .unwrap();
        manager.enqueue(&monitor, &first).await;
        manager.enqueue(&monitor, &second).await;
        // 窗口未到时不应投递。
        assert!(sink.deliveries.lock().unwrap().is_empty());

        tokio::time::sleep(Duration::from_millis(120)).await;
        let deliveries = sink.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert!(deliveries[0].text.contains("first"));
        assert!(deliveries[0].text.contains("second"));
    }

    #[tokio::test]
    async fn idle_only_waits_for_the_agent() {
        let sink = Arc::new(RecordingSink::default());
        sink.idle.store(false, Ordering::SeqCst);
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        manager
            .add(
                "idle".to_owned(),
                "qq".to_owned(),
                None,
                None,
                None,
                false,
                10,
                None,
                false,
                Duration::ZERO,
                true,
                Duration::ZERO,
                None,
                CatchUp::Coalesce,
                false,
                crate::config::OnMissing::Queue,
            )
            .await
            .unwrap();

        let event = event("private:10001", "hello", false);
        let monitor = manager.monitors.read().await.get("idle").cloned().unwrap();
        manager.enqueue(&monitor, &event).await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(sink.deliveries.lock().unwrap().is_empty());

        sink.idle.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(sink.deliveries.lock().unwrap().len(), 1);
    }

    #[test]
    fn sender_not_equal_predicate() {
        let event = MonitorEvent {
            kind: "message".to_owned(),
            chat: "group:1".to_owned(),
            sender: Some(42),
            text: "hi".to_owned(),
            mentions_self: true,
            owner_intent: false,
            message: None,
        };
        assert!(
            matches(
                Some("event.type == 'message' and event.mentions_self and event.sender != '7'"),
                &event
            )
            .unwrap()
        );
        assert!(
            !matches(
                Some("event.type == 'message' and event.sender != '42'"),
                &event
            )
            .unwrap()
        );
    }

    #[tokio::test]
    async fn removing_a_wake_preset_is_remembered() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        manager.set_db(Arc::clone(&db));
        manager
            .add(
                "wake-on-owner".to_owned(),
                "qq".to_owned(),
                None,
                Some("event.type == 'message'".to_owned()),
                None,
                false,
                10,
                None,
                false,
                Duration::ZERO,
                false,
                Duration::ZERO,
                None,
                CatchUp::Coalesce,
                false,
                crate::config::OnMissing::Queue,
            )
            .await
            .unwrap();
        assert!(manager.remove("wake-on-owner").await);
        assert_eq!(
            db.meta_get("monitor.removed.wake-on-owner")
                .unwrap()
                .as_deref(),
            Some(&b"1"[..])
        );
    }

    #[tokio::test]
    async fn owner_message_delivery_carries_owner_intent() {
        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        manager.set_owner_qq(322);
        manager
            .add(
                "owner".to_owned(),
                "qq".to_owned(),
                None,
                Some("event.sender == '322'".to_owned()),
                None,
                false,
                10,
                None,
                false,
                Duration::ZERO,
                false,
                Duration::ZERO,
                None,
                CatchUp::Coalesce,
                false,
                crate::config::OnMissing::Queue,
            )
            .await
            .unwrap();
        let raw = serde_json::json!({
            "post_type": "message",
            "message_type": "private",
            "self_id": 1,
            "user_id": 322,
            "message_id": 9,
            "time": 1793304000,
            "sender": {"nickname": "owner"},
            "message": "hello"
        });
        let event = crate::qq::event::normalize("message.private.normal", &raw);
        manager.on_qq_event(&event).await;
        let deliveries = sink.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert!(deliveries[0].owner_intent);
    }
}
