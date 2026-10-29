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
    config: QueueConfig,
    handler: Arc<dyn TurnHandler>,
    states: Mutex<HashMap<ChatId, ChatState>>,
    semaphore: Arc<Semaphore>,
    counter: AtomicU64,
}

impl ChatQueue {
    pub fn new(config: QueueConfig, handler: Arc<dyn TurnHandler>) -> Arc<Self> {
        let max = config.max_concurrent.max(1);
        Arc::new(Self {
            config,
            handler,
            states: Mutex::new(HashMap::new()),
            semaphore: Arc::new(Semaphore::new(max)),
            counter: AtomicU64::new(0),
        })
    }

    /// 注入一条已通过过滤器的消息。
    pub async fn ingest(self: &Arc<Self>, message: QqMessage) {
        let chat = message.chat;
        let mut states = self.states.lock().await;
        let state = states.entry(chat).or_default();

        // 回合进行中：steer 模式直接注入。
        if let Some(steering) = &state.active
            && self.config.mode == QueueMode::Steer
        {
            let _ = steering.send(AgentMessage::user(render_inbound(&message)));
            return;
        }

        self.push_with_caps(state, message);
        state.token = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let token = state.token;

        // 如果已有回合在跑，等它结束；否则按去抖窗口触发。
        // `pending_batches` 只在真正形成批次（`fire`）时增加，避免重复计数。
        if state.active.is_none() && state.pending_batches == 0 {
            drop(states);
            self.schedule(chat, token, self.config.debounce_ms);
        }
    }

    /// 按上限压入待处理队列。
    fn push_with_caps(&self, state: &mut ChatState, message: QqMessage) {
        if state.pending.len() >= self.config.cap_messages {
            match self.config.drop {
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
        let (batch, steering_tx, permission) = {
            let mut states = self.states.lock().await;
            let Some(state) = states.get_mut(&chat) else {
                return;
            };
            // 更新的消息到来了：这次计时作废。
            if state.token != token || state.pending.is_empty() {
                return;
            }

            // 批次上限：超出时按丢弃策略处理旧批次（这里只有消息，直接丢最旧）。
            if state.pending_batches > self.config.cap_batches {
                state.pending.pop_front();
                state.dropped += 1;
            }

            let messages: Vec<QqMessage> = state.pending.drain(..).collect();
            let dropped = std::mem::take(&mut state.dropped);
            let (steering_tx, steering_rx) = mpsc::unbounded_channel();
            state.active = Some(steering_tx.clone());
            state.pending_batches += 1;

            let permit = match Arc::clone(&self.semaphore).acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => return,
            };
            (
                TurnInput {
                    chat,
                    messages,
                    dropped,
                },
                steering_tx,
                (permit, steering_rx),
            )
        };

        let (permit, steering_rx) = permission;
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
