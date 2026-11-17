//! 回合执行器：把队列输入接到 agent loop、transcript 与工具上。
//!
//! 设计依据：设计 §3.4 / §4 / §8.1。
//!
//! 并发模型（共享 transcript）：
//! - 入站消息由回合开始时立即追加到 transcript，并进入共享历史；
//! - 每个回合在**本地副本**上运行，避免别的回合的未提交块插进工具调用之间；
//! - 回合结束把新块写入 `turn_buffer`，再一次性提交到 transcript，最后并入共享历史。
//!
//! runtime context 每回合单独注入，不写进 transcript；它承载时间、origin_chat、
//! `current_session`、收件箱摘要等易变字段。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use crate::agent::queue::{TurnHandler, TurnInput, TurnReport};
use crate::agent::{
    AgentEvent, AgentLoop, AgentMessage, FallbackProvider, LoopConfig, LoopOutcome, SteeringQueue,
    TurnEvidence,
};
use crate::config::{AgentMode, GateMode};
use crate::prompt::{self, RuntimeContext};
use crate::providers::{Provider, ProviderSlot, SharedProviderSlot, SystemBlock, ThinkingOptions};
use crate::qq::event::{ChatId, QqMessage};
use crate::qq::{QqClient, format};
use crate::security::gate::Gate;
use crate::session::SessionStore;
use crate::session::turn_buffer::{TurnBufferStore, new_turn_id};
use crate::storage::messages::MessageStore;
use crate::tools::ToolRegistry;
use crate::vfs::sqlite::VfsStore;

mod compaction;
mod evidence;
mod images;
mod injected;
mod journal;
mod ops;
mod prepare;
mod review;

pub use ops::RuntimeOps;

/// 运行配置。
#[derive(Clone)]
pub struct RunnerConfig {
    pub model: String,
    pub mode: AgentMode,
    pub account_qq: u64,
    pub owner_qq: u64,
    pub master_qq: u64,
    pub owner_name: String,
    pub receive_scopes: Vec<String>,
    pub send_scopes: Vec<String>,
    pub gate_mode: GateMode,
    /// 以下几项是**启动时**的模型设置，只用来初始化 provider 槽位；运行中以槽位为准
    /// （`sbot model set` 热切换会整体替换槽位，见 [`ProviderSlot`]）。
    pub max_tokens: u32,
    /// 模型是否声明接受图片输入；只影响入站图片自动注入。
    pub supports_images: bool,
    pub thinking: ThinkingOptions,
    pub max_steps: u32,
    pub retry: crate::agent::agent_loop::RetryPolicy,
    /// 注入“上次回复以来的群消息”时最多取多少条。
    pub chat_history_limit: u32,
    /// 会话级注入的预算（`/USER.md`、`/MEMORY.md`、风格档案）；见 `prompt::context`。
    pub context_budget: prompt::context::ContextBudget,
    /// 交给 provider 执行的服务端工具。
    pub server_tools: Vec<crate::providers::ServerToolKind>,
    /// 单回合工作集上限；超过则先压缩。
    pub turn_context_budget: u32,
    /// 压缩后保留的最近 token 数。
    pub keep_recent_tokens: u32,
    /// 连续自动压缩失败多少次后停止尝试（避免每个回合都打一次失败的摘要请求）。
    pub compaction_max_failures: u32,
    /// 周期安全检查器；`None` = 关闭。
    pub security_review: Option<Arc<dyn crate::security::review::SecurityCheck>>,
    /// 每新增这么多 token 在回合边界检查一次；0 = 关闭。
    pub check_every_tokens: u64,
    /// 检查窗口向前重叠的近似 token。
    pub check_overlap_tokens: u64,
    /// 压缩前也检查一次。
    pub check_on_compaction: bool,
    /// 模型上下文窗口（启动时的值，初始化槽位用）；与 `turn_context_budget` 一起决定压缩阈值。
    pub context_window: u64,
    /// 压缩预算里预留给输出/系统提示的 token。
    pub reserve_tokens: u32,
    /// 是否裁剪旧工具结果。
    pub prune: bool,
    pub prune_soft_limit: u32,
    pub prune_keep_turns: u32,
    /// 压缩前静默提取 durable facts。
    pub memory_flush: Option<Arc<crate::memory::MemoryFlusher>>,
    /// 压缩后把记忆索引写进新分段。
    pub post_compact_recall: bool,
    /// 摘要使用独立模型；`None` = 主模型。
    pub compaction: Option<Arc<CompactionModel>>,
    /// 最近一次回合活动时间（unix 秒），供记忆复盘判断空闲。
    pub last_activity: Arc<std::sync::atomic::AtomicI64>,
    /// 运行预算；`None` = 不限制（测试/旧路径）。
    pub budget: Option<Arc<crate::budget::Budget>>,
    /// 输入/输出闸的 state 截断长度（字符），来自 `guard.max_state_tokens`。
    pub max_state_chars: usize,
    /// `[hooks.events]`；BeforeSend 在 API 里执行，其余事件由 Runner 触发。
    pub event_hooks: Vec<crate::config::EventHookConfig>,
}

/// 摘要用的独立 provider 与模型 id。
pub struct CompactionModel {
    pub provider: Arc<dyn Provider>,
    pub model: String,
}

/// Runner 的稳定依赖：存储、工具、QQ 与停机开关。
struct RunnerDeps {
    tools: Arc<ToolRegistry>,
    gate: Arc<Gate>,
    vfs: Arc<VfsStore>,
    messages: Arc<MessageStore>,
    sessions: Arc<SessionStore>,
    turn_buffer: Arc<TurnBufferStore>,
    qq: Option<Arc<QqClient>>,
    ai_enabled: Arc<std::sync::atomic::AtomicBool>,
    estop_file: std::path::PathBuf,
}

/// 回合执行器。
pub struct Runner {
    /// 当前 provider；热切换时由 runtime 原地替换。
    slot: SharedProviderSlot,
    /// 主模型失败后的回退链。
    fallbacks: Vec<FallbackProvider>,
    /// 子代理 hub；后置注入，用于嵌套 spawn 时给子代理注册 `Agent` 工具。
    hub: std::sync::OnceLock<Arc<crate::swarm::AgentHub>>,
    /// `sbot ai stop` 的开关；关闭时不发起新的模型请求。
    deps: Arc<RunnerDeps>,
    config: RunnerConfig,
    /// 已提交的全局 transcript（内存镜像）。
    history: Arc<Mutex<Vec<AgentMessage>>>,
    /// 单会话：所有回合共用一把锁。
    turn_lock: Arc<tokio::sync::Mutex<()>>,
    /// 静态 + hooks 组成的 system 块；压缩成功后重建（`PostCompact` 语义）。
    system: std::sync::RwLock<Vec<SystemBlock>>,
    /// 压缩互斥：同一时刻只允许一次。
    compaction_lock: Mutex<()>,
    /// 连续自动压缩失败次数；达到上限后不再自动尝试。
    compaction_failures: std::sync::atomic::AtomicU32,
    /// 每 chat 最近允许的回合时间，用于 `chat_turns_per_hour`。
    turn_times: Mutex<HashMap<String, VecDeque<Instant>>>,
    /// 自上次周期检查以来新增的 token 估算。
    unchecked_tokens: std::sync::atomic::AtomicU64,
    /// 周期检查连续失败次数。
    review_failures: std::sync::atomic::AtomicU32,
    /// `critical` 周期检查后置位：下一个回合开始前丢弃当前上下文（见 `review`）。
    discard_context: std::sync::atomic::AtomicBool,
    /// 周期检查互斥。
    review_lock: Mutex<()>,
    /// 每个 transcript scope 一个回合锁；shared 模式天然只有一个锁。
    /// 当前正在跑的主/injected 回合数；idle_only 用。
    active_turns: std::sync::atomic::AtomicUsize,
}

/// 把回合证据注入每次工具调用。
///
/// `ToolExecutor` 的签名不带上下文；runner 每个回合包一层，把
/// owner intent 与 origin_chat 传给 `API` 门禁。
struct TurnExecutor {
    registry: Arc<ToolRegistry>,
    evidence: TurnEvidence,
}

#[async_trait::async_trait]
impl crate::agent::ToolExecutor for TurnExecutor {
    fn specs(&self) -> Vec<crate::providers::ToolSpec> {
        self.registry.specs()
    }

    fn execution_mode(&self, name: &str) -> crate::agent::ExecutionMode {
        self.registry.execution_mode(name)
    }

    async fn execute(&self, call: &crate::agent::ToolCall) -> crate::agent::ToolResult {
        self.registry
            .execute_with_evidence(call, self.evidence.clone())
            .await
    }
}

/// 预算跳过回合时，本批图片仍写进 transcript 与历史：消息已经入库，预算恢复后的回合
/// 不能丢掉这些视觉上下文（图片只在这一刻下载得到）。
async fn keep_images(
    sessions: &SessionStore,
    history: &Mutex<Vec<AgentMessage>>,
    image_prompts: Vec<AgentMessage>,
) -> Result<()> {
    if !image_prompts.is_empty() {
        sessions.append(&image_prompts)?;
        history.lock().await.extend(image_prompts);
    }
    Ok(())
}

