//! 聊天队列：去抖、合并、上限与并发。
//!
//! 设计依据：设计 §3.4 / §4 / §9.2 `[queue]`。
//!
//! 语义：
//! - 同一 chat 的连发在 `debounce_ms` 窗口内合并成一个回合；不同 chat 各自计时。
//! - 回合运行期间到达的消息（`[queue].mode`）：
//!   - `steer`：直接进入 steering 队列，当前回合在下一个模型边界看到它们；
//!   - `followup`：留在待处理里，回合结束后**逐条**各开一个回合；
//!   - `collect`：留在待处理里，回合结束后再等一个 `debounce_ms`，期间到达的一起合并成
//!     **一个**回合。
//! - steer 进去的消息若回合没有回应（回合提前结束、或被跳过），回合结束时按原顺序
//!   放回待处理队首，再开一轮；一条消息不会“送进去了却没人回”。
//! - 每 chat 限制待处理消息条数；溢出按 `summarize | old | new` 处理，
//!   `summarize` 只保留一个“丢弃了多少条”的计数，由回合提示语转述。
//! - 全局并发由 `max_concurrent` 控制。
//!
//! 本模块不依赖 agent 实现：真正的回合执行由 [`TurnHandler`] 提供。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{Mutex, Semaphore, mpsc};
use tracing::{debug, warn};

use crate::config::{DropPolicy, QueueConfig, QueueMode};
use crate::qq::event::{ChatId, QqMessage};

/// 回合结束后开下一个回合前的短暂间隔（毫秒）：让收尾时到达的消息赶上同一批。
const FOLLOW_UP_DELAY_MS: u64 = 50;

/// 一次回合的输入。
#[derive(Debug, Clone)]
pub struct TurnInput {
    pub chat: ChatId,
    pub messages: Vec<QqMessage>,
    /// `summarize` 丢弃的消息条数；>0 时提示词里要说明。
    pub dropped: u64,
}

/// 一个回合的收尾结果。
#[derive(Debug, Default)]
pub struct TurnReport {
    /// 回合执行错误；`None` 表示正常结束（包括按开关、预算跳过）。
    pub error: Option<anyhow::Error>,
    /// 经 steering 送进本回合、但没有得到回应的消息条数。
    ///
    /// steering 按到达顺序消费，没回应的一定是最后送进来的那几条；队列把它们按原
    /// 顺序放回待处理队首，回合结束后再开一轮。
    pub unanswered: usize,
}

#[cfg(test)]
impl TurnReport {
    /// 测试用：回合出错时 panic，否则返回没回应的 steering 消息条数。
    pub(crate) fn expect_ok(self) -> usize {
        if let Some(error) = self.error {
            panic!("turn failed: {error:#}");
        }
        self.unanswered
    }
}

/// 回合执行器。
#[async_trait::async_trait]
pub trait TurnHandler: Send + Sync {
    /// 执行一个回合；`steering` 在回合运行期间持续接收同 chat 的新消息。
    ///
    /// 通道里是原始的 [`QqMessage`]：执行器要据发送者更新本回合证据
    /// （别人的消息进入回合会撤销 owner intent），渲染也由执行器负责。
    ///
    /// 执行器负责在收尾时关闭 `steering`：关闭之后的 `send` 会失败，队列据此把
    /// 消息留在待处理里。
    async fn handle(
        &self,
        input: TurnInput,
        steering: mpsc::UnboundedReceiver<QqMessage>,
    ) -> TurnReport;
}

/// 每个 chat 的状态。
#[derive(Default)]
struct ChatState {
    pending: VecDeque<QqMessage>,
    dropped: u64,
    /// 当前回合的 steering 发送端；`None` 表示空闲。
    active: Option<mpsc::UnboundedSender<QqMessage>>,
    /// 本回合已经 steer 进去的消息（按到达顺序）；回合没回应的部分会放回 `pending`。
    steered: Vec<QqMessage>,
    /// 去抖令牌：只有最新一次计时有效。
    token: u64,
    /// 已排队（含运行中）的批次数。
    pending_batches: usize,
}

