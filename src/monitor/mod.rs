//! 监视器：事件流到消息的订阅。
//!
//! 设计依据：设计 §5.11。
//!
//! 结构：`{source, when?, deliver, lifecycle?}`。
//! - source：`qq` / `timer` / `internal`（运行时状态变化，见 [`internal`]）；`stream`
//!   （外部 WebSocket / SSE，见 `stream.rs`）只允许写在配置文件里；
//! - when：谓词子集（`event.mentions_self`、`event.type == 'message'`、
//!   `event.chat == 'group:123'`、`event.sender == '10001'` / `!=`、`event.text contains 'x'`，
//!   前缀 `not` 取反，用 `and` 组合）；
//! - deliver：投递给主代理或子代理，以 `event`、`prompt` 或 `chat`（把 QQ 消息交给
//!   ChatQueue）形式；
//! - lifecycle：`max_events`、`expires`、`one_shot`、`min_interval_secs`。
//!
//! 运行时创建的 monitor 只在内存里，重启即消失；配置文件里的才是长期常驻。
//! 静默不等于正常：谓词求值失败、到期、达到上限都会产生一条系统消息。
//!
//! 模块：`predicate`（`when`）、`spec`（配置 / 工具参数 → [`MonitorSpec`]）、`delivery`
//! （批处理与投递）、`timer` / `stream` / `internal`（三种非 QQ 源）。

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

use crate::config::{CatchUp, DeliverAs};
use crate::qq::event::QqEvent;
use crate::storage::Database;

mod delivery;
pub mod internal;
mod predicate;
mod spec;
mod stream;
mod timer;

use delivery::{deliver_once, enqueue};
pub use internal::{InternalBus, InternalEvent, InternalKind};
pub use predicate::{matches, validate_when};
pub use spec::{MonitorOrigin, MonitorSpec, load_config_monitors, runtime_config};
pub use timer::Timer;
use timer::spawn_timer;

/// 投递内容。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorDelivery {
    pub name: String,
    pub source: String,
    /// `None` = 主代理；`Some("sa-…")` = 绑定的子代理实例；`Some("agent:<name>")` = 按定义名。
    pub target: Option<String>,
    /// 投递形式；`Chat` 时 sink 把 `message` 交给 ChatQueue，按普通会话回合处理。
    pub deliver_as: DeliverAs,
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

/// 运行时 monitor。
struct Monitor {
    name: String,
    source: String,
    /// 投递目标；`None` 表示主代理。
    target: Option<String>,
    when: Option<String>,
    prompt: Option<String>,
    deliver_as: DeliverAs,
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
    /// 从哪里来（配置 / 运行时 / 预设）。
    origin: MonitorOrigin,
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
            deliver_as: DeliverAs::Event,
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

    /// 新增一个 monitor；同名已存在时报错。同名的预设删除标记在这里清除：用 `Monitor add`
    /// 重新添加一个删掉的预设名，下次启动预设会回来。
    pub async fn add(&self, spec: MonitorSpec) -> Result<()> {
        let MonitorSpec {
            name,
            source,
            timer,
            when,
            prompt,
            deliver_as,
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
            origin,
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
                deliver_as,
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
                origin,
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

    /// 删除并停止投递；删除内置预设会写进 `meta`，重启不再安装。
    pub async fn remove(&self, name: &str) -> bool {
        let removed = self.monitors.write().await.remove(name);
        if let Some(monitor) = removed {
            monitor.cancelled.store(true, Ordering::SeqCst);
            if monitor.origin == MonitorOrigin::Preset
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
                    "origin": monitor.origin.as_str(),
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
        self.dispatch("qq", &view).await;
    }

    /// 订阅内部事件总线：之后发出的事件交给 `internal:<kind>` 的 monitor。
    ///
    /// 监听任务只持有弱引用，管理器释放后自行退出；积压过多被覆盖时记日志后继续。
    pub fn listen(self: &Arc<Self>, bus: &InternalBus) {
        let mut receiver = bus.subscribe();
        let manager = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let event = match receiver.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(skipped, "internal monitor events were dropped");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let Some(manager) = manager.upgrade() else {
                    break;
                };
                manager.on_internal(&event).await;
            }
        });
    }