struct ActiveTurnGuard<'a>(&'a std::sync::atomic::AtomicUsize);
impl Drop for ActiveTurnGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Runner {
    /// 不带回退链构造。
    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    pub(crate) async fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<ToolRegistry>,
        gate: Arc<Gate>,
        vfs: Arc<VfsStore>,
        messages: Arc<MessageStore>,
        sessions: Arc<SessionStore>,
        turn_buffer: Arc<TurnBufferStore>,
        qq: Option<Arc<QqClient>>,
        config: RunnerConfig,
        history: Arc<Mutex<Vec<AgentMessage>>>,
        ai_enabled: Arc<std::sync::atomic::AtomicBool>,
        estop_file: std::path::PathBuf,
    ) -> Result<Arc<Self>> {
        Self::new_with_fallbacks(
            provider,
            Vec::new(),
            tools,
            gate,
            vfs,
            messages,
            sessions,
            turn_buffer,
            qq,
            config,
            history,
            ai_enabled,
            estop_file,
        )
        .await
    }

    /// 带回退链构造。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new_with_fallbacks(
        provider: Arc<dyn Provider>,
        fallbacks: Vec<FallbackProvider>,
        tools: Arc<ToolRegistry>,
        gate: Arc<Gate>,
        vfs: Arc<VfsStore>,
        messages: Arc<MessageStore>,
        sessions: Arc<SessionStore>,
        turn_buffer: Arc<TurnBufferStore>,
        qq: Option<Arc<QqClient>>,
        config: RunnerConfig,
        history: Arc<Mutex<Vec<AgentMessage>>>,
        ai_enabled: Arc<std::sync::atomic::AtomicBool>,
        estop_file: std::path::PathBuf,
    ) -> Result<Arc<Self>> {
        // 恢复崩溃前未提交的回合，再载入当前分段。
        let recovered = turn_buffer.recover_open(&sessions).await?;
        if recovered > 0 {
            warn!(
                recovered,
                "recovered interrupted turns from the turn buffer"
            );
        }
        let current = sessions.current()?;
        let mut recovered_history = sessions.read_segment(&current.file)?;
        // transcript 不改写；隔离过的原文在恢复时重新替换成占位符。
        let redacted = ops::reapply_quarantine(&vfs, &mut recovered_history);
        if redacted > 0 {
            debug!(redacted, "re-applied quarantine to the recovered context");
        }
        {
            let mut shared = history.lock().await;
            // 只在此处初始化一次；重复构造会重置内存历史。
            if shared.is_empty() {
                *shared = recovered_history;
            }
        }

        let slot = Arc::new(std::sync::RwLock::new(ProviderSlot {
            reference: config.model.clone(),
            api_model: provider.model_id().to_owned(),
            provider,
            max_tokens: config.max_tokens,
            thinking: config.thinking.clone(),
            context_window: config.context_window,
            supports_images: config.supports_images,
        }));

        let deps = Arc::new(RunnerDeps {
            tools,
            gate,
            vfs,
            messages,
            sessions,
            turn_buffer,
            qq,
            ai_enabled,
            estop_file,
        });
        let runner = Arc::new(Self {
            slot,
            fallbacks,
            hub: std::sync::OnceLock::new(),
            deps,
            config,
            history,
            turn_lock: Arc::new(tokio::sync::Mutex::new(())),
            // 下面由 `rebuild_system` 填上：启动与压缩之后是同一套组装。
            system: std::sync::RwLock::new(Vec::new()),
            compaction_lock: Mutex::new(()),
            compaction_failures: std::sync::atomic::AtomicU32::new(0),
            turn_times: Mutex::new(HashMap::new()),
            unchecked_tokens: std::sync::atomic::AtomicU64::new(0),
            review_failures: std::sync::atomic::AtomicU32::new(0),
            discard_context: std::sync::atomic::AtomicBool::new(false),
            review_lock: Mutex::new(()),
            active_turns: std::sync::atomic::AtomicUsize::new(0),
        });
        runner.rebuild_system();
        let _ = runner
            .fire_hooks(crate::config::HookEvent::SessionStart, None, None)
            .await;
        Ok(runner)
    }

    pub(crate) fn is_busy(&self) -> bool {
        self.active_turns.load(std::sync::atomic::Ordering::Relaxed) > 0
    }

    /// 当前 system 块快照。
    pub(crate) fn system(&self) -> Vec<SystemBlock> {
        self.system
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 重新读取 hooks 并重建 system 块（压缩成功后调用）。
    /// 触发一个非 BeforeSend 事件：`log` 由 [`crate::agent::hooks::fire`] 记录，`block`
    /// 交给调用方，`notify` 在这里私聊 `master_qq` 一次（设了渠道、且 NapCat 在线时）。
    /// 这条通知是运行时发出的，不经过 BeforeSend。
    async fn fire_hooks(
        &self,
        event: crate::config::HookEvent,
        chat: Option<ChatId>,
        detail: Option<&str>,
    ) -> crate::agent::hooks::HookOutcome {
        let outcome = crate::agent::hooks::fire(&self.config.event_hooks, event, chat);
        if outcome.notify > 0
            && self.config.master_qq != 0
            && let Some(qq) = &self.deps.qq
        {
            let body = format!(
                "[hook {event:?}] {}",
                detail
                    .unwrap_or("no detail")
                    .chars()
                    .take(500)
                    .collect::<String>()
            );
            if let Err(error) = qq
                .send_text(ChatId::Private(self.config.master_qq), &body, None)
                .await
            {
                warn!(%error, ?event, "cannot deliver a hook notification");
            }
        }
        outcome
    }

    /// 某个事件的 `inject_context` 文本（`path` 从 VFS 读取）。
    fn hook_context(&self, event: crate::config::HookEvent, chat: Option<ChatId>) -> Vec<String> {
        crate::agent::hooks::injected(&self.config.event_hooks, event, chat, &self.deps.vfs)
    }

    /// 进程退出前调用：触发 `SessionEnd`（`log` / `notify`）。
    pub(crate) async fn end_session(&self) {
        let _ = self
            .fire_hooks(
                crate::config::HookEvent::SessionEnd,
                None,
                Some("the daemon is shutting down"),
            )
            .await;
    }

    /// 组装 system：静态提示词、VFS 里常驻注入的文件、`session_start` hook 注入的上下文。
    ///
    /// 启动时与每次压缩之后都走这里，两处得到的是同一种 system。
    fn rebuild_system(&self) {
        let mut system = vec![SystemBlock {
            text: prompt::render_static(self.config.mode),
            cache: true,
        }];
        for block in prompt::context::load(&self.deps.vfs, &self.config.context_budget).render() {
            system.push(SystemBlock {
                text: block,
                cache: false,
            });
        }
        for text in self.hook_context(crate::config::HookEvent::SessionStart, None) {
            system.push(SystemBlock { text, cache: false });
        }
        *self
            .system
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = system;
    }

    /// 组装运行上下文。
    fn runtime_context(&self, chat: ChatId, sessions: &SessionStore) -> RuntimeContext {
        let inbox = self
            .deps
            .messages
            .unread_counts()
            .map(|counts| {
                counts
                    .iter()
                    .map(|(chat, count)| format!("{chat}: {count}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let session = sessions
            .current_path()
            .unwrap_or_else(|_| "/sessions/unknown.jsonl".to_owned());
        let slot = self.provider_snapshot();
        RuntimeContext {
            mode: match self.config.mode {
                AgentMode::StandIn => "stand_in".to_owned(),
                AgentMode::Companion => "companion".to_owned(),
            },
            account_qq: self.config.account_qq,
            owner_name: self.config.owner_name.clone(),
            origin_chat: chat.to_string(),
            owner_qq: self.config.owner_qq,
            master_qq: self.config.master_qq,
            current_time: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            model: slot.reference.clone(),
            thinking_level: slot.thinking.effort.clone().unwrap_or_default(),
            current_session: session,
            receive_scopes: self.config.receive_scopes.clone(),
            send_scopes: self.config.send_scopes.clone(),
            group_roles: String::new(),
            gate_mode: match self.config.gate_mode {
                GateMode::Trusted => "trusted",
                GateMode::Auto => "auto",
                GateMode::Approve => "approve",
            }
            .to_owned(),
            inbox_summary: if inbox.is_empty() {
                "nothing unread".to_owned()
            } else {
                inbox
            },
        }
    }

    /// 共享槽位；runtime 用它实现模型/effort 热切换。
    pub(crate) fn provider_slot(&self) -> SharedProviderSlot {
        Arc::clone(&self.slot)
    }

    /// 运行预算；热切换新模型时，新 provider 要接到同一个计量去处。
    pub(crate) fn budget(&self) -> Option<Arc<crate::budget::Budget>> {
        self.config.budget.clone()
    }

    /// 注入子代理 hub；之后嵌套 spawn 才能为子代理注册 `Agent` 工具。
    pub(crate) fn set_hub(&self, hub: Arc<crate::swarm::AgentHub>) {
        let _ = self.hub.set(hub);
    }

    /// 取一份 provider 快照；调用方在 await 前调用，避免跨 await 持有锁。
    fn provider_snapshot(&self) -> ProviderSlot {
        self.slot
            .read()
            .expect("provider slot lock poisoned")
            .clone()
    }

    /// 主回合、注入回合与子代理共用的循环配置。
    ///
    /// 模型 id、输出上限与 thinking 取自调用方的 `slot`：调用方用同一个快照里的 provider，
    /// 中途热切换模型时两者也不会错配（以前这里另取一次快照，provider 是旧模型、id 是新的）。
    fn loop_config(&self, slot: &ProviderSlot) -> LoopConfig {
        LoopConfig {
            model: slot.api_model.clone(),
            system: self.system(),
            max_tokens: slot.max_tokens,
            thinking: slot.thinking.clone(),
            server_tools: self.config.server_tools.clone(),
            max_steps: self.config.max_steps,
            retry: self.config.retry,
            max_state_chars: self.config.max_state_chars,
            prune: self.config.prune,
            prune_soft_limit: self.config.prune_soft_limit as usize,
            prune_keep_turns: self.config.prune_keep_turns,
            max_turn_tokens: self
                .config
                .budget
                .as_ref()
                .map(|budget| budget.turn_tokens())
                .unwrap_or(0),
            max_wall_secs: self
                .config
                .budget
                .as_ref()
                .map(|budget| budget.wall_secs())
                .unwrap_or(0),
        }
    }

    /// 跑一个完整回合，并接管 steering 通道的收尾。
    async fn execute(
        &self,
        input: TurnInput,
        mut steering_rx: mpsc::UnboundedReceiver<QqMessage>,
    ) -> TurnReport {
        let mut unanswered = 0usize;
        let result = self
            .run_turn(input, &mut steering_rx, &mut unanswered)
            .await;

        // 关闭后 `send` 会失败，`ChatQueue::ingest` 据此把之后的消息留在 pending。
        // 正常结束时 `drive_turn` 已经关闭并排空过；这里兜底的是回合被跳过（开关、
        // 急停、暂停、预算）或提前出错的路径。剩下的消息都没有得到回应，交还队列。
        // 排空用 `recv().await`：tokio 保证 `close()` 后 `recv()` 返回 `None` 时，
        // 所有被接受的发送都已落进缓冲。
        steering_rx.close();
        while steering_rx.recv().await.is_some() {
            unanswered += 1;
        }
        TurnReport {
            error: result.err(),
            unanswered,
        }
    }

    /// 回合主体：前置检查、模型循环与提交都在这里。
    ///
    /// `unanswered` 返回经 steering 送进来、但本回合没有回应的消息条数。
    async fn run_turn(
        &self,
        input: TurnInput,
        steering_rx: &mut mpsc::UnboundedReceiver<QqMessage>,
        unanswered: &mut usize,
    ) -> Result<()> {
        if self.new_turns_paused(input.chat) {
            return Ok(());
        }
        let chat = input.chat;
        let turn_lock = Arc::clone(&self.turn_lock);
        let _turn_guard = turn_lock.lock().await;
        self.discard_context_if_flagged().await;
        let sessions = Arc::clone(&self.deps.sessions);
        let history = Arc::clone(&self.history);
        let turn_id = new_turn_id();

        self.config.last_activity.store(
            chrono::Utc::now().timestamp(),
            std::sync::atomic::Ordering::Relaxed,
        );

        // 1) 入站消息进入 transcript（在请求模型之前）。
        let (inbound, accepted_messages) = self
            .accept_inbound(chat, &input.messages, &sessions)
            .await?;

        // 2) 写进 transcript 之后才进共享历史；取历史副本，后续新块只写本地副本。
        let mut working = {
            let mut history = history.lock().await;
            history.extend(inbound);
            history.clone()
        };

        let image_prompts = self.inbound_image_prompts(&accepted_messages).await;

        // 2.5) 预算：消息已经入库，只跳过模型回合；下一个窗口/日期自动恢复。
        // 图片也持久化，避免预算恢复后丢失视觉上下文。预算检查必须在压缩之前：
        // 压缩本身要请求模型，预算耗尽后每次唤醒都会白白摘要一次。
        if self.turn_budget_exhausted(chat).await {
            return keep_images(&sessions, &history, image_prompts).await;
        }

        // 2.6) 超过工作集上限时先压缩，再重新载入历史副本。
        self.maybe_auto_compact(&mut working, &history, &sessions)
            .await;
        // 压缩会改写 `working`，基点在压缩之后取：否则 `working[base..]` 既会越界，
        // 也会把保留的旧块重复提交。
        let base = working.len();

        // 3) 组装本回合的即时提示。证据只看被接受的消息：被 `message_received` hook 拦下的
        // 不进上下文，模型看不到主人说了什么，也就谈不上照主人的话行动。
        let evidence = self.evidence_for_batch(chat, &accepted_messages);
        let mut prompts =
            self.turn_prompts(&input, !accepted_messages.is_empty(), &evidence, &sessions);

        // runtime/pending prompt 不落 transcript；入站图片属于真实消息内容，需要落库。
        let runtime_blocks = prompts.len();
        prompts.extend(image_prompts);

        let prompt_tokens = crate::session::compaction::estimate_messages(&prompts);
        let journal =
            journal::TurnJournal::new(Arc::clone(&self.deps.turn_buffer), turn_id, runtime_blocks);
        let (outcome, not_answered, journal) = self
            .drive_turn(chat, &mut working, prompts, evidence, steering_rx, journal)
            .await;
        *unanswered = not_answered;
        self.commit_turn(
            chat,
            journal,
            &sessions,
            &history,
            &working[base + runtime_blocks..],
            prompt_tokens,
        )
        .await?;
        // 每个回合一行 info：排障文档让用户找的就是这一行。
        info!(
            chat = %chat,
            stop = ?outcome.stop_reason,
            turns = outcome.turns,
            "turn completed"
        );
        Ok(())
    }
    /// 跑模型循环，把本回合产生的新块并入 `working`。
    ///
    /// 返回循环结果、“经 steering 送进来、但没有回应”的消息条数，以及本回合日志
    /// （循环期间并入上下文的块已经实时写进 turn buffer）。
    async fn drive_turn(
        &self,
        chat: ChatId,
        working: &mut Vec<AgentMessage>,
        prompts: Vec<AgentMessage>,
        evidence: TurnEvidence,
        steering_rx: &mut mpsc::UnboundedReceiver<QqMessage>,
        journal: journal::TurnJournal,
    ) -> (LoopOutcome, usize, journal::TurnJournal) {
        // 4) 运行循环。工具执行器带上本回合证据。
        let slot = self.provider_snapshot();
        // steering 进来的消息按发送者更新证据：别人的消息会撤销 owner intent。
        let mut inbound = evidence::SteeredInbound::new(
            chat,
            self.config.owner_qq,
            &self.config.event_hooks,
            evidence.clone(),
            &mut *steering_rx,
        );
        let executor: Arc<dyn crate::agent::ToolExecutor> = Arc::new(TurnExecutor {
            registry: Arc::clone(&self.deps.tools),
            evidence,
        });
        let loop_ = AgentLoop::new_with_fallbacks(
            Arc::clone(&slot.provider),
            self.fallbacks.clone(),
            executor,
            Arc::clone(&self.deps.gate),
            self.loop_config(&slot),
        );
        let turn_id = journal.turn_id().to_owned();
        let (events_tx, logger) = spawn_turn_logger(journal);
        let mut steering = SteeringQueue::default();
        let outcome = loop_
            .run_with_steering(
                working,
                prompts,
                &mut steering,
                Some(&mut inbound),
                &events_tx,
            )
            .await;
        drop(inbound);

        // 模型循环一结束就关闭 steering。之后的 Stop hook、提交、周期审查与已读标记
        // 可能要好几秒；这期间到达的消息 `send` 会失败，由队列留在 pending、回合结束后
        // 再开一轮，而不是“送进来了却没人回”。
        steering_rx.close();
        let mut unanswered = 0usize;
        while steering_rx.recv().await.is_some() {
            unanswered += 1;
        }
        // 提前结束（出错、超限、被闸拦下）时，循环已经取出、但还没进入上下文的消息也
        // 没有回应。两部分都是最后送进来的一段：不写进本回合，交还队列重新排队，
        // 下一轮按正常输入处理（带它自己的证据）。
        unanswered += steering.drain().len();

        drop(events_tx);
        let journal = self.reclaim_journal(logger, turn_id).await;
        // 用量已由计量 provider 逐请求记账（见 `providers::metered`）。
        // `Stop` 只允许 `log`（设计 §7.5）。
        let _ = self
            .fire_hooks(
                crate::config::HookEvent::Stop,
                Some(chat),
                Some(&format!("turn finished: {:?}", outcome.stop_reason)),
            )
            .await;
        (outcome, unanswered, journal)
    }

    /// 提交本回合新块：buffer → transcript → 共享历史，并跑周期检查。
    ///
    /// `new_blocks` 是本回合要落盘的块（不含 runtime / pending 提示）。
    async fn commit_turn(
        &self,
        chat: ChatId,
        journal: journal::TurnJournal,
        sessions: &Arc<SessionStore>,
        history: &Arc<Mutex<Vec<AgentMessage>>>,
        new_blocks: &[AgentMessage],
        prompt_tokens: usize,
    ) -> Result<()> {
        // 5) 提交新块：buffer -> transcript -> 共享历史。
        journal.commit(sessions, new_blocks).await?;
        if !new_blocks.is_empty() {
            history.lock().await.extend(new_blocks.iter().cloned());
        }
        self.accumulate_review_tokens(
            prompt_tokens + crate::session::compaction::estimate_messages(new_blocks),
        );
        self.maybe_security_review(history).await;

        // 6) 已读标记（模拟真人；失败不影响回合结果）。
        if let Some(qq) = &self.deps.qq {
            let _ = qq.mark_read(chat).await;
        }
        Ok(())
    }
}

/// 把模型循环的事件接到回合日志上，返回事件的发送端与后台任务。
///
/// 并入上下文的块实时写进 turn buffer（进程中途退出也能恢复），错误与安全闸的拦截记进
/// 日志。发送端全部丢弃后任务结束，把日志交回来（见 [`Runner::reclaim_journal`]）。
fn spawn_turn_logger(
    mut journal: journal::TurnJournal,
) -> (
    mpsc::Sender<AgentEvent>,
    tokio::task::JoinHandle<journal::TurnJournal>,
) {
    let (events_tx, mut events_rx) = mpsc::channel(256);
    let logger = tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            match event {
                AgentEvent::Appended { message } => journal.record(&message),
                AgentEvent::Security {
                    checkpoint,
                    allowed: false,
                    reason,
                } => warn!(?checkpoint, %reason, "security gate blocked this turn"),
                AgentEvent::Error { message } => warn!(%message, "agent error"),
                _ => {}
            }
        }
        journal
    });
    (events_tx, logger)
}

impl Runner {
    /// 从事件任务手里取回回合日志；调用前必须已经丢弃事件的发送端。
    ///
    /// 任务没把日志交回来时（调试构建里它 panic 了；发布构建的 panic 直接结束进程，到不了
    /// 这里）用同一个 turn id 重建：提交时按 seq 覆盖重写，不会重复。
    async fn reclaim_journal(
        &self,
        logger: tokio::task::JoinHandle<journal::TurnJournal>,
        turn_id: String,
    ) -> journal::TurnJournal {
        match logger.await {
            Ok(journal) => journal,
            Err(error) => {
                warn!(%error, "the turn event task failed");
                journal::TurnJournal::new(Arc::clone(&self.deps.turn_buffer), turn_id, 0)
            }
        }
    }

    /// 子代理能用的工具：父注册表里按定义筛出的子集，以实例身份调用。
    fn subagent_tools(
        &self,
        request: &crate::swarm::SubagentRequest,
    ) -> Arc<dyn crate::agent::ToolExecutor> {
        // 以实例身份调用工具：写保护与 monitor 归属都看这个身份。
        let mut tools = self
            .deps
            .tools
            .filtered(&request.tools)
            .for_subagent(request.id.clone());
        if request.local_only {
            tools = tools.without_qq();
        }
        // 嵌套 spawn：只有未到 max_depth 且 hub 已注入时，才给子代理 Agent 工具。
        if request.depth + 1 < request.max_depth
            && let Some(hub) = self.hub.get()
        {
            let handle = Arc::new(crate::tools::agent::HubHandle::new());
            let _ = handle.set(Arc::clone(hub));
            tools.register(Arc::new(crate::tools::agent::AgentTool::new(
                handle,
                request.depth + 1,
            )));
        }
        Arc::new(TurnExecutor {
            registry: Arc::new(tools),
            evidence: request.evidence.clone(),
        })
    }
}

#[async_trait::async_trait]
impl TurnHandler for Runner {
    async fn handle(
        &self,
        input: TurnInput,
        steering: mpsc::UnboundedReceiver<QqMessage>,
    ) -> TurnReport {
        self.execute(input, steering).await
    }
}

#[async_trait::async_trait]
impl crate::swarm::SubagentRunner for Runner {
    /// 运行一个子代理回合。
    ///
    /// 与主回合共享 provider 槽位、回退链与安全闸，但：
    /// - 工具集是父注册表的子集；
    /// - 有自己的历史，不写 transcript / turn buffer；
    /// - 不注入 owner context / USER / memory / skills。
    async fn run_subagent(&self, request: crate::swarm::SubagentRequest) -> Result<String> {
        // 与主回合同样的开关：`sbot ai stop` 与 ESTOP 也停下子代理。
        if !self
            .deps
            .ai_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            anyhow::bail!("AI calls are stopped; the subagent did not run");
        }
        if self.deps.estop_file.exists() {
            anyhow::bail!("ESTOP is active; the subagent did not run");
        }
        let slot = self.provider_snapshot();
        let tools = self.subagent_tools(&request);
        let loop_ = AgentLoop::new_with_fallbacks(
            Arc::clone(&slot.provider),
            self.fallbacks.clone(),
            tools,
            Arc::clone(&self.deps.gate),
            // 子代理：自己的精简 system、没有服务端工具、步数来自定义；其余与主回合相同。
            LoopConfig {
                system: vec![SystemBlock {
                    text: request.system,
                    cache: false,
                }],
                server_tools: Vec::new(),
                max_steps: request.max_steps,
                ..self.loop_config(&slot)
            },
        );
        let mut history = request.history;
        history.push(AgentMessage::user(request.task));
        let stop_reason = run_until_answered(&loop_, &mut history, request.incoming).await;
        if let Some(shared) = request.shared_history {
            *shared.lock().await = history.clone();
        }

        if stop_reason == crate::agent::StopReason::Refusal {
            anyhow::bail!("the subagent request was refused by the provider");
        }
        let text = last_assistant_text(&history);
        if text.trim().is_empty() {
            anyhow::bail!("the subagent returned no text");
        }
        Ok(text)
    }
}

