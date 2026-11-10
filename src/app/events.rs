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
                            format!("[monitor {}]\n{}", delivery.name, delivery.text),
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
        // `deliver.as = "chat"`（内置唤醒预设就是这种）：原始消息交给 ChatQueue，按普通会话
        // 回合处理；没有消息（例如手动 `fire`）时退回 monitor 回合。
        if delivery.deliver_as == crate::config::DeliverAs::Chat
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
    // 断线后一直重连：SDK 自己的重连有次数上限，之后由它接手（见 `QqClient::keep_connected`）。
    client.keep_connected(shutdown.clone());
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
) {
    let mut event = crate::qq::event::normalize(name, data);

    // 本账号发出的消息：只有本进程发出的才是 agent 自己的话（`self`）。
    // 其余的是有人在这个账号上手打的——stand_in 下就是主人——它们和别人的消息
    // 一样进 inbox，由 monitor 决定是否唤醒（见 `crate::qq::ledger`）。
    if let QqEvent::Message(message) = &mut event
        && message.is_self
        && agent.self_id != 0
        && !client.ledger().claim(message).await
    {
        message.mark_typed_by_hand(agent.self_id);
    }

    // 主人/通知账号在私聊里回复确认码：批准对应 gate 请求，不进入 agent 回合，
    // 也不入库（确认码不应留在模型能读到的归档里）。
    if let QqEvent::Message(message) = &event
        && let Some(sender) =
            approval_reply_sender(message, agent.self_id, agent.owner_qq, agent.master_qq)
        && let Some(approved) = agent.approvals.approve_by_nonce(&message.plain_text())
    {
        let id = approved["approved"].as_str().unwrap_or_default();
        let action = approved["action"].as_str().unwrap_or_default();
        let ack = format!(
            "Approved {id} ({action}). The agent may use it once within the next 120s; \
             ask it to retry if it does not."
        );
        if let Err(error) = client
            .call(
                "send_private_msg",
                serde_json::json!({"user_id": sender, "message": ack}),
            )
            .await
        {
            warn!(%error, "cannot acknowledge a gate approval");
        }
        return;
    }
    // 准入过滤必须在审计、入库与 monitor 之前：被拒事件既不应入库，也不应通过
    // monitor 或 `/system/events` 进入上下文（否则过滤只剩“不写 JSONL”，挡不住注入）。
    if !agent.admit(&event) {
        debug!("event rejected by the admission filter");
        return;
    }
    if !matches!(&event, QqEvent::Message(_)) {
        agent.monitors.on_qq_event(&event).await;
    }
    match event {
        QqEvent::Message(message) => match messages.append(&message) {
            Ok(true) => {
                if !message.is_self {
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

/// 这条消息能否作为 gate 确认码的回复；能则返回回复者的 QQ。
///
/// 只接受“另一个账号发来的私聊”，且该账号是 owner 或 master：
/// - 私聊会话对象必须就是发送者（群消息、转发、引用都不算）；
/// - 本账号自己发出的消息（`is_self`，包括 stand_in 下主人在手机上手动发的）
///   一律不算：它们与模型通过 API 发出的消息无法区分，而模型可能从归档里
///   读到确认码，接受它们就等于允许模型自我批准；
/// - 发送者等于本账号的消息同理。
fn approval_reply_sender(
    message: &crate::qq::event::QqMessage,
    account_qq: u64,
    owner_qq: u64,
    master_qq: u64,
) -> Option<u64> {
    if message.is_self {
        return None;
    }
    let crate::qq::event::ChatId::Private(peer) = message.chat else {
        return None;
    };
    let sender = message.sender.qq?;
    if sender != peer || sender == account_qq || sender == 0 {
        return None;
    }
    (sender == owner_qq || sender == master_qq).then_some(sender)
}

#[cfg(test)]
mod tests {
    use super::approval_reply_sender;
    use crate::qq::event::{QqEvent, normalize};
    use serde_json::json;

    /// 用 NapCat 的事件形状构造一条消息。
    fn message(data: serde_json::Value) -> crate::qq::event::QqMessage {
        match normalize("message", &data) {
            QqEvent::Message(message) => message,
            other => panic!("expected a message, got {other:?}"),
        }
    }

    /// companion 模式：主人（20000）从自己的账号私聊 bot（10000）回复确认码。
    #[test]
    fn owner_private_reply_from_another_account_is_accepted() {
        let reply = message(json!({
            "post_type": "message", "message_type": "private", "self_id": 10000,
            "user_id": 20000, "message_id": 1, "time": 1793304000,
            "message": [{"type": "text", "data": {"text": "123456"}}]
        }));
        assert_eq!(
            approval_reply_sender(&reply, 10000, 20000, 10000),
            Some(20000)
        );
        // 独立的通知账号（30000）同样可以。
        let reply = message(json!({
            "post_type": "message", "message_type": "private", "self_id": 10000,
            "user_id": 30000, "message_id": 2, "time": 1793304000,
            "message": [{"type": "text", "data": {"text": "123456"}}]
        }));
        assert_eq!(
            approval_reply_sender(&reply, 10000, 10000, 30000),
            Some(30000)
        );
    }

    /// stand_in 默认配置：owner = master = 本账号。主人在手机上发的消息是
    /// `message_sent` 回声，与模型发出的消息无法区分，不能用来批准。
    #[test]
    fn messages_from_the_account_itself_never_approve() {
        let echo = message(json!({
            "post_type": "message_sent", "message_type": "private", "self_id": 10000,
            "user_id": 10000, "target_id": 10000, "message_id": 3, "time": 1793304000,
            "message": [{"type": "text", "data": {"text": "123456"}}]
        }));
        assert_eq!(approval_reply_sender(&echo, 10000, 10000, 10000), None);
    }

    /// 群里、陌生人私聊发确认码都不算。
    #[test]
    fn group_messages_and_strangers_never_approve() {
        let group = message(json!({
            "post_type": "message", "message_type": "group", "self_id": 10000,
            "group_id": 42, "user_id": 20000, "message_id": 4, "time": 1793304000,
            "message": [{"type": "text", "data": {"text": "123456"}}]
        }));
        assert_eq!(approval_reply_sender(&group, 10000, 20000, 20000), None);
        let stranger = message(json!({
            "post_type": "message", "message_type": "private", "self_id": 10000,
            "user_id": 40000, "message_id": 5, "time": 1793304000,
            "message": [{"type": "text", "data": {"text": "123456"}}]
        }));
        assert_eq!(approval_reply_sender(&stranger, 10000, 20000, 20000), None);
    }
}
