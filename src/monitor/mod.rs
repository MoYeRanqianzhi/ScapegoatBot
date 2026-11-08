//! 监视器：事件流到消息的订阅。
//!
//! 设计依据：设计 §5.11。
//!
//! 结构：`{source, when?, deliver, lifecycle?}`。
//! - source：`qq` / `timer` / `internal`；`stream` 只允许写在配置文件里；
//! - when：谓词子集（`event.mentions_self`、`event.type == 'message'`、
//!   `event.chat == 'group:123'`、`event.sender == '10001'` / `!=`、`event.text contains 'x'`，
//!   前缀 `not` 取反，用 `and` 组合）；
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

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::config::CatchUp;
use crate::qq::event::QqEvent;
use crate::qq::format;
use crate::storage::Database;

mod timer;

pub use timer::Timer;
use timer::spawn_timer;

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

/// 谓词求值。表达式在 [`validate_when`] 里已经校验过；这里的错误只是防御。
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

/// 校验 `when` 表达式：每一项都必须是受支持的谓词。
///
/// 必须逐项检查：`matches` 遇到第一个为假的项就短路，后面写错的项可能永远不被求值，
/// 直到某个事件恰好让前面的项都为真才暴露。
pub fn validate_when(when: &str) -> Result<(), String> {
    let probe = MonitorEvent {
        kind: String::new(),
        chat: String::new(),
        sender: None,
        text: String::new(),
        mentions_self: false,
        owner_intent: false,
        message: None,
    };
    let mut terms = 0;
    for term in when.split(" and ") {
        let term = term.trim();
        if term.is_empty() {
            return Err(format!("empty term in `when` expression {when:?}"));
        }
        match_term(term, &probe)?;
        terms += 1;
    }
    if terms == 0 {
        return Err("`when` expression is empty".to_owned());
    }
    Ok(())
}

