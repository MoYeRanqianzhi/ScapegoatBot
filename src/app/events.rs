//! QQ 事件循环与 monitor 投递端。
//!
//! `RunnerSink` 把 monitor 的投递变成一次注入回合；`spawn_qq_pipeline` /
//! `handle_qq_event` 是 NapCat 事件的入口，事件在这里过滤、入库并交给 monitor。

use std::sync::Arc;

use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::agent::queue::ChatQueue;
use crate::agent::runner::Runner;
use crate::monitor::{MonitorDelivery, MonitorSink};
use crate::qq::event::QqEvent;
use crate::qq::{LoginInfo, QqClient};
use crate::storage::Database;
use crate::storage::messages::MessageStore;

use super::AgentRuntime;

#[derive(Clone)]
pub(super) struct EventAudit {
    pub(super) db: Arc<Database>,
    pub(super) key: Arc<crate::storage::crypto::MasterKey>,
}

impl EventAudit {
    fn record(&self, kind: &str, chat: Option<&str>, actor: Option<u64>, raw: &serde_json::Value) {
        let Ok(plain) = serde_json::to_vec(raw) else {
            return;
        };
        let Ok(sealed) = self.key.seal("event-audit", b"", &plain) else {
            warn!(%kind, "cannot encrypt an event audit record");
            return;
        };
        if let Err(error) = self.db.record_event(kind, chat, actor, &sealed) {
            warn!(%error, %kind, "cannot store an event audit record");
        }
    }
}

/// monitor 的投递端：把事件变成一次注入回合。
#[derive(Default)]
pub(super) struct RunnerSink {
    pub(super) runner: std::sync::OnceLock<Arc<Runner>>,
    /// 用于 `idle_only`：有活跃回合时不投递。
    pub(super) queue: std::sync::OnceLock<Arc<ChatQueue>>,
    /// 绑定了子代理的 monitor 走这里。
    pub(super) hub: std::sync::OnceLock<Arc<crate::swarm::AgentHub>>,
}

#[async_trait::async_trait]
impl MonitorSink for RunnerSink {
    async fn deliver(&self, delivery: MonitorDelivery) {
        // 绑定目标：交给子代理，不进主上下文。
        if let Some(target) = &delivery.target {
            let Some(hub) = self.hub.get() else {
                warn!(monitor = %delivery.name, target, "subagent monitor delivered before the hub was ready");
                return;
            };
            match delivery.on_missing {
                crate::config::OnMissing::Drop => {
                    if hub.deliver_to(target, delivery.text.clone()).await.is_err() {
                        warn!(monitor = %delivery.name, target, "subagent monitor target is missing; dropped");
                    }
                }
                crate::config::OnMissing::Queue => {
                    for _ in 0..30 {
                        if hub.deliver_to(target, delivery.text.clone()).await.is_ok() {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                    warn!(monitor = %delivery.name, target, "subagent monitor target never appeared; dropped");
                }
                crate::config::OnMissing::Spawn => {
                    if let Err(error) = hub
                        .spawn(
                            Some("general"),
                            None,
                            format!(
                                "[monitor {}]
{}",
                                delivery.name, delivery.text
                            ),
                            0,
                            crate::transcript::TurnEvidence::monitor(),
                        )
                        .await
                    {
                        warn!(%error, monitor = %delivery.name, "cannot spawn a target for the monitor");
                    }
                }
            }
            return;
        }
        if delivery.name.starts_with("wake-on-")
            && let Some(message) = delivery.message.clone()
            && let Some(queue) = self.queue.get()
        {
            queue.ingest(message).await;
            return;
        }
        let Some(runner) = self.runner.get() else {
            warn!(monitor = %delivery.name, "monitor delivered before the runner was ready");
            return;
        };
        if let Err(error) = runner
            .run_event(
                &delivery.name,
                &delivery.source,
                &delivery.text,
                delivery.owner_intent,
            )
            .await
        {
            warn!(%error, monitor = %delivery.name, "monitor delivery failed");
        }
    }

    async fn is_idle(&self) -> bool {
        if self.runner.get().is_some_and(|runner| runner.is_busy()) {
            return false;
        }
        match self.queue.get() {
            Some(queue) => queue.is_idle().await,
            None => true,
        }
    }
}

/// NapCat 事件循环。
pub(super) fn spawn_qq_pipeline(
    agent: Arc<AgentRuntime>,
    client: Arc<QqClient>,
    info: LoginInfo,
    messages: Arc<MessageStore>,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut subscription = client.subscribe();
        info!(qq = info.user_id, "listening for QQ events");
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                event = subscription.recv() => {
                    match event {
                        Some(data) => {
                            handle_qq_event(
                                &agent,
                                &client,
                                &messages,
                                &data.name,
                                &data.data,
                                info.user_id,
                            )
                            .await;
                        }
                        None => {
                            warn!("NapCat event subscription ended");
                            break;
                        }
                    }
                }
            }
        }
    });
}