/// 一次触发从待处理里取多少条。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Take {
    /// 全部：去抖窗口里的连发、`steer` / `collect` 的后续批次。
    All,
    /// 只取最早的一条：`followup` 的后续回合逐条处理。
    One,
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
    pub(crate) fn new(config: QueueConfig, handler: Arc<dyn TurnHandler>) -> Arc<Self> {
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
    pub(crate) fn reload_config(&self, config: QueueConfig) {
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
    pub(crate) async fn ingest(self: &Arc<Self>, message: QqMessage) {
        let chat = message.chat;
        let config = self.config();
        let mut states = self.states.lock().await;
        let state = states.entry(chat).or_default();

        // 回合进行中：steer 模式直接注入。
        if config.mode == QueueMode::Steer
            && let Some(steering) = &state.active
        {
            let sent = steering.send(message.clone()).is_ok();
            if sent {
                // 记下已送进回合的消息：回合若没有回应它们，结束时放回 pending。
                state.steered.push(message);
            } else {
                // handler 已关闭 steering 但 active 尚未清空：放回 pending，
                // 回合结束时会补开一轮。
                self.push_with_caps(state, message);
            }
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
            self.schedule(chat, token, config.debounce_ms, Take::All);
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
    fn schedule(self: &Arc<Self>, chat: ChatId, token: u64, delay_ms: u64, take: Take) {
        let queue = Arc::clone(self);
        tokio::spawn(async move {
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            queue.fire(chat, token, take).await;
        });
    }

    /// 去抖窗口结束：取出一批并启动回合。
    async fn fire(self: &Arc<Self>, chat: ChatId, token: u64, take: Take) {
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

            let count = match take {
                Take::All => state.pending.len(),
                Take::One => 1,
            };
            let messages: Vec<QqMessage> = state.pending.drain(..count).collect();
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
                    // 回合没跑：等待期间 steer 进去的消息原样放回队首。
                    for message in std::mem::take(&mut state.steered).into_iter().rev() {
                        state.pending.push_front(message);
                    }
                }
                return;
            }
        };
        let handler = Arc::clone(&self.handler);
        let queue = Arc::clone(self);
        tokio::spawn(async move {
            let messages = batch.messages.len();
            let report = handler.handle(batch, steering_rx).await;
            match &report.error {
                None => debug!(chat = %chat, messages, "turn finished"),
                Some(error) => warn!(%error, chat = %chat, "turn failed"),
            }
            drop(permit);
            drop(steering_tx);

            // 回合结束：清理状态，若期间还有消息则再开一轮。
            let mut states = queue.states.lock().await;
            if let Some(state) = states.get_mut(&chat) {
                state.active = None;
                state.pending_batches = state.pending_batches.saturating_sub(1);
                // 没得到回应的 steering 消息是最后送进去的几条；按原顺序放回队首——
                // 它们比收尾期间回填进 pending 的消息更早。
                let steered = std::mem::take(&mut state.steered);
                let unanswered = report.unanswered.min(steered.len());
                for message in steered.into_iter().rev().take(unanswered) {
                    state.pending.push_front(message);
                }
                if !state.pending.is_empty() {
                    state.token = queue.counter.fetch_add(1, Ordering::Relaxed) + 1;
                    let token = state.token;
                    // 预约下一批；`fire` 接手后不再重复计数。
                    state.pending_batches += 1;
                    drop(states);
                    // 后续批次：`collect` 再等一个完整去抖窗口把这段时间的消息并成一批；
                    // `followup` 逐条开回合；`steer` 只剩没送进去或没回应的几条，小窗口后一起处理。
                    let config = queue.config();
                    let (delay, take) = match config.mode {
                        QueueMode::Collect => (config.debounce_ms, Take::All),
                        QueueMode::Followup => (FOLLOW_UP_DELAY_MS, Take::One),
                        QueueMode::Steer => (FOLLOW_UP_DELAY_MS, Take::All),
                    };
                    queue.schedule(chat, token, delay, take);
                }
            }
        });
    }

    /// 当前待处理消息数（测试与 status 使用）。
    #[cfg(test)]
    pub(crate) async fn pending(&self, chat: ChatId) -> usize {
        self.states
            .lock()
            .await
            .get(&chat)
            .map(|state| state.pending.len())
            .unwrap_or(0)
    }

    /// 正在运行的回合数。
    pub(crate) async fn active_turns(&self) -> usize {
        self.states
            .lock()
            .await
            .values()
            .filter(|state| state.active.is_some())
            .count()
    }

    /// 没有运行中的回合、没有待派发批次、没有排队消息。
    pub(crate) async fn is_idle(&self) -> bool {
        self.states.lock().await.values().all(|state| {
            state.active.is_none() && state.pending.is_empty() && state.pending_batches == 0
        })
    }
}

#[async_trait::async_trait]
impl crate::memory::QueueActivity for ChatQueue {
    async fn running_turns(&self) -> usize {
        ChatQueue::active_turns(self).await
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
        steer: StdMutex<Vec<QqMessage>>,
        /// 每个回合人为拖延的时间。
        delay_ms: u64,
    }

