//! 聊天队列：去抖、合并、上限与并发。
//!
//! 设计依据：设计 §3.4 / §4 / §9.2 `[queue]`。
//!
//! 语义：
//! - 同一 chat 的连发在 `debounce_ms` 窗口内合并成一个回合；不同 chat 各自计时。
//! - 回合运行期间到达的消息：`steer` 直接进入 steering 队列；`followup` / `collect`
//!   留在下一条待处理里，回合结束后再开新回合。
//! - 每 chat 同时限制消息条数与批次上限；溢出按 `summarize | old | new` 处理，
//!   `summarize` 只保留一个“丢弃了多少条”的计数，由回合提示语转述。
//! - 全局并发由 `max_concurrent` 控制。
//!
//! 本模块不依赖 agent 实现：真正的回合执行由 [`TurnHandler`] 提供。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use tokio::sync::{Mutex, Semaphore, mpsc};
use tracing::{debug, warn};

use crate::agent::message::AgentMessage;
use crate::config::{DropPolicy, QueueConfig, QueueMode};
use crate::qq::event::{ChatId, QqMessage};
use crate::qq::format::render_inbound;

/// 一次回合的输入。
#[derive(Debug, Clone)]
pub struct TurnInput {
    pub chat: ChatId,
    pub messages: Vec<QqMessage>,
    /// `summarize` 丢弃的消息条数；>0 时提示词里要说明。
    pub dropped: u64,
}

/// 回合执行器。
#[async_trait::async_trait]
pub trait TurnHandler: Send + Sync {
    /// 执行一个回合；`steering` 在回合运行期间持续接收同 chat 的新消息。
    async fn handle(
        &self,
        input: TurnInput,
        steering: mpsc::UnboundedReceiver<AgentMessage>,
    ) -> Result<()>;
}

/// 每个 chat 的状态。
#[derive(Default)]
struct ChatState {
    pending: VecDeque<QqMessage>,
    dropped: u64,
    /// 当前回合的 steering 发送端；`None` 表示空闲。
    active: Option<mpsc::UnboundedSender<AgentMessage>>,
    /// 去抖令牌：只有最新一次计时有效。
    token: u64,
    /// 已排队（含运行中）的批次数。
    pending_batches: usize,
}

/// 聊天队列。
pub struct ChatQueue {
    /// `/config/queue` 写入后可原地替换；信号量上限仍需重启才变化。
    config: std::sync::RwLock<QueueConfig>,
    handler: Arc<dyn TurnHandler>,
    states: Mutex<HashMap<ChatId, ChatState>>,
    semaphore: Arc<Semaphore>,
    counter: AtomicU64,
}

impl ChatQueue {
    pub fn new(config: QueueConfig, handler: Arc<dyn TurnHandler>) -> Arc<Self> {
        let max = config.max_concurrent.max(1);
        Arc::new(Self {
            config: std::sync::RwLock::new(config),
            handler,
            states: Mutex::new(HashMap::new()),
            semaphore: Arc::new(Semaphore::new(max)),
            counter: AtomicU64::new(0),
        })
    }

    /// 热更新队列参数；`max_concurrent` 的信号量不会缩小，需重启生效。
    pub fn reload_config(&self, config: QueueConfig) {
        *self.config.write().expect("queue config lock poisoned") = config;
    }

    /// 当前配置快照。
    fn config(&self) -> QueueConfig {
        self.config
            .read()
            .expect("queue config lock poisoned")
            .clone()
    }

    /// 注入一条已通过过滤器的消息。
    pub async fn ingest(self: &Arc<Self>, message: QqMessage) {
        let chat = message.chat;
        let config = self.config();
        let mut states = self.states.lock().await;
        let state = states.entry(chat).or_default();

        // 回合进行中：steer 模式直接注入。
        if let Some(steering) = &state.active
            && config.mode == QueueMode::Steer
        {
            let _ = steering.send(AgentMessage::user(render_inbound(&message)));
            return;
        }

        self.push_with_caps(state, message);
        state.token = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let token = state.token;

        // 如果已有回合在跑，等它结束；否则按去抖窗口触发。
        // 空闲时总是用最新 token 重新计时：旧计时器会因 token 不匹配而作废，
        // 但它持有的预约由新计时器继承，所以只在没有预约时补一次计数。
        if state.active.is_none() {
            if state.pending_batches == 0 {
                state.pending_batches = 1;
            }
            drop(states);
            self.schedule(chat, token, config.debounce_ms);
        }
    }

    /// 按上限压入待处理队列。
    fn push_with_caps(&self, state: &mut ChatState, message: QqMessage) {
        let config = self.config();
        if state.pending.len() >= config.cap_messages {
            match config.drop {
                DropPolicy::New => {
                    debug!(chat = %message.chat, "queue full; dropping the newest message");
                    return;
                }
                DropPolicy::Old => {
                    state.pending.pop_front();
                    debug!(chat = %message.chat, "queue full; dropped the oldest message");
                }
                DropPolicy::Summarize => {
                    state.pending.pop_front();
                    state.dropped += 1;
                    debug!(chat = %message.chat, "queue full; dropped the oldest message and kept a count");
                }
            }
        }
        state.pending.push_back(message);
    }