async fn handle_qq_event(
    agent: &AgentRuntime,
    client: &QqClient,
    messages: &MessageStore,
    name: &str,
    data: &serde_json::Value,
    _self_id: u64,
) {
    let event = crate::qq::event::normalize(name, data);

    // 主人/通知账号在私聊里回复 nonce：批准对应 gate 请求，不进入 agent 回合。
    if let QqEvent::Message(message) = &event
        && !message.is_self
        && !message.backfilled
        && message.chat == crate::qq::event::ChatId::Private(agent.self_id)
        && message
            .sender
            .qq
            .is_some_and(|qq| qq == agent.owner_qq || qq == agent.master_qq)
        && let Some(approved) = agent.approvals.approve_by_nonce(&message.plain_text())
    {
        let id = approved["approved"].as_str().unwrap_or_default();
        let action = approved["action"].as_str().unwrap_or_default();
        let ack = format!("gate approval {id} for {action} accepted; retry the action within 120s");
        if let Some(qq) = message.sender.qq
            && let Err(error) = client
                .call(
                    "send_private_msg",
                    serde_json::json!({"user_id": qq, "message": ack}),
                )
                .await
        {
            warn!(%error, "cannot acknowledge a gate nonce");
        }
        return;
    }
    // 准入过滤必须在 monitor 之前：被拒消息既不应入库，也不应通过 monitor
    // 进入上下文（否则过滤只剩“不写 JSONL”，挡不住注入）。
    if let QqEvent::Message(message) = &event
        && !agent.filter_and_count(message)
    {
        debug!(chat = %message.chat, "message rejected by the admission filter");
        return;
    }
    if !matches!(&event, QqEvent::Message(_)) {
        agent.monitors.on_qq_event(&event).await;
    }
    match event {
        QqEvent::Message(message) => match messages.append(&message) {
            Ok(true) => {
                if !message.is_self && !message.backfilled {
                    agent.monitors.on_qq_event(&QqEvent::Message(message)).await;
                }
            }
            Ok(false) => {}
            Err(error) => warn!(%error, "cannot store an inbound message"),
        },
        QqEvent::Notice(notice) => {
            agent.audit.record(
                &format!("notice:{}", notice.kind),
                notice.chat.as_ref().map(|chat| chat.to_string()).as_deref(),
                notice.actor,
                &notice.raw,
            );
            if let Some(message) = notice.to_inbox_message()
                && let Err(error) = messages.append(&message)
            {
                warn!(%error, kind = %notice.kind, "cannot store a QQ notice");
            }
            debug!(kind = %notice.kind, "QQ notice");
        }
        QqEvent::Request(request) => {
            agent.audit.record(
                &format!("request:{}", request.kind),
                request
                    .group
                    .map(|group| format!("group:{group}"))
                    .as_deref(),
                request.actor,
                &request.raw,
            );
            if let Some(message) = request.to_inbox_message()
                && let Err(error) = messages.append(&message)
            {
                warn!(%error, kind = %request.kind, "cannot store a QQ request");
            }
            debug!(kind = %request.kind, actor = ?request.actor, "QQ request");
        }
        QqEvent::Meta { kind, data } => {
            let heartbeat = data
                .get("meta_event_type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind == "heartbeat");
            if !heartbeat {
                agent.audit.record(&kind, None, None, &data);
            }
            debug!(kind, "meta event");
        }
        QqEvent::Connection { connected, reason } => {
            debug!(connected, %reason, "connection state");
        }
        QqEvent::Unknown { name, data } => {
            agent.audit.record(&name, None, None, &data);
            if let Some(message) = crate::qq::event::unknown_to_inbox_message(&name, &data)
                && let Err(error) = messages.append(&message)
            {
                warn!(%error, %name, "cannot store an unknown QQ event");
            }
            debug!(name, "unknown QQ event");
        }
    }
}