    #[async_trait::async_trait]
    impl TurnHandler for RecordingHandler {
        async fn handle(
            &self,
            input: TurnInput,
            mut steering: mpsc::UnboundedReceiver<QqMessage>,
        ) -> TurnReport {
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
            TurnReport::default()
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
    async fn followup_runs_queued_messages_one_turn_each_and_collect_merges_them() {
        for (mode, expected) in [
            (QueueMode::Followup, vec![vec![1], vec![2], vec![3]]),
            (QueueMode::Collect, vec![vec![1], vec![2, 3]]),
        ] {
            let handler = Arc::new(RecordingHandler {
                delay_ms: 80,
                ..RecordingHandler::default()
            });
            let mut cfg = config();
            cfg.debounce_ms = 20;
            cfg.mode = mode;
            let queue = ChatQueue::new(cfg, handler.clone());
            let chat = ChatId::Group(1);

            queue.ingest(msg(chat, 1)).await;
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            // 第一回合进行中到达两条。
            queue.ingest(msg(chat, 2)).await;
            queue.ingest(msg(chat, 3)).await;
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;

            let calls = handler.calls.lock().unwrap();
            let batches: Vec<Vec<i64>> = calls
                .iter()
                .map(|call| {
                    call.messages
                        .iter()
                        .map(|message| message.message_id)
                        .collect()
                })
                .collect();
            assert_eq!(batches, expected, "{mode:?}");
            assert!(
                handler.steer.lock().unwrap().is_empty(),
                "{mode:?} must not steer"
            );
        }
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
        assert_eq!(steer[0].message_id, 2);
    }

    #[tokio::test]
    async fn idle_covers_pending_debounce_batches() {
        let handler = Arc::new(RecordingHandler::default());
        let queue = ChatQueue::new(config(), handler);
        assert!(queue.is_idle().await);
        queue.ingest(msg(ChatId::Group(1), 1)).await;
        assert!(!queue.is_idle().await);
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert!(queue.is_idle().await);
    }

    struct ClosingHandler {
        calls: StdMutex<Vec<TurnInput>>,
        closed: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl TurnHandler for ClosingHandler {
        async fn handle(
            &self,
            input: TurnInput,
            mut steering: mpsc::UnboundedReceiver<QqMessage>,
        ) -> TurnReport {
            self.calls.lock().unwrap().push(input);
            if self.calls.lock().unwrap().len() == 1 {
                // 模拟 Runner::execute 收尾：先 close，让后续 send 立即失败。
                steering.close();
                self.closed.notify_one();
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            }
            TurnReport::default()
        }
    }

    #[tokio::test]
    async fn send_after_close_requeues_the_message() {
        let handler = Arc::new(ClosingHandler {
            calls: StdMutex::new(Vec::new()),
            closed: tokio::sync::Notify::new(),
        });
        let mut cfg = config();
        cfg.debounce_ms = 20;
        let queue = ChatQueue::new(cfg, handler.clone());
        let chat = ChatId::Group(1);

        queue.ingest(msg(chat, 1)).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), handler.closed.notified())
            .await
            .expect("the first turn should start and close its steering receiver");
        // close 已生效：这条 send 会失败，ChatQueue 必须把它回填到 pending。
        queue.ingest(msg(chat, 2)).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let calls = handler.calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "the requeued message should open a second turn"
        );
        assert_eq!(calls[1].messages[0].message_id, 2);
    }

    /// 回合收到 steering 消息但没有回应（比如提前结束）时报告条数；
    /// 队列把这几条按原顺序放回，排在收尾期间回填的消息前面，再开一轮。
    struct UnansweredHandler {
        calls: StdMutex<Vec<TurnInput>>,
        steered: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl TurnHandler for UnansweredHandler {
        async fn handle(
            &self,
            input: TurnInput,
            mut steering: mpsc::UnboundedReceiver<QqMessage>,
        ) -> TurnReport {
            let first = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(input);
                calls.len() == 1
            };
            if !first {
                return TurnReport::default();
            }
            // 等两条 steering 消息到达，然后像提前结束的回合一样收尾：关闭、排空、
            // 只回应第一条。
            let mut received = 0;
            while received < 2 {
                if steering.recv().await.is_some() {
                    received += 1;
                }
            }
            steering.close();
            self.steered.notify_one();
            // 关闭之后到达的消息走 pending。
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            TurnReport {
                error: None,
                unanswered: 1,
            }
        }
    }

    #[tokio::test]
    async fn unanswered_steering_messages_open_a_follow_up_turn_in_order() {
        let handler = Arc::new(UnansweredHandler {
            calls: StdMutex::new(Vec::new()),
            steered: tokio::sync::Notify::new(),
        });
        let mut cfg = config();
        cfg.debounce_ms = 20;
        cfg.mode = QueueMode::Steer;
        let queue = ChatQueue::new(cfg, handler.clone());
        let chat = ChatId::Group(1);

        queue.ingest(msg(chat, 1)).await;
        // 等第一回合开始，再 steer 两条进去。
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        queue.ingest(msg(chat, 2)).await;
        queue.ingest(msg(chat, 3)).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            handler.steered.notified(),
        )
        .await
        .expect("the first turn should receive both steering messages");
        // steering 已关闭：这条进 pending。
        queue.ingest(msg(chat, 4)).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let calls = handler.calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "unanswered messages must open a second turn"
        );
        let ids: Vec<i64> = calls[1]
            .messages
            .iter()
            .map(|message| message.message_id)
            .collect();
        // 2 已回应；3 没回应，排在关闭后才到的 4 前面。
        assert_eq!(ids, vec![3, 4]);
    }
}