/// 跑子代理的循环，直到运行中送进来的消息都得到了回应；返回最后一次运行的停止原因。
///
/// `incoming` 是 `Agent message` 的收件通道。循环正常结束时通道里可能还有刚送达的消息：
/// 先关闭通道（之后的 `Agent message` 会等这次运行写回再续聊，见 `AgentHub::message`），
/// 再为关闭前已经送达的那些补跑一轮——不能“送达”之后没人理。
async fn run_until_answered(
    loop_: &AgentLoop,
    history: &mut Vec<AgentMessage>,
    mut incoming: Option<mpsc::UnboundedReceiver<AgentMessage>>,
) -> crate::agent::StopReason {
    // 子代理的事件没有订阅者：收下丢掉，只为了让循环的发送不阻塞。
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let mut steering = SteeringQueue::default();
    let mut outcome = loop_
        .run_with_steering(
            history,
            Vec::new(),
            &mut steering,
            incoming
                .as_mut()
                .map(|receiver| receiver as &mut dyn crate::agent::agent_loop::SteeringSource),
            &tx,
        )
        .await;
    let mut leftovers = steering.drain();
    if let Some(receiver) = &mut incoming {
        receiver.close();
        while let Ok(message) = receiver.try_recv() {
            leftovers.push(message);
        }
    }
    if !leftovers.is_empty() {
        if outcome.stop_reason == crate::agent::StopReason::EndTurn {
            outcome = loop_
                .run_with_steering(history, leftovers, &mut SteeringQueue::default(), None, &tx)
                .await;
        } else {
            // 提前结束（出错、超限、被闸拦下）：不再请求模型，消息留在历史里，
            // 下一次续聊时可见。
            history.extend(leftovers);
        }
    }
    drop(tx);
    let _ = drain.await;
    outcome.stop_reason
}