    /// 启动去抖计时。
    fn schedule(self: &Arc<Self>, chat: ChatId, token: u64, delay_ms: u64) {
        let queue = Arc::clone(self);
        tokio::spawn(async move {
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            queue.fire(chat, token).await;
        });
    }

    /// 去抖窗口结束：取出一批并启动回合。
    async fn fire(self: &Arc<Self>, chat: ChatId, token: u64) {
        let config = self.config();
        let (batch, steering_tx, steering_rx) = {
            let mut states = self.states.lock().await;
            let Some(state) = states.get_mut(&chat) else {
                return;
            };
            // 更新的消息到来了：这次计时作废；预约由最新那次计时持有。
            if state.token != token {
                return;
            }
            if state.pending.is_empty() {
                state.pending_batches = state.pending_batches.saturating_sub(1);
                return;
            }
            if state.pending_batches > config.cap_batches {
                // 批次上限：当前实现每个 chat 同时只有一个排队批次，超出说明
                // 计数异常；丢掉最旧的消息并记账，避免继续堆积。
                state.pending.pop_front();
                state.dropped += 1;
            }

            let messages: Vec<QqMessage> = state.pending.drain(..).collect();
            let dropped = std::mem::take(&mut state.dropped);
            let (steering_tx, steering_rx) = mpsc::unbounded_channel();
            state.active = Some(steering_tx.clone());
            (
                TurnInput {
                    chat,
                    messages,
                    dropped,
                },
                steering_tx,
                steering_rx,
            )
        };

        // 全局并发可能在锁外等待很久：绝不能持 `states` 锁等信号量，
        // 否则一个 chat 打满并发就会拖住所有 chat 的 ingest。
        let permit = match Arc::clone(&self.semaphore).acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                let mut states = self.states.lock().await;
                if let Some(state) = states.get_mut(&chat) {
                    state.active = None;
                    state.pending_batches = state.pending_batches.saturating_sub(1);
                }
                return;
            }
        };
        let handler = Arc::clone(&self.handler);
        let queue = Arc::clone(self);
        tokio::spawn(async move {
            let input_for_log = batch.clone();
            match handler.handle(batch, steering_rx).await {
                Ok(()) => {
                    debug!(chat = %chat, messages = input_for_log.messages.len(), "turn finished")
                }
                Err(error) => warn!(%error, chat = %chat, "turn failed"),
            }
            drop(permit);
            drop(steering_tx);

            // 回合结束：清理状态，若期间还有消息则再开一轮。
            let mut states = queue.states.lock().await;
            if let Some(state) = states.get_mut(&chat) {
                state.active = None;
                state.pending_batches = state.pending_batches.saturating_sub(1);
                if !state.pending.is_empty() {
                    state.token = queue.counter.fetch_add(1, Ordering::Relaxed) + 1;
                    let token = state.token;
                    // 预约下一批；`fire` 接手后不再重复计数。
                    state.pending_batches += 1;
                    drop(states);
                    // follow-up 不再等待完整去抖窗口，只留一个很小的合并窗口。
                    queue.schedule(chat, token, 50);
                }
            }
        });
    }

    /// 当前待处理消息数（测试与 status 使用）。
    pub async fn pending(&self, chat: ChatId) -> usize {
        self.states
            .lock()
            .await
            .get(&chat)
            .map(|state| state.pending.len())
            .unwrap_or(0)
    }

    /// 正在运行的回合数。
    pub async fn active_turns(&self) -> usize {
        self.states
            .lock()
            .await
            .values()
            .filter(|state| state.active.is_some())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;
    use crate::qq::event::Sender;
    use chrono::Utc;
    use serde_json::Value;

    #[derive(Default)]
    struct RecordingHandler {
        calls: StdMutex<Vec<TurnInput>>,
        steer: StdMutex<Vec<AgentMessage>>,
        /// 每个回合人为拖延的时间。
        delay_ms: u64,
    }

    #[async_trait::async_trait]
    impl TurnHandler for RecordingHandler {
        async fn handle(
            &self,
            input: TurnInput,
            mut steering: mpsc::UnboundedReceiver<AgentMessage>,
        ) -> Result<()> {
            self.calls.lock().unwrap().push(input.clone());
            if self.delay_ms > 0 {
                let deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_millis(self.delay_ms);
                loop {
                    tokio::select! {
                        message = steering.recv() => match message {
                            Some(message) => self.steer.lock().unwrap().push(message),
                            None => break,
                        },
                        _ = tokio::time::sleep_until(deadline) => break,
                    }
                }
            } else {
                while let Ok(message) = steering.try_recv() {
                    self.steer.lock().unwrap().push(message);
                }
            }
            Ok(())
        }
    }

    fn msg(chat: ChatId, id: i64) -> QqMessage {
        QqMessage {
            message_id: id,
            chat,
            sender: Sender {
                qq: Some(1),
                nickname: "n".to_owned(),
                card: String::new(),
                role: "member".to_owned(),
            },
            time: Utc::now(),
            segments: vec![crate::qq::event::Segment::Text {
                text: format!("m{id}"),
            }],
            reply_to: None,
            is_self: false,
            backfilled: false,
            raw: Value::Null,
        }
    }

    fn config() -> QueueConfig {
        QueueConfig {
            mode: QueueMode::Steer,
            max_concurrent: 2,
            cap_messages: 3,
            cap_batches: 20,
            drop: DropPolicy::Summarize,
            debounce_ms: 100,
        }
    }

    #[tokio::test]
    async fn reload_config_updates_the_snapshot() {
        let handler = Arc::new(RecordingHandler::default());
        let queue = ChatQueue::new(config(), handler);
        let mut updated = config();
        updated.cap_messages = 99;
        updated.debounce_ms = 7;
        queue.reload_config(updated);
        let snapshot = queue.config();
        assert_eq!(snapshot.cap_messages, 99);
        assert_eq!(snapshot.debounce_ms, 7);
    }

    #[tokio::test]
    async fn debounce_coalesces_bursts() {
        let handler = Arc::new(RecordingHandler::default());
        let mut cfg = config();
        cfg.debounce_ms = 40;
        let queue = ChatQueue::new(cfg, handler.clone());
        let chat = ChatId::Group(1);
        for id in 1..=3 {
            queue.ingest(msg(chat, id)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let calls = handler.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].messages.len(), 3);
    }

    #[tokio::test]
    async fn different_chats_get_independent_turns() {
        let handler = Arc::new(RecordingHandler::default());
        let mut cfg = config();
        cfg.debounce_ms = 40;
        let queue = ChatQueue::new(cfg, handler.clone());
        queue.ingest(msg(ChatId::Group(1), 1)).await;
        queue.ingest(msg(ChatId::Private(2), 1)).await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let calls = handler.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        let chats: Vec<ChatId> = calls.iter().map(|call| call.chat).collect();
        assert!(chats.contains(&ChatId::Group(1)));
        assert!(chats.contains(&ChatId::Private(2)));
    }

    #[tokio::test]
    async fn caps_drop_and_count_for_summarize() {
        let handler = Arc::new(RecordingHandler::default());
        let mut cfg = config();
        cfg.debounce_ms = 200;
        let queue = ChatQueue::new(cfg, handler.clone());
        let chat = ChatId::Group(1);
        for id in 1..=6 {
            queue.ingest(msg(chat, id)).await;
        }
        assert_eq!(queue.pending(chat).await, 3);

        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let calls = handler.calls.lock().unwrap();
        assert_eq!(calls[0].messages.len(), 3);
        assert_eq!(calls[0].dropped, 3);
    }

    #[tokio::test]
    async fn follow_up_does_not_leak_the_batch_reservation() {
        let handler = Arc::new(RecordingHandler {
            delay_ms: 30,
            ..RecordingHandler::default()
        });
        let mut cfg = config();
        cfg.debounce_ms = 20;
        cfg.mode = QueueMode::Followup;
        let queue = ChatQueue::new(cfg, handler.clone());
        let chat = ChatId::Group(1);

        queue.ingest(msg(chat, 1)).await;
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        // 回合进行中到达：按 followup 留到下一批。
        queue.ingest(msg(chat, 2)).await;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        {
            let calls = handler.calls.lock().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[1].messages[0].message_id, 2);
        }
        let states = queue.states.lock().await;
        let state = states.get(&chat).unwrap();
        assert!(state.active.is_none());
        assert_eq!(state.pending_batches, 0, "batch reservation leaked");
        assert!(state.pending.is_empty());
    }

    #[tokio::test]
    async fn steering_reaches_running_turn() {
        let handler = Arc::new(RecordingHandler {
            delay_ms: 200,
            ..RecordingHandler::default()
        });
        let mut cfg = config();
        cfg.max_concurrent = 1;
        cfg.debounce_ms = 30;
        let queue = ChatQueue::new(cfg, handler.clone());
        let chat = ChatId::Group(1);
        queue.ingest(msg(chat, 1)).await;
        // 等去抖触发、回合开始。
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        // 回合正在进行，新消息走 steering。
        queue.ingest(msg(chat, 2)).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let calls = handler.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        drop(calls);
        let steer = handler.steer.lock().unwrap();
        assert_eq!(steer.len(), 1);
        match &steer[0] {
            AgentMessage::User { text } => assert!(text.contains("m2")),
            other => panic!("unexpected {other:?}"),
        }
    }
}