    /// 喂入一个内部事件。
    pub async fn on_internal(&self, event: &InternalEvent) {
        let view = MonitorEvent {
            kind: event.event_type(),
            text: event.text.clone(),
            ..MonitorEvent::default()
        };
        self.dispatch(&event.kind.source(), &view).await;
    }

    /// 把事件交给 `source` 相同的每个 monitor；`when` 不满足的跳过。
    async fn dispatch(&self, source: &str, view: &MonitorEvent) {
        let monitors: Vec<Arc<Monitor>> = {
            let monitors = self.monitors.read().await;
            monitors
                .values()
                .filter(|monitor| monitor.source == source)
                .cloned()
                .collect()
        };
        for monitor in monitors {
            match matches(monitor.when.as_deref(), view) {
                Ok(true) => enqueue(&monitor, &self.sink, view).await,
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

    /// 为配置里的 `stream` monitor 建立连接；只由 [`load_config_monitors`] 调用。
    async fn attach_stream(&self, name: &str, url: String, kind: crate::config::StreamKind) {
        let monitor = self.monitors.read().await.get(name).cloned();
        if let Some(monitor) = monitor {
            stream::spawn_stream(monitor, Arc::clone(&self.sink), url, kind);
        }
    }
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

    /// 等到收到 `count` 条投递（最多 5 秒），返回当时的全部投递。
    ///
    /// 投递发生在后台任务里，什么时候轮到它跑取决于机器；固定睡一段时间再断言的写法在慢的
    /// CI 机器上会偶发失败。
    async fn wait_for_deliveries(sink: &RecordingSink, count: usize) -> Vec<MonitorDelivery> {
        for _ in 0..500 {
            if sink.deliveries.lock().unwrap().len() >= count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        sink.deliveries.lock().unwrap().clone()
    }

    /// 一个默认的运行时 monitor：逐个投递、10 次上限、不过期。
    fn spec(name: &str, source: &str) -> MonitorSpec {
        MonitorSpec {
            name: name.to_owned(),
            source: source.to_owned(),
            timer: None,
            when: None,
            prompt: None,
            deliver_as: DeliverAs::Event,
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
            origin: MonitorOrigin::Runtime,
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

    #[tokio::test]
    async fn internal_events_reach_only_their_kind() {
        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        let bus = InternalBus::new();
        manager.listen(&bus);
        for (name, kind, when) in [
            (
                "on-hold",
                InternalKind::Security,
                Some("event.type == 'security:hold'"),
            ),
            ("on-budget", InternalKind::Budget, None),
        ] {
            manager
                .add(MonitorSpec {
                    when: when.map(str::to_owned),
                    max_events: 100,
                    ..spec(name, &kind.source())
                })
                .await
                .unwrap();
        }
        bus.emit(InternalKind::Security, "released", "not matched by when");
        bus.emit(
            InternalKind::Security,
            "hold",
            "security hold entered: test",
        );
        bus.emit(InternalKind::Storage, "disk", "nobody listens to storage");
        bus.emit(
            InternalKind::Budget,
            "exhausted",
            "daily token budget exhausted",
        );
        // 监听任务异步投递；等到两条都到达。
        let deliveries = wait_for_deliveries(&sink, 2).await;
        let names: Vec<&str> = deliveries
            .iter()
            .map(|delivery| delivery.name.as_str())
            .collect();
        assert_eq!(names, vec!["on-hold", "on-budget"], "{deliveries:?}");
        assert!(deliveries[0].text.contains("security hold entered"));
        assert!(deliveries.iter().all(|delivery| !delivery.owner_intent));
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
                max_events: 1,
                expires: Some(Duration::from_secs(seconds)),
                ..spec("forever", "qq")
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
                when: Some("event.mentions_self".to_owned()),
                max_events: 100,
                ..spec("at-me", "qq")
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
                target: Some("sa-1".to_owned()),
                cc: true,
                ..spec("cced", "qq")
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
                max_events: 1,
                ..spec("once", "qq")
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
            .add(MonitorSpec { ..spec("m", "qq") })
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
                batch_window: Duration::from_millis(50),
                ..spec("batched", "qq")
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
        enqueue(&monitor, &manager.sink, &first).await;
        enqueue(&monitor, &manager.sink, &second).await;
        // 窗口未到时不应投递。
        assert!(sink.deliveries.lock().unwrap().is_empty());

        let deliveries = wait_for_deliveries(&sink, 1).await;
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
                idle_only: true,
                ..spec("idle", "qq")
            })
            .await
            .unwrap();

        let event = event("private:10001", "hello", false);
        let monitor = manager.monitors.read().await.get("idle").cloned().unwrap();
        enqueue(&monitor, &manager.sink, &event).await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(sink.deliveries.lock().unwrap().is_empty());

        sink.idle.store(true, Ordering::SeqCst);
        assert_eq!(wait_for_deliveries(&sink, 1).await.len(), 1);
    }

    #[tokio::test]
    async fn removing_a_wake_preset_is_remembered() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        manager.set_db(Arc::clone(&db));
        manager
            .add(MonitorSpec {
                when: Some("event.type == 'message'".to_owned()),
                origin: MonitorOrigin::Preset,
                ..spec("wake-on-owner", "qq")
            })
            .await
            .unwrap();
        assert_eq!(manager.list().await[0]["origin"], "preset");
        assert!(manager.remove("wake-on-owner").await);
        assert_eq!(
            db.meta_get("monitor.removed.wake-on-owner")
                .unwrap()
                .as_deref(),
            Some(&b"1"[..])
        );
    }

    /// 以前删除是否被记住、投递走哪条路都看名字是否以 `wake-on-` 开头；现在看来源与
    /// `deliver_as`，名字只是名字。
    #[tokio::test]
    async fn the_name_prefix_no_longer_changes_behaviour() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        manager.set_db(Arc::clone(&db));
        manager.add(spec("wake-on-anything", "qq")).await.unwrap();
        manager.fire("wake-on-anything").await.unwrap();
        assert_eq!(
            sink.deliveries.lock().unwrap()[0].deliver_as,
            DeliverAs::Event
        );
        assert!(manager.remove("wake-on-anything").await);
        assert_eq!(
            db.meta_get("monitor.removed.wake-on-anything")
                .unwrap()
                .as_deref(),
            Some(&b"0"[..])
        );
    }

    /// `as = "chat"` 不走批处理：每条原始消息都要单独交给 ChatQueue。
    #[tokio::test]
    async fn chat_delivery_keeps_each_message() {
        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        manager.set_self_id(1);
        manager
            .add(MonitorSpec {
                deliver_as: DeliverAs::Chat,
                batch_window: Duration::from_millis(200),
                ..spec("chatty", "qq")
            })
            .await
            .unwrap();
        for id in [3, 4] {
            let raw = serde_json::json!({
                "post_type": "message",
                "message_type": "group",
                "self_id": 1,
                "group_id": 123,
                "user_id": 2,
                "message_id": id,
                "time": 1793304000,
                "sender": {"nickname": "n"},
                "message": "hi"
            });
            let event = crate::qq::event::normalize("message.group.normal", &raw);
            manager.on_qq_event(&event).await;
        }
        let deliveries = sink.deliveries.lock().unwrap();
        let ids: Vec<i64> = deliveries
            .iter()
            .map(|delivery| {
                assert_eq!(delivery.deliver_as, DeliverAs::Chat);
                delivery.message.as_ref().unwrap().message_id
            })
            .collect();
        assert_eq!(ids, vec![3, 4]);
    }

    #[tokio::test]
    async fn owner_message_delivery_carries_owner_intent() {
        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        manager.set_owner_qq(322);
        manager
            .add(MonitorSpec {
                when: Some("event.sender == '322'".to_owned()),
                ..spec("owner", "qq")
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
        let sink = Arc::new(RecordingSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        let error = manager
            .add(MonitorSpec {
                when: Some("event.type == 'message' and event.sender = '1'".to_owned()),
                ..spec("broken", "qq")
            })
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("unsupported predicate"),
            "{error}"
        );
        assert!(manager.monitors.read().await.is_empty());
    }
}