/// 历史里最后一条有文字的助手消息的文本；没有则为空串。
fn last_assistant_text(history: &[AgentMessage]) -> String {
    history
        .iter()
        .rev()
        .find_map(|message| match message {
            AgentMessage::Assistant(assistant) if !assistant.text().trim().is_empty() => {
                Some(assistant.text())
            }
            _ => None,
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicBool;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::agent::{StopReason, Usage};
    use crate::config::{
        AgentMode, EncryptionMode, GateMode, GuardBackend, GuardConfig, GuardMode,
    };
    use crate::providers::{EventReceiver, LlmRequest, Provider, ProviderError, ProviderEvent};
    use crate::qq::event::{Segment, Sender};
    use crate::storage::Database;
    use crate::storage::crypto::MasterKey;
    use crate::tools::api::{AgentOps, ApiTool, NapcatGateway};
    use crate::vfs::quota::Limits;

    struct FakeProvider {
        script: StdMutex<VecDeque<Vec<ProviderEvent>>>,
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn protocol(&self) -> crate::config::models::Protocol {
            crate::config::models::Protocol::Chat
        }

        fn model_id(&self) -> &str {
            "fake"
        }

        async fn stream(&self, _request: LlmRequest) -> Result<EventReceiver, ProviderError> {
            let events = self.script.lock().unwrap().pop_front().unwrap_or_else(|| {
                vec![ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                    refusal: None,
                }]
            });
            let (tx, rx) = mpsc::channel(64);
            tokio::spawn(async move {
                for event in events {
                    let _ = tx.send(event).await;
                }
            });
            Ok(rx)
        }
    }

    #[derive(Default)]
    struct FakeGateway {
        calls: StdMutex<Vec<(String, serde_json::Value)>>,
    }

    #[async_trait]
    impl NapcatGateway for FakeGateway {
        async fn call(
            &self,
            action: &str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.calls.lock().unwrap().push((action.to_owned(), params));
            Ok(json!({"status":"ok","retcode":0,"data":{"message_id":777}}))
        }
    }

    struct NoOps;

    #[async_trait]
    impl AgentOps for NoOps {
        async fn status(&self) -> Result<serde_json::Value, String> {
            Ok(json!({}))
        }
        async fn resume(&self) -> Result<serde_json::Value, String> {
            Ok(json!({}))
        }
        async fn quarantine(
            &self,
            _params: &serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({}))
        }
        async fn compact(&self) -> Result<serde_json::Value, String> {
            Ok(json!({}))
        }
        async fn clear_context(&self) -> Result<serde_json::Value, String> {
            Ok(json!({}))
        }
    }

    fn inbound(chat: ChatId, id: i64, text: &str) -> crate::qq::event::QqMessage {
        crate::qq::event::QqMessage {
            message_id: id,
            chat,
            sender: Sender {
                qq: Some(10001),
                nickname: "Alice".to_owned(),
                card: String::new(),
                role: "member".to_owned(),
            },
            time: chrono::Utc::now(),
            segments: vec![Segment::Text {
                text: text.to_owned(),
            }],
            reply_to: None,
            is_self: false,
            raw: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn turn_runs_tool_and_records_self_message() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([8u8; 32]));
        let vfs = Arc::new(
            VfsStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                Limits::default(),
            )
            .unwrap(),
        );
        let messages = Arc::new(
            MessageStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let sessions = Arc::new(
            SessionStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let turn_buffer = Arc::new(TurnBufferStore::new(
            Arc::clone(&db),
            Arc::clone(&key),
            EncryptionMode::XChaCha20Poly1305,
        ));
        let gate = Arc::new(Gate::from_config(
            &GuardConfig {
                enabled: true,
                mode: GuardMode::Shadow,
                backend: GuardBackend::Off,
                threshold: 0.9,
                ..GuardConfig::default()
            },
            None,
            None,
            crate::security::gate::hold_path(dir.path()),
        ));

        let gateway = Arc::new(FakeGateway::default());
        let mut registry = crate::tools::ToolRegistry::new(Arc::clone(&vfs), None);
        crate::tools::fs::register(&mut registry);
        registry.register(Arc::new(ApiTool::new(
            Some(Arc::clone(&gateway) as Arc<dyn NapcatGateway>),
            Some(Arc::new(NoOps)),
            999,
            999,
            Some(Arc::clone(&gate)),
            vec!["*".to_owned()],
            Some(Arc::clone(&messages)),
        )));
        let tools = Arc::new(registry);

        let script = vec![
            vec![
                ProviderEvent::ToolCallStart {
                    index: 0,
                    id: "c1".to_owned(),
                    name: "API".to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    index: 0,
                    delta: json!({
                        "action": "napcat.send_group_msg",
                        "params": {"group_id": 1, "message": "hello"}
                    })
                    .to_string(),
                },
                ProviderEvent::Stop {
                    reason: StopReason::ToolUse,
                    refusal: None,
                },
            ],
            vec![
                ProviderEvent::TextDelta("done".to_owned()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                    refusal: None,
                },
            ],
        ];
        let provider = Arc::new(FakeProvider {
            script: StdMutex::new(script.into()),
        });

        let history = Arc::new(Mutex::new(Vec::new()));
        let runner = Runner::new(
            provider,
            tools,
            Arc::clone(&gate),
            Arc::clone(&vfs),
            Arc::clone(&messages),
            Arc::clone(&sessions),
            Arc::clone(&turn_buffer),
            None,
            RunnerConfig {
                model: "fake".to_owned(),
                mode: AgentMode::StandIn,
                account_qq: 10000,
                owner_qq: 10000,
                master_qq: 10000,
                owner_name: "Owner".to_owned(),
                receive_scopes: vec!["*".to_owned()],
                send_scopes: vec!["*".to_owned()],
                gate_mode: GateMode::Auto,
                max_tokens: 256,
                supports_images: true,
                thinking: ThinkingOptions::default(),
                max_steps: 8,
                retry: crate::agent::agent_loop::RetryPolicy::default(),
                chat_history_limit: 10,
                context_budget: prompt::context::ContextBudget::default(),
                server_tools: Vec::new(),
                turn_context_budget: 120_000,
                keep_recent_tokens: 20_000,
                compaction_max_failures: 3,
                security_review: None,
                check_every_tokens: 0,
                check_overlap_tokens: 0,
                check_on_compaction: false,
                context_window: 200_000,
                reserve_tokens: 16_384,
                prune: false,
                prune_soft_limit: 4_000,
                prune_keep_turns: 3,
                memory_flush: None,
                post_compact_recall: false,
                compaction: None,
                last_activity: Arc::new(std::sync::atomic::AtomicI64::new(0)),
                budget: None,
                max_state_chars: 32_000,
                event_hooks: Vec::new(),
            },
            Arc::clone(&history),
            Arc::new(AtomicBool::new(true)),
            dir.path().join("ESTOP"),
        )
        .await
        .unwrap();

        let chat = ChatId::Group(1);
        // 生产路径里消息先由事件循环落库；这里复现同一步骤。
        let incoming = inbound(chat, 42, "hi");
        messages.append(&incoming).unwrap();
        let (_tx, rx) = mpsc::unbounded_channel();
        runner
            .handle(
                TurnInput {
                    chat,
                    messages: vec![incoming],
                    dropped: 0,
                },
                rx,
            )
            .await
            .expect_ok();

        // 工具真的调用了网关。锁必须在一个块里释放，后面还有 await。
        {
            let calls = gateway.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, "send_group_msg");
        }

        // 发送被记录为 self。
        let recent = messages.recent(chat, 10).unwrap();
        assert!(
            recent
                .iter()
                .any(|message| message.is_self && message.plain_text() == "hello")
        );
        // 入站消息也在。
        assert!(
            recent
                .iter()
                .any(|message| !message.is_self && message.plain_text() == "hi")
        );

        // transcript 里有 assistant 与 tool 块。
        let current = sessions.current().unwrap();
        let transcript = sessions.read_segment(&current.file).unwrap();
        assert!(
            transcript
                .iter()
                .any(|message| matches!(message, AgentMessage::Assistant(_)))
        );
        assert!(
            transcript
                .iter()
                .any(|message| matches!(message, AgentMessage::Tool(_)))
        );
        // 共享历史与 transcript 一致。
        let history = history.lock().await;
        assert_eq!(history.len(), transcript.len());
    }

    #[tokio::test]
    async fn stopped_ai_skips_the_turn() {
        // 只验证开关分支：没有 provider 调用时不会 panic。
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([8u8; 32]));
        let vfs = Arc::new(
            VfsStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                Limits::default(),
            )
            .unwrap(),
        );
        let messages = Arc::new(
            MessageStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let sessions = Arc::new(
            SessionStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let turn_buffer = Arc::new(TurnBufferStore::new(
            Arc::clone(&db),
            Arc::clone(&key),
            EncryptionMode::XChaCha20Poly1305,
        ));
        let gate = Arc::new(Gate::from_config(
            &GuardConfig {
                enabled: true,
                mode: GuardMode::Shadow,
                backend: GuardBackend::Off,
                threshold: 0.9,
                ..GuardConfig::default()
            },
            None,
            None,
            crate::security::gate::hold_path(dir.path()),
        ));
        let tools = Arc::new(crate::tools::ToolRegistry::new(Arc::clone(&vfs), None));
        let provider = Arc::new(FakeProvider {
            script: StdMutex::new(VecDeque::new()),
        });
        let runner = Runner::new(
            provider,
            tools,
            gate,
            vfs,
            Arc::clone(&messages),
            sessions,
            turn_buffer,
            None,
            RunnerConfig {
                model: "fake".to_owned(),
                mode: AgentMode::StandIn,
                account_qq: 10000,
                owner_qq: 10000,
                master_qq: 10000,
                owner_name: String::new(),
                receive_scopes: vec!["*".to_owned()],
                send_scopes: vec!["*".to_owned()],
                gate_mode: GateMode::Auto,
                max_tokens: 256,
                supports_images: true,
                thinking: ThinkingOptions::default(),
                max_steps: 4,
                retry: crate::agent::agent_loop::RetryPolicy::default(),
                chat_history_limit: 10,
                context_budget: prompt::context::ContextBudget::default(),
                server_tools: Vec::new(),
                turn_context_budget: 120_000,
                keep_recent_tokens: 20_000,
                compaction_max_failures: 3,
                security_review: None,
                check_every_tokens: 0,
                check_overlap_tokens: 0,
                check_on_compaction: false,
                context_window: 200_000,
                reserve_tokens: 16_384,
                prune: false,
                prune_soft_limit: 4_000,
                prune_keep_turns: 3,
                memory_flush: None,
                post_compact_recall: false,
                compaction: None,
                last_activity: Arc::new(std::sync::atomic::AtomicI64::new(0)),
                budget: None,
                max_state_chars: 32_000,
                event_hooks: Vec::new(),
            },
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(AtomicBool::new(false)),
            dir.path().join("ESTOP"),
        )
        .await
        .unwrap();

        let chat = ChatId::Group(1);
        let (_tx, rx) = mpsc::unbounded_channel();
        runner
            .handle(
                TurnInput {
                    chat,
                    messages: vec![inbound(chat, 1, "hi")],
                    dropped: 0,
                },
                rx,
            )
            .await
            .expect_ok();
        // 回合被跳过：没有 assistant 块。
        let current = runner.deps.sessions.current().unwrap();
        let transcript = runner.deps.sessions.read_segment(&current.file).unwrap();
        assert!(
            !transcript
                .iter()
                .any(|message| matches!(message, AgentMessage::Assistant(_)))
        );
        let _ = Usage::default();
        let _ = messages.usage().unwrap();
    }

    #[tokio::test]
    async fn skipped_turn_returns_steering_messages_to_the_queue() {
        let (runner, _gate, dir) = runner_with(None, Vec::new(), 120_000, 20_000).await;
        // ESTOP 让回合在开始前就被跳过。
        std::fs::write(dir.path().join("ESTOP"), b"").unwrap();

        let chat = ChatId::Group(1);
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(inbound(chat, 2, "steering survives")).unwrap();
        let unanswered = runner
            .handle(
                TurnInput {
                    chat,
                    messages: vec![inbound(chat, 1, "hi")],
                    dropped: 0,
                },
                rx,
            )
            .await
            .expect_ok();

        // 被跳过的回合不回应 steering 消息：交还队列重新排队，自己什么都不写。
        assert_eq!(unanswered, 1);
        let sessions = Arc::clone(&runner.deps.sessions);
        let current = sessions.current().unwrap();
        let transcript = sessions.read_segment(&current.file).unwrap();
        assert!(
            !transcript.iter().any(|message| matches!(
                message,
                AgentMessage::User { text, .. } if text.contains("steering survives")
            )),
            "a skipped turn must not write the steering message: {transcript:?}"
        );
        let history = Arc::clone(&runner.history);
        let history = history.lock().await;
        assert!(
            !history.iter().any(|message| matches!(
                message,
                AgentMessage::User { text, .. } if text.contains("steering survives")
            )),
            "a skipped turn must not add the steering message to history: {history:?}"
        );
    }

    /// 回合正常结束后才到的 steering 消息不能被“收下却不回应”：
    /// 模型循环一结束 steering 就关闭，之后的发送失败、留在队列里。
    #[tokio::test]
    async fn steering_closes_as_soon_as_the_model_loop_ends() {
        let (runner, _gate, _dir) = runner_with(
            None,
            vec![vec![
                ProviderEvent::TextDelta("ok".to_owned()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                    refusal: None,
                },
            ]],
            120_000,
            20_000,
        )
        .await;
        let chat = ChatId::Group(1);
        let (tx, rx) = mpsc::unbounded_channel();
        let unanswered = runner
            .handle(
                TurnInput {
                    chat,
                    messages: vec![inbound(chat, 1, "hi")],
                    dropped: 0,
                },
                rx,
            )
            .await
            .expect_ok();
        assert_eq!(unanswered, 0);
        // 回合结束后发送一定失败：队列据此把消息留在 pending。
        assert!(tx.send(inbound(chat, 2, "too late")).is_err());
    }

    /// 回合提前结束时，循环已经取出、但还没进入上下文的 steering 消息交还队列。
    #[tokio::test]
    async fn early_exit_returns_waiting_steering_messages() {
        let (runner, _gate, _dir) = runner_with(
            None,
            vec![vec![
                ProviderEvent::ToolCallStart {
                    index: 0,
                    id: "c1".to_owned(),
                    name: "Read".to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    index: 0,
                    delta: "{\"path\":".to_owned(),
                },
                // 参数被截断：循环不执行工具、直接结束本回合。
                ProviderEvent::Stop {
                    reason: StopReason::MaxTokens,
                    refusal: None,
                },
            ]],
            120_000,
            20_000,
        )
        .await;
        let chat = ChatId::Group(1);
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(inbound(chat, 2, "are you there?")).unwrap();
        let unanswered = runner
            .handle(
                TurnInput {
                    chat,
                    messages: vec![inbound(chat, 1, "hi")],
                    dropped: 0,
                },
                rx,
            )
            .await
            .expect_ok();
        assert_eq!(unanswered, 1);
        let history = Arc::clone(&runner.history);
        let history = history.lock().await;
        assert!(
            !history.iter().any(|message| matches!(
                message,
                AgentMessage::User { text, .. } if text.contains("are you there")
            )),
            "an unanswered steering message is re-queued, not appended: {history:?}"
        );
    }

    #[tokio::test]
    async fn compact_now_summarizes_and_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([8u8; 32]));
        let vfs = Arc::new(
            VfsStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                Limits::default(),
            )
            .unwrap(),
        );
        let messages = Arc::new(
            MessageStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let sessions = Arc::new(
            SessionStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let turn_buffer = Arc::new(TurnBufferStore::new(
            Arc::clone(&db),
            Arc::clone(&key),
            EncryptionMode::XChaCha20Poly1305,
        ));
        let gate = Arc::new(Gate::from_config(
            &GuardConfig {
                enabled: true,
                mode: GuardMode::Shadow,
                backend: GuardBackend::Off,
                threshold: 0.9,
                ..GuardConfig::default()
            },
            None,
            None,
            crate::security::gate::hold_path(dir.path()),
        ));
        let tools = Arc::new(crate::tools::ToolRegistry::new(Arc::clone(&vfs), None));
        // 两次摘要请求：第一次压缩在落盘时失败，第二次成功。
        let summary_reply = || {
            vec![
                ProviderEvent::TextDelta("## Facts\ncompacted".to_owned()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                    refusal: None,
                },
            ]
        };
        let provider = Arc::new(FakeProvider {
            script: StdMutex::new(vec![summary_reply(), summary_reply()].into()),
        });
        let history = Arc::new(Mutex::new(Vec::new()));
        let runner = Runner::new(
            provider,
            tools,
            gate,
            vfs,
            messages,
            Arc::clone(&sessions),
            turn_buffer,
            None,
            RunnerConfig {
                model: "fake".to_owned(),
                mode: AgentMode::StandIn,
                account_qq: 10000,
                owner_qq: 10000,
                master_qq: 10000,
                owner_name: String::new(),
                receive_scopes: vec!["*".to_owned()],
                send_scopes: vec!["*".to_owned()],
                gate_mode: GateMode::Auto,
                max_tokens: 256,
                supports_images: true,
                thinking: ThinkingOptions::default(),
                max_steps: 4,
                retry: crate::agent::agent_loop::RetryPolicy::default(),
                chat_history_limit: 10,
                context_budget: prompt::context::ContextBudget::default(),
                server_tools: Vec::new(),
                turn_context_budget: 120_000,
                keep_recent_tokens: 5,
                compaction_max_failures: 3,
                security_review: None,
                check_every_tokens: 0,
                check_overlap_tokens: 0,
                check_on_compaction: false,
                context_window: 200_000,
                reserve_tokens: 16_384,
                prune: false,
                prune_soft_limit: 4_000,
                prune_keep_turns: 3,
                memory_flush: None,
                post_compact_recall: false,
                compaction: None,
                last_activity: Arc::new(std::sync::atomic::AtomicI64::new(0)),
                budget: None,
                max_state_chars: 32_000,
                event_hooks: Vec::new(),
            },
            Arc::clone(&history),
            Arc::new(AtomicBool::new(true)),
            dir.path().join("ESTOP"),
        )
        .await
        .unwrap();

        {
            let mut shared = history.lock().await;
            for index in 0..20 {
                shared.push(AgentMessage::user(format!("old message {index}")));
            }
            // 最近的一条回复带着压缩前那段大提示的用量。
            shared.push(AgentMessage::Assistant(crate::agent::AssistantMessage {
                content: vec![crate::agent::AssistantBlock::Text {
                    text: "recent reply".to_owned(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 150_000,
                    output_tokens: 10,
                    ..Usage::default()
                },
                refusal: None,
            }));
        }
        let previous = sessions.current().unwrap();

        // 摘要做完了、新分段登记不上：这次压缩失败，当前分段与内存里的历史都必须原样不动
        // （否则旧分段已经关闭、当前分段是空的，进程重启后上下文从空分段恢复）。
        let before = history.lock().await.clone();
        db.lock()
            .execute_batch(
                "CREATE TRIGGER fail_insert BEFORE INSERT ON transcript_files
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        let error = runner.compact_now("test").await.unwrap_err();
        assert!(
            format!("{error:#}").contains("injected failure"),
            "{error:#}"
        );
        db.lock().execute_batch("DROP TRIGGER fail_insert").unwrap();
        let unchanged = sessions.current().unwrap();
        assert_eq!(unchanged.file, previous.file);
        assert!(unchanged.ended_at.is_none());
        assert_eq!(*history.lock().await, before);

        // 自动压缩已经因为连续失败停掉时，一次成功的手动压缩让它恢复。
        let failures = &runner.compaction_failures;
        failures.store(3, std::sync::atomic::Ordering::Relaxed);
        let value = runner.compact_now("test").await.unwrap();
        assert_eq!(value["compacted"], true);
        assert_eq!(value["previous_session"], previous.file);
        assert_eq!(failures.load(std::sync::atomic::Ordering::Relaxed), 0);
        let current = sessions.current().unwrap();
        assert_ne!(current.file, previous.file);
        let new_history = history.lock().await;
        match &new_history[0] {
            AgentMessage::System { text } => {
                assert!(text.contains("previous_session=/sessions/"));
                assert!(text.contains("compacted"));
            }
            other => panic!("expected a summary system message, got {other:?}"),
        }
        assert!(new_history.len() < 22);
        // 保留下来的回复不再带压缩前的用量：否则估算仍然超限，下次唤醒又会压缩。
        assert!(new_history.iter().any(|message| matches!(
            message,
            AgentMessage::Assistant(assistant) if assistant.text() == "recent reply"
        )));
        assert!(new_history.iter().all(|message| match message {
            AgentMessage::Assistant(assistant) => assistant.usage == Usage::default(),
            _ => true,
        }));
        assert!(crate::session::compaction::estimate_messages(&new_history) < 10_000);
        // 新分段里落盘的就是内存里的新历史：进程重启后恢复出的上下文与现在相同。
        assert_eq!(sessions.read_segment(&current.file).unwrap(), *new_history);
        assert_eq!(current.message_count, new_history.len() as u64);
    }

    #[tokio::test]
    async fn automatic_compaction_keeps_the_turn_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([9u8; 32]));
        let vfs = Arc::new(
            VfsStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                Limits::default(),
            )
            .unwrap(),
        );
        let messages = Arc::new(
            MessageStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let sessions = Arc::new(
            SessionStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let turn_buffer = Arc::new(TurnBufferStore::new(
            Arc::clone(&db),
            Arc::clone(&key),
            EncryptionMode::XChaCha20Poly1305,
        ));
        let gate = Arc::new(Gate::from_config(
            &GuardConfig {
                enabled: true,
                mode: GuardMode::Shadow,
                backend: GuardBackend::Off,
                threshold: 0.9,
                ..GuardConfig::default()
            },
            None,
            None,
            crate::security::gate::hold_path(dir.path()),
        ));
        let tools = Arc::new(crate::tools::ToolRegistry::new(Arc::clone(&vfs), None));
        let provider = Arc::new(FakeProvider {
            script: StdMutex::new(
                vec![
                    vec![
                        ProviderEvent::TextDelta("## Active chats\nsummary".to_owned()),
                        ProviderEvent::Stop {
                            reason: StopReason::EndTurn,
                            refusal: None,
                        },
                    ],
                    vec![
                        ProviderEvent::TextDelta("fresh reply".to_owned()),
                        ProviderEvent::Stop {
                            reason: StopReason::EndTurn,
                            refusal: None,
                        },
                    ],
                ]
                .into(),
            ),
        });

        let history = Arc::new(Mutex::new(Vec::new()));
        {
            let mut preloaded = history.lock().await;
            for index in 0..6 {
                preloaded.push(AgentMessage::user(format!("old context line {index}")));
            }
        }
        let runner = Runner::new(
            provider,
            tools,
            Arc::clone(&gate),
            Arc::clone(&vfs),
            Arc::clone(&messages),
            Arc::clone(&sessions),
            Arc::clone(&turn_buffer),
            None,
            RunnerConfig {
                model: "fake".to_owned(),
                mode: AgentMode::StandIn,
                account_qq: 10000,
                owner_qq: 10000,
                master_qq: 10000,
                owner_name: "Owner".to_owned(),
                receive_scopes: vec!["*".to_owned()],
                send_scopes: vec!["*".to_owned()],
                gate_mode: GateMode::Auto,
                max_tokens: 256,
                supports_images: true,
                thinking: ThinkingOptions::default(),
                max_steps: 4,
                retry: crate::agent::agent_loop::RetryPolicy::default(),
                chat_history_limit: 10,
                context_budget: prompt::context::ContextBudget::default(),
                server_tools: Vec::new(),
                // 故意压到很小：任何真实上下文都会触发自动压缩。
                turn_context_budget: 2,
                keep_recent_tokens: 1,
                compaction_max_failures: 3,
                security_review: None,
                check_every_tokens: 0,
                check_overlap_tokens: 0,
                check_on_compaction: false,
                context_window: 200_000,
                reserve_tokens: 16_384,
                prune: false,
                prune_soft_limit: 4_000,
                prune_keep_turns: 3,
                memory_flush: None,
                post_compact_recall: false,
                compaction: None,
                last_activity: Arc::new(std::sync::atomic::AtomicI64::new(0)),
                budget: None,
                max_state_chars: 32_000,
                event_hooks: Vec::new(),
            },
            Arc::clone(&history),
            Arc::new(AtomicBool::new(true)),
            dir.path().join("ESTOP"),
        )
        .await
        .unwrap();

        let chat = ChatId::Group(1);
        let incoming = inbound(chat, 7, "new message");
        messages.append(&incoming).unwrap();
        let (_tx, rx) = mpsc::unbounded_channel();
        runner
            .handle(
                TurnInput {
                    chat,
                    messages: vec![incoming],
                    dropped: 0,
                },
                rx,
            )
            .await
            .expect_ok();

        let committed = history.lock().await.clone();
        // 压缩后历史从摘要块开始，且本回合的回复被提交（不会因 base 错位而 panic/丢块）。
        assert!(matches!(
            committed.first(),
            Some(AgentMessage::System { .. })
        ));
        assert!(committed.iter().any(|message| matches!(
            message,
            AgentMessage::Assistant(assistant)
                if assistant.content.iter().any(|block| matches!(
                    block, crate::agent::message::AssistantBlock::Text { text } if text.contains("fresh reply")
                ))
        )));
        // 摘要 + 保留的最近消息 + 本回合回复，旧的 6 条已被压缩掉。
        assert!(committed.len() <= 5, "unexpected history: {committed:?}");
        assert!(sessions.list().unwrap().len() >= 2);
    }

    struct FixedReview(crate::security::review::ReviewOutcome);

    #[async_trait::async_trait]
    impl crate::security::review::SecurityCheck for FixedReview {
        async fn review(
            &self,
            _window: &str,
        ) -> Result<crate::security::review::ReviewOutcome, String> {
            Ok(self.0.clone())
        }
    }

    async fn runner_with_review(
        outcome: crate::security::review::ReviewOutcome,
    ) -> (Arc<Runner>, Arc<Gate>, tempfile::TempDir) {
        runner_with(
            Some(outcome),
            vec![vec![
                ProviderEvent::TextDelta("ok".to_owned()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                    refusal: None,
                },
            ]],
            120_000,
            20_000,
        )
        .await
    }

    async fn runner_with(
        outcome: Option<crate::security::review::ReviewOutcome>,
        script: Vec<Vec<ProviderEvent>>,
        turn_context_budget: u32,
        keep_recent_tokens: u32,
    ) -> (Arc<Runner>, Arc<Gate>, tempfile::TempDir) {
        runner_with_hooks(
            outcome,
            script,
            turn_context_budget,
            keep_recent_tokens,
            Vec::new(),
            None,
        )
        .await
    }

    /// [`runner_with`] 加上事件 hook，以及一个可选的额外工具（测试用它观察回合内部的状态）。
    async fn runner_with_hooks(
        outcome: Option<crate::security::review::ReviewOutcome>,
        script: Vec<Vec<ProviderEvent>>,
        turn_context_budget: u32,
        keep_recent_tokens: u32,
        event_hooks: Vec<crate::config::EventHookConfig>,
        extra_tool: Option<Arc<dyn crate::tools::Tool>>,
    ) -> (Arc<Runner>, Arc<Gate>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([11u8; 32]));
        let vfs = Arc::new(
            VfsStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                Limits::default(),
            )
            .unwrap(),
        );
        let messages = Arc::new(
            MessageStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let sessions = Arc::new(
            SessionStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let turn_buffer = Arc::new(TurnBufferStore::new(
            Arc::clone(&db),
            Arc::clone(&key),
            EncryptionMode::XChaCha20Poly1305,
        ));
        let gate = Arc::new(Gate::from_config(
            &GuardConfig {
                enabled: true,
                mode: GuardMode::Shadow,
                backend: GuardBackend::Off,
                threshold: 0.9,
                ..GuardConfig::default()
            },
            None,
            Some(Arc::clone(&db)),
            crate::security::gate::hold_path(dir.path()),
        ));
        let has_review = outcome.is_some();
        let provider = Arc::new(FakeProvider {
            script: StdMutex::new(script.into()),
        });
        let mut registry = crate::tools::ToolRegistry::new(Arc::clone(&vfs), None);
        if let Some(tool) = extra_tool {
            registry.register(tool);
        }
        let tools = Arc::new(registry);
        let history = Arc::new(Mutex::new(Vec::new()));
        let runner = Runner::new(
            provider,
            tools,
            Arc::clone(&gate),
            Arc::clone(&vfs),
            Arc::clone(&messages),
            Arc::clone(&sessions),
            Arc::clone(&turn_buffer),
            None,
            RunnerConfig {
                model: "fake".to_owned(),
                mode: AgentMode::StandIn,
                account_qq: 10000,
                owner_qq: 10000,
                master_qq: 10000,
                owner_name: String::new(),
                receive_scopes: vec!["*".to_owned()],
                send_scopes: vec!["*".to_owned()],
                gate_mode: GateMode::Auto,
                max_tokens: 256,
                supports_images: true,
                thinking: ThinkingOptions::default(),
                max_steps: 4,
                retry: crate::agent::agent_loop::RetryPolicy::default(),
                chat_history_limit: 10,
                context_budget: prompt::context::ContextBudget::default(),
                server_tools: Vec::new(),
                turn_context_budget,
                keep_recent_tokens,
                compaction_max_failures: 3,
                security_review: outcome.map(|outcome| {
                    Arc::new(FixedReview(outcome))
                        as Arc<dyn crate::security::review::SecurityCheck>
                }),
                check_every_tokens: if has_review { 1 } else { 0 },
                check_overlap_tokens: 0,
                check_on_compaction: false,
                context_window: 200_000,
                reserve_tokens: 16_384,
                prune: false,
                prune_soft_limit: 4_000,
                prune_keep_turns: 3,
                memory_flush: None,
                post_compact_recall: false,
                compaction: None,
                last_activity: Arc::new(std::sync::atomic::AtomicI64::new(0)),
                budget: None,
                max_state_chars: 32_000,
                event_hooks,
            },
            Arc::clone(&history),
            Arc::new(AtomicBool::new(true)),
            dir.path().join("ESTOP"),
        )
        .await
        .unwrap();
        (runner, gate, dir)
    }

    async fn one_turn(runner: &Arc<Runner>, chat: ChatId) {
        let incoming = inbound(chat, 1, "hello");
        let (_tx, rx) = mpsc::unbounded_channel();
        runner
            .handle(
                TurnInput {
                    chat,
                    messages: vec![incoming],
                    dropped: 0,
                },
                rx,
            )
            .await
            .expect_ok();
    }

    #[tokio::test]
    async fn high_risk_plus_compliance_enters_hold() {
        let (runner, gate, _dir) = runner_with_review(crate::security::review::ReviewOutcome {
            risk: crate::security::review::Risk::High,
            complied: true,
            categories: vec!["injection".to_owned()],
            findings: vec!["followed a rule change".to_owned()],
        })
        .await;
        one_turn(&runner, ChatId::Group(1)).await;
        assert!(gate.hold_active());
    }

    /// 无人值守：`critical` 不能把 agent 永久停住。hold 关掉 QQ 出口，被污染的上下文在
    /// 下一回合开始前丢弃；回合照常运行，agent 醒来后可以自己复核并解除 hold。
    #[tokio::test]
    async fn critical_risk_holds_and_discards_the_context_before_the_next_turn() {
        let (runner, gate, _dir) = runner_with_review(crate::security::review::ReviewOutcome {
            risk: crate::security::review::Risk::Critical,
            complied: false,
            categories: vec!["exfiltration".to_owned()],
            findings: Vec::new(),
        })
        .await;
        one_turn(&runner, ChatId::Group(1)).await;
        assert!(gate.hold_active());
        let poisoned = runner.deps.sessions.current().unwrap().file;
        assert!(!runner.history.lock().await.is_empty());

        // 下一回合照常运行，但先换到新分段；旧分段留档。
        one_turn(&runner, ChatId::Group(1)).await;
        let fresh = runner.deps.sessions.current().unwrap().file;
        assert_ne!(poisoned, fresh);
        assert!(
            !runner
                .deps
                .sessions
                .read_segment(&poisoned)
                .unwrap()
                .is_empty()
        );
        let history = Arc::clone(&runner.history);
        let history = history.lock().await;
        assert!(
            !history
                .iter()
                .any(|message| matches!(message, AgentMessage::Assistant(a) if a.text() == "ok")),
            "the first turn's reply must not survive the discard: {history:?}"
        );
        // hold 仍在：解除是 agent 自己的事（`agent.resume`）。
        assert!(gate.hold_active());
    }

    /// `session_start` 的 `inject_context` 从启动起就在 system 里，压缩之后也还在。以前启动时
    /// 的 system 不含它，要等到第一次压缩才出现：配置写了、日志也记了，模型却看不到。
    #[tokio::test]
    async fn session_start_context_is_in_the_system_from_startup() {
        let hook = crate::config::EventHookConfig {
            event: crate::config::HookEvent::SessionStart,
            action: crate::config::HookAction::InjectContext,
            text: "house rule: marker-7461".to_owned(),
            ..Default::default()
        };
        let (runner, _gate, _dir) =
            runner_with_hooks(None, Vec::new(), 120_000, 20_000, vec![hook], None).await;
        let injected = |runner: &Runner| {
            runner
                .system
                .read()
                .unwrap()
                .iter()
                .filter(|block| block.text.contains("marker-7461"))
                .count()
        };
        assert_eq!(injected(&runner), 1);
        // 静态提示词仍是第一块，而且是唯一带缓存断点的。
        {
            let system = runner.system.read().unwrap();
            assert!(system[0].cache);
            assert!(system[1..].iter().all(|block| !block.cache));
        }
        // 重建不会把它加第二遍。
        runner.rebuild_system();
        assert_eq!(injected(&runner), 1);
    }

    /// 探针工具：每次被调用时记下这个回合带不带 owner intent。
    struct IntentProbe(Arc<StdMutex<Vec<bool>>>);

    #[async_trait]
    impl crate::tools::Tool for IntentProbe {
        fn name(&self) -> &'static str {
            "Probe"
        }
        fn description(&self) -> &'static str {
            "Records whether the current turn carries owner intent."
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(
            &self,
            ctx: &crate::tools::ToolContext,
            _args: serde_json::Value,
        ) -> crate::transcript::ToolResult {
            self.0.lock().unwrap().push(ctx.evidence.owner_intent());
            crate::transcript::ToolResult::ok("recorded")
        }
    }

    /// 主人的消息触发的回合带 owner intent；同一条消息被 `message_received` 的 `block` 拦下
    /// 时不带——它没进上下文，模型不知道主人说了什么。
    #[tokio::test]
    async fn a_blocked_batch_carries_no_owner_intent() {
        let chat = ChatId::Group(1);
        let block = crate::config::EventHookConfig {
            event: crate::config::HookEvent::MessageReceived,
            action: crate::config::HookAction::Block,
            reason: "quiet hours".to_owned(),
            match_: crate::config::HookMatch {
                group_id: Some(1),
                ..Default::default()
            },
            ..Default::default()
        };
        for (hooks, expected) in [(Vec::new(), true), (vec![block], false)] {
            let script = vec![
                vec![
                    ProviderEvent::ToolCallStart {
                        index: 0,
                        id: "c1".to_owned(),
                        name: "Probe".to_owned(),
                    },
                    ProviderEvent::ToolCallArgumentsDelta {
                        index: 0,
                        delta: "{}".to_owned(),
                    },
                    ProviderEvent::Stop {
                        reason: StopReason::ToolUse,
                        refusal: None,
                    },
                ],
                vec![
                    ProviderEvent::TextDelta("done".to_owned()),
                    ProviderEvent::Stop {
                        reason: StopReason::EndTurn,
                        refusal: None,
                    },
                ],
            ];
            let seen = Arc::new(StdMutex::new(Vec::new()));
            let (runner, _gate, _dir) = runner_with_hooks(
                None,
                script,
                120_000,
                20_000,
                hooks,
                Some(Arc::new(IntentProbe(Arc::clone(&seen)))),
            )
            .await;
            // 测试里的主人就是 10000（`runner_with_hooks` 的 `owner_qq`）。
            let mut from_owner = inbound(chat, 1, "rename the group");
            from_owner.sender.qq = Some(10000);
            let (_tx, rx) = mpsc::unbounded_channel();
            runner
                .handle(
                    TurnInput {
                        chat,
                        messages: vec![from_owner],
                        dropped: 0,
                    },
                    rx,
                )
                .await
                .expect_ok();
            assert_eq!(*seen.lock().unwrap(), vec![expected]);
        }
    }

    /// 一个只回 `ok` 的 runner，不带周期审查。
    async fn plain_runner() -> (Arc<Runner>, tempfile::TempDir) {
        let reply = || {
            vec![
                ProviderEvent::TextDelta("ok".to_owned()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                    refusal: None,
                },
            ]
        };
        let (runner, _gate, dir) = runner_with(None, vec![reply(), reply()], 120_000, 20_000).await;
        (runner, dir)
    }

    /// 让 transcript 的索引更新在消息数到达 `count` 时失败。
    fn fail_transcript_at(runner: &Runner, count: u32) {
        runner
            .deps
            .sessions
            .database()
            .lock()
            .execute_batch(&format!(
                "CREATE TRIGGER fail_update BEFORE UPDATE ON transcript_files
                 WHEN NEW.message_count >= {count}
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;"
            ))
            .unwrap();
    }

    /// 当前分段里落盘的消息。
    fn transcript(runner: &Runner) -> Vec<AgentMessage> {
        let sessions = &runner.deps.sessions;
        sessions
            .read_segment(&sessions.current().unwrap().file)
            .unwrap()
    }

    /// 一批入站消息写 transcript 写到一半失败：回合报错，transcript 与共享历史里都没有这批
    /// 消息的任何一条。只进了其中一边的话，运行中的上下文与重启后恢复出来的上下文就不一样。
    #[tokio::test]
    async fn an_inbound_batch_enters_the_transcript_and_the_history_together() {
        let (runner, _dir) = plain_runner().await;
        let chat = ChatId::Group(1);
        // 第一条消息能写进去，第二条写不进去。
        fail_transcript_at(&runner, 2);
        let (_tx, rx) = mpsc::unbounded_channel();
        let report = runner
            .handle(
                TurnInput {
                    chat,
                    messages: vec![inbound(chat, 1, "first"), inbound(chat, 2, "second")],
                    dropped: 0,
                },
                rx,
            )
            .await;
        let error = report.error.expect("the turn fails");
        assert!(
            format!("{error:#}").contains("injected failure"),
            "{error:#}"
        );
        assert!(transcript(&runner).is_empty(), "{:?}", transcript(&runner));
        assert!(runner.history.lock().await.is_empty());

        // 故障排除后同一批消息照常处理，两边一致。
        runner
            .deps
            .sessions
            .database()
            .lock()
            .execute_batch("DROP TRIGGER fail_update")
            .unwrap();
        let (_tx, rx) = mpsc::unbounded_channel();
        runner
            .handle(
                TurnInput {
                    chat,
                    messages: vec![inbound(chat, 1, "first"), inbound(chat, 2, "second")],
                    dropped: 0,
                },
                rx,
            )
            .await
            .expect_ok();
        let history = runner.history.lock().await.clone();
        assert_eq!(history.len(), 3, "{history:?}");
        assert_eq!(transcript(&runner), history);
    }

    /// monitor 投递（控制通道同理）的消息写不进 transcript：回合报错，共享历史里也没有它。
    #[tokio::test]
    async fn an_injected_message_enters_the_transcript_and_the_history_together() {
        let (runner, _dir) = plain_runner().await;
        fail_transcript_at(&runner, 1);
        let error = runner
            .run_event("probe", "timer", "tick", false)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("injected failure"),
            "{error:#}"
        );
        assert!(transcript(&runner).is_empty(), "{:?}", transcript(&runner));
        assert!(runner.history.lock().await.is_empty());

        runner
            .deps
            .sessions
            .database()
            .lock()
            .execute_batch("DROP TRIGGER fail_update")
            .unwrap();
        runner
            .run_event("probe", "timer", "tick", false)
            .await
            .unwrap();
        let history = runner.history.lock().await.clone();
        assert_eq!(history.len(), 2, "{history:?}");
        assert_eq!(transcript(&runner), history);
    }

    #[tokio::test]
    async fn soak_many_turns_stay_bounded_with_compaction() {
        let mut script = Vec::new();
        for _ in 0..2_000 {
            script.push(vec![
                ProviderEvent::TextDelta("ok".to_owned()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                    refusal: None,
                },
            ]);
        }
        let (runner, _gate, _dir) = runner_with(None, script, 6_000, 1_500).await;
        for id in 1..=300 {
            let chat = ChatId::Group(1);
            let incoming = inbound(chat, id, "hello");
            let (_tx, rx) = mpsc::unbounded_channel();
            runner
                .handle(
                    TurnInput {
                        chat,
                        messages: vec![incoming],
                        dropped: 0,
                    },
                    rx,
                )
                .await
                .expect_ok();
        }
        let history_handle = Arc::clone(&runner.history);
        let history = history_handle.lock().await;
        assert!(!history.is_empty());
        assert!(
            history.len() <= 400,
            "history grew to {} messages despite compaction",
            history.len()
        );
    }
}