fn match_term(term: &str, event: &MonitorEvent) -> Result<bool, String> {
    // `not <term>` 对任意受支持的项取反。
    if let Some(inner) = term.strip_prefix("not ") {
        return match_term(inner.trim(), event).map(|matched| !matched);
    }
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

impl Monitor {
    /// 构造一条系统通知（到期、上限、谓词失败）；它们不来自任何用户。
    ///
    /// `source` 保留各调用点原来的分类：配置错误记 `internal`，生命周期事件记
    /// monitor 自身的 source。
    fn system_delivery(&self, source: &str, text: String) -> MonitorDelivery {
        MonitorDelivery {
            name: self.name.clone(),
            source: source.to_owned(),
            target: self.target.clone(),
            as_prompt: false,
            text,
            system: true,
            owner_intent: false,
            message: None,
            on_missing: self.on_missing,
        }
    }

    /// `min_interval` 内的重复投递一律拒绝；通过时占用当前时间窗。
    fn rate_limit_ok(&self) -> bool {
        let mut last = self.last_delivery.lock().expect("monitor lock");
        if let Some(previous) = *last
            && previous.elapsed() < self.min_interval
        {
            return false;
        }
        *last = Some(std::time::Instant::now());
        true
    }
}

/// 一个 monitor 的完整定义；`MonitorManager::add` 的唯一参数。
///
/// 字段与 `[[monitor]]` 配置一一对应，位置无关：运行时创建、配置文件加载与
/// 唤醒预设都用具名字面量构造，布尔与 `Duration` 不会再被顺序放错。
pub struct MonitorSpec {
    /// 唯一名字；重复时 `add` 报错。
    pub name: String,
    /// `qq` / `timer` / `internal:<kind>` / `stream`。
    pub source: String,
    /// 定时源；`None` 表示事件驱动。
    pub timer: Option<Timer>,
    /// 谓词表达式；`None` 匹配该 source 的全部事件。
    pub when: Option<String>,
    /// `as_prompt = true` 时的模板，支持 `{{event.text}}` / `{{event.chat}}` / `{{count}}`。
    pub prompt: Option<String>,
    /// 投递形式：`true` 投递指令，`false` 投递原始事件。
    pub as_prompt: bool,
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
    pub on_missing: crate::config::OnMissing,
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
    pub async fn add(&self, spec: MonitorSpec) -> Result<()> {
        let MonitorSpec {
            name,
            source,
            timer,
            when,
            prompt,
            as_prompt,
            max_events,
            expires,
            one_shot,
            min_interval,
            idle_only,
            batch_window,
            target,
            catch_up,
            cc,
            on_missing,
        } = spec;
        if let Some(when) = &when {
            validate_when(when).map_err(|error| anyhow::anyhow!("monitor {name:?}: {error}"))?;
        }
        // 过期时刻先算出来：参数来自模型与配置，时长再大也只能报错，不能让
        // `DateTime + Duration` 溢出 panic（release 是 panic = "abort"，会带走整个进程）。
        let expires_at = match expires {
            None => None,
            Some(duration) => Some(
                chrono::Duration::from_std(duration)
                    .ok()
                    .and_then(|duration| Local::now().checked_add_signed(duration))
                    .ok_or_else(|| anyhow::anyhow!("monitor {name:?}: expires is too far away"))?,
            ),
        };
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
                expires_at,
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

    /// 按名字取 monitor；不存在时报错。
    async fn get_or_bail(&self, name: &str) -> Result<Arc<Monitor>> {
        let monitor = {
            let monitors = self.monitors.read().await;
            monitors.get(name).cloned()
        };
        let Some(monitor) = monitor else {
            anyhow::bail!("monitor {name:?} does not exist");
        };
        Ok(monitor)
    }

    /// 手动触发一次。
    #[cfg(test)]
    pub async fn fire(&self, name: &str) -> Result<()> {
        let monitor = self.get_or_bail(name).await?;
        if monitor.cancelled.load(Ordering::SeqCst)
            || monitor
                .expires_at
                .is_some_and(|deadline| Local::now() > deadline)
        {
            anyhow::bail!("monitor {name:?} is no longer deliverable");
        }
        if !monitor.rate_limit_ok() {
            anyhow::bail!("monitor {name:?} is rate limited");
        }
        deliver_once(&monitor, &*self.sink, None, 0, true).await;
        Ok(())
    }

    /// 工具调用用：后台投递，避免在工具栈里同步跑回合、重入 turn_lock。
    pub async fn fire_async(&self, name: &str) -> Result<()> {
        let monitor = self.get_or_bail(name).await?;
        if !monitor.rate_limit_ok() {
            anyhow::bail!("monitor {name:?} is rate limited");
        }
        let sink = Arc::clone(&self.sink);
        tokio::spawn(async move {
            deliver_once(&monitor, &*sink, None, 0, true).await;
        });
        Ok(())
    }

    /// 以一条合成事件投递一次；用于 `backlog = "inbox"` 的首次补投。
    pub async fn deliver_text(&self, name: &str, text: String) -> Result<()> {
        let monitor = self.get_or_bail(name).await?;
        let event = MonitorEvent {
            text,
            ..MonitorEvent::default()
        };
        deliver_once(&monitor, &*self.sink, Some(&event), 1, false).await;
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
                // `add` 已校验表达式，这里不应发生。真发生了就停掉这个 monitor 并只通知一次：
                // 否则每个 QQ 事件都会投递一条系统通知（每条都是一个回合）。
                Err(error) => {
                    warn!(%error, monitor = %monitor.name, "monitor predicate failed");
                    if self.remove(&monitor.name).await {
                        self.sink
                            .deliver(monitor.system_delivery(
                                "internal",
                                format!(
                                    "system: monitor {:?} was removed: its `when` expression \
                                     failed ({error})",
                                    monitor.name
                                ),
                            ))
                            .await;
                    }
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
                deliver_once(monitor, &*self.sink, Some(event), 1, false).await;
                monitor.pending_deliveries.fetch_sub(1, Ordering::SeqCst);
                return;
            }
            let monitor = Arc::clone(monitor);
            let sink = Arc::clone(&self.sink);
            let event = event.clone();
            monitor.pending_deliveries.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                deliver_once(&monitor, &*sink, Some(&event), 1, false).await;
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
            monitor.flush_queued.store(false, Ordering::SeqCst);
            let texts = std::mem::take(&mut *monitor.batch.lock().expect("monitor lock"));
            if texts.is_empty() {
                monitor.pending_deliveries.fetch_sub(1, Ordering::SeqCst);
                return;
            }
            let event = MonitorEvent {
                text: texts.join("\n"),
                ..MonitorEvent::default()
            };
            deliver_once(&monitor, &*sink, Some(&event), texts.len() as u64, false).await;
            monitor.pending_deliveries.fetch_sub(1, Ordering::SeqCst);
        });
    }
}
async fn deliver_once(
    monitor: &Monitor,
    sink: &dyn MonitorSink,
    event: Option<&MonitorEvent>,
    count: u64,
    force: bool,
) {
    if monitor.cancelled.load(Ordering::SeqCst) {
        return;
    }
    if monitor
        .expires_at
        .is_some_and(|deadline| Local::now() > deadline)
    {
        monitor.cancelled.store(true, Ordering::SeqCst);
        sink.deliver(monitor.system_delivery(
            &monitor.source,
            format!(
                "system: monitor {:?} expired and was disabled",
                monitor.name
            ),
        ))
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

    if force {
        *monitor.last_delivery.lock().expect("monitor lock") = Some(std::time::Instant::now());
    } else if !monitor.rate_limit_ok() {
        return;
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
            sink.deliver(monitor.system_delivery(
                &monitor.source,
                format!(
                    "system: monitor {:?} reached its event limit and was disabled",
                    monitor.name
                ),
            ))
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
            // 配置校验已经调用过同一个解析器；这里再出错只可能是绕过了校验。
            let spec = Timer::parse(
                timer.cron.as_deref(),
                timer.every.as_deref(),
                timer.at.as_deref(),
                timer.jitter_secs,
            )
            .map_err(|error| anyhow::anyhow!("monitor {:?}: {error}", monitor.name))?;
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
            .add(MonitorSpec {
                name: monitor.name.clone(),
                source,
                timer,
                when: monitor.when.clone(),
                prompt: if monitor.deliver.as_ == crate::config::DeliverAs::Prompt {
                    Some(monitor.deliver.prompt.clone())
                } else {
                    None
                },
                as_prompt: monitor.deliver.as_ == crate::config::DeliverAs::Prompt,
                max_events: monitor.lifecycle.max_events,
                expires,
                one_shot: monitor.lifecycle.one_shot,
                min_interval: Duration::from_secs(monitor.lifecycle.min_interval_secs),
                idle_only: monitor.lifecycle.idle_only,
                batch_window: Duration::from_millis(monitor.deliver.batch_window_ms),
                target: if monitor.deliver.to == "main" {
                    None
                } else {
                    Some(monitor.deliver.to.clone())
                },
                catch_up: monitor.lifecycle.catch_up,
                cc: monitor.deliver.cc,
                on_missing: monitor.on_missing,
            })
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

    /// 模型可以写任意大的 `expires`；以前 `Local::now() + duration` 溢出会 panic，
    /// release 下 `panic = "abort"` 会直接带走守护进程。
    #[tokio::test]
    async fn absurd_expiry_is_an_error_not_a_panic() {
        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        let seconds = crate::config::parse_duration_secs("999999999999999s").unwrap();
        let error = manager
            .add(MonitorSpec {
                name: "forever".to_owned(),
                source: "qq".to_owned(),
                timer: None,
                when: None,
                prompt: None,
                as_prompt: false,
                max_events: 1,
                expires: Some(Duration::from_secs(seconds)),
                one_shot: false,
                min_interval: Duration::ZERO,
                idle_only: false,
                batch_window: Duration::ZERO,
                target: None,
                catch_up: CatchUp::Coalesce,
                cc: false,
                on_missing: crate::config::OnMissing::Queue,
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("too far"), "{error}");
        assert!(manager.list().await.is_empty());
    }

    #[tokio::test]
    async fn qq_monitor_delivers_matching_events_only() {
        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        manager.set_self_id(1);
        manager
            .add(MonitorSpec {
                name: "at-me".to_owned(),
                source: "qq".to_owned(),
                timer: None,
                when: Some("event.mentions_self".to_owned()),
                prompt: None,
                as_prompt: false,
                max_events: 100,
                expires: None,
                one_shot: false,
                min_interval: Duration::ZERO,
                idle_only: false,
                batch_window: Duration::ZERO,
                target: None,
                catch_up: CatchUp::Coalesce,
                cc: false,
                on_missing: crate::config::OnMissing::Queue,
            })
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
            .add(MonitorSpec {
                name: "cced".to_owned(),
                source: "qq".to_owned(),
                timer: None,
                when: None,
                prompt: None,
                as_prompt: false,
                max_events: 10,
                expires: None,
                one_shot: false,
                min_interval: Duration::ZERO,
                idle_only: false,
                batch_window: Duration::ZERO,
                target: Some("sa-1".to_owned()),
                catch_up: CatchUp::Coalesce,
                cc: true,
                on_missing: crate::config::OnMissing::Queue,
            })
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
            .add(MonitorSpec {
                name: "once".to_owned(),
                source: "qq".to_owned(),
                timer: None,
                when: None,
                prompt: None,
                as_prompt: false,
                max_events: 1,
                expires: None,
                one_shot: false,
                min_interval: Duration::ZERO,
                idle_only: false,
                batch_window: Duration::ZERO,
                target: None,
                catch_up: CatchUp::Coalesce,
                cc: false,
                on_missing: crate::config::OnMissing::Queue,
            })
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
            .add(MonitorSpec {
                name: "m".to_owned(),
                source: "qq".to_owned(),
                timer: None,
                when: None,
                prompt: None,
                as_prompt: false,
                max_events: 10,
                expires: None,
                one_shot: false,
                min_interval: Duration::ZERO,
                idle_only: false,
                batch_window: Duration::ZERO,
                target: None,
                catch_up: CatchUp::Coalesce,
                cc: false,
                on_missing: crate::config::OnMissing::Queue,
            })
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
            .add(MonitorSpec {
                name: "batched".to_owned(),
                source: "qq".to_owned(),
                timer: None,
                when: None,
                prompt: None,
                as_prompt: false,
                max_events: 10,
                expires: None,
                one_shot: false,
                min_interval: Duration::ZERO,
                idle_only: false,
                batch_window: Duration::from_millis(50),
                target: None,
                catch_up: CatchUp::Coalesce,
                cc: false,
                on_missing: crate::config::OnMissing::Queue,
            })
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
            .add(MonitorSpec {
                name: "idle".to_owned(),
                source: "qq".to_owned(),
                timer: None,
                when: None,
                prompt: None,
                as_prompt: false,
                max_events: 10,
                expires: None,
                one_shot: false,
                min_interval: Duration::ZERO,
                idle_only: true,
                batch_window: Duration::ZERO,
                target: None,
                catch_up: CatchUp::Coalesce,
                cc: false,
                on_missing: crate::config::OnMissing::Queue,
            })
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
            .add(MonitorSpec {
                name: "wake-on-owner".to_owned(),
                source: "qq".to_owned(),
                timer: None,
                when: Some("event.type == 'message'".to_owned()),
                prompt: None,
                as_prompt: false,
                max_events: 10,
                expires: None,
                one_shot: false,
                min_interval: Duration::ZERO,
                idle_only: false,
                batch_window: Duration::ZERO,
                target: None,
                catch_up: CatchUp::Coalesce,
                cc: false,
                on_missing: crate::config::OnMissing::Queue,
            })
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
            .add(MonitorSpec {
                name: "owner".to_owned(),
                source: "qq".to_owned(),
                timer: None,
                when: Some("event.sender == '322'".to_owned()),
                prompt: None,
                as_prompt: false,
                max_events: 10,
                expires: None,
                one_shot: false,
                min_interval: Duration::ZERO,
                idle_only: false,
                batch_window: Duration::ZERO,
                target: None,
                catch_up: CatchUp::Coalesce,
                cc: false,
                on_missing: crate::config::OnMissing::Queue,
            })
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

    /// 写错的表达式在创建时就被拒绝：短路求值会让后面的错项长期潜伏。
    #[tokio::test]
    async fn invalid_when_expressions_are_rejected_when_added() {
        assert!(validate_when("event.mentions_self and not event.text contains 'x'").is_ok());
        assert!(validate_when("event.type == 'notice' and event.bogus").is_err());
        assert!(validate_when("event.mentions_self and ").is_err());

        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        let error = manager
            .add(MonitorSpec {
                name: "broken".to_owned(),
                source: "qq".to_owned(),
                timer: None,
                when: Some("event.type == 'message' and event.sender = '1'".to_owned()),
                prompt: None,
                as_prompt: false,
                max_events: 10,
                expires: None,
                one_shot: false,
                min_interval: Duration::ZERO,
                idle_only: false,
                batch_window: Duration::ZERO,
                target: None,
                catch_up: CatchUp::Coalesce,
                cc: false,
                on_missing: crate::config::OnMissing::Queue,
            })
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("unsupported predicate"),
            "{error}"
        );
        assert!(manager.monitors.read().await.is_empty());
    }

    #[test]
    fn not_negates_any_supported_term() {
        let event = MonitorEvent {
            kind: "message".to_owned(),
            chat: "group:1".to_owned(),
            sender: Some(5),
            text: "hi".to_owned(),
            mentions_self: true,
            owner_intent: false,
            message: None,
        };
        assert_eq!(
            matches(
                Some("event.mentions_self and not event.sender == '7'"),
                &event
            ),
            Ok(true)
        );
        assert_eq!(matches(Some("not event.mentions_self"), &event), Ok(false));
        assert!(matches(Some("not event.bogus"), &event).is_err());
    }
}
