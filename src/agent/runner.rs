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
use tracing::{debug, warn};

use crate::agent::queue::{TurnHandler, TurnInput};
use crate::agent::{
    AgentEvent, AgentLoop, AgentMessage, FallbackProvider, LoopConfig, LoopOutcome, SteeringQueue,
    TurnEvidence,
};
use crate::config::{AgentMode, GateMode};
use crate::prompt::{self, RuntimeContext};
use crate::providers::{Provider, SystemBlock, ThinkingOptions};
use crate::qq::event::ChatId;
use crate::qq::{QqClient, format};
use crate::security::gate::Gate;
use crate::session::SessionStore;
use crate::session::turn_buffer::{TurnBufferStore, new_turn_id};
use crate::storage::Database;
use crate::storage::messages::MessageStore;
use crate::tools::ToolRegistry;
use crate::vfs::Vfs;
use crate::vfs::sqlite::VfsStore;

/// 运行配置。
#[derive(Clone)]
pub struct RunnerConfig {
    pub model: String,
    pub mode: AgentMode,
    pub account_qq: u64,
    pub owner_qq: u64,
    pub master_qq: u64,
    pub owner_name: String,
    pub session_scope: String,
    pub receive_scopes: Vec<String>,
    pub send_scopes: Vec<String>,
    pub gate_mode: GateMode,
    pub max_tokens: u32,
    pub thinking: ThinkingOptions,
    pub max_steps: u32,
    pub retry: crate::agent::agent_loop::RetryPolicy,
    /// 注入“上次回复以来的群消息”时最多取多少条。
    pub chat_history_limit: u32,
    /// context hooks（AGENTS/USER/MEMORY/STYLE 或自定义）。
    pub hooks: Vec<crate::config::ContextHookConfig>,
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
    /// 模型上下文窗口；与 `turn_context_budget` 一起决定压缩阈值。
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
}

/// 摘要用的独立 provider 与模型 id。
pub struct CompactionModel {
    pub provider: Arc<dyn Provider>,
    pub model: String,
}

/// 当前生效的 provider 快照。
///
/// `sbot model set` / `sbot effort set` 通过共享的 `RwLock` 原地替换，
/// 下一回合立即生效；正在进行的回合继续使用它开始时的快照。
#[derive(Clone)]
pub struct ProviderSlot {
    /// `provider/id` 形式的引用，用于日志与运行时上下文。
    pub reference: String,
    /// 发给 provider 的 API model id。
    pub api_model: String,
    pub provider: Arc<dyn Provider>,
    pub max_tokens: u32,
    pub thinking: ThinkingOptions,
}

/// 共享的 provider 槽位。
pub type SharedProviderSlot = Arc<std::sync::RwLock<ProviderSlot>>;

/// per_chat 的会话与历史缓存。
///
/// 这些状态原本散在 `Runner` 的五个字段里；收进一个结构后，`Runner` 只保留
/// 真正的运行时句柄，`clear_all_context` 与测试也只需面对一个入口。
struct ScopeCache {
    per_chat: bool,
    sessions: Mutex<HashMap<String, Arc<SessionStore>>>,
    histories: Mutex<HashMap<String, Arc<Mutex<Vec<AgentMessage>>>>>,
}

impl ScopeCache {
    fn new(per_chat: bool) -> Self {
        Self {
            per_chat,
            sessions: Mutex::new(HashMap::new()),
            histories: Mutex::new(HashMap::new()),
        }
    }

    async fn session_for(&self, scope: &str, base: &Arc<SessionStore>) -> Arc<SessionStore> {
        let mut sessions = self.sessions.lock().await;
        if let Some(store) = sessions.get(scope) {
            return Arc::clone(store);
        }
        let store = Arc::new(base.for_scope(scope));
        sessions.insert(scope.to_owned(), Arc::clone(&store));
        store
    }

    async fn history_for(
        &self,
        scope: &str,
        base: &Arc<SessionStore>,
    ) -> Arc<Mutex<Vec<AgentMessage>>> {
        let mut histories = self.histories.lock().await;
        if let Some(history) = histories.get(scope) {
            return Arc::clone(history);
        }
        let sessions = Arc::clone(base);
        let sessions = sessions.for_scope(scope);
        let messages = sessions
            .current()
            .and_then(|current| sessions.read_segment(&current.file))
            .unwrap_or_default();
        let history = Arc::new(Mutex::new(messages));
        histories.insert(scope.to_owned(), Arc::clone(&history));
        history
    }

    async fn all_sessions(&self) -> Vec<Arc<SessionStore>> {
        self.sessions.lock().await.values().cloned().collect()
    }

    async fn clear_histories(&self) {
        for history in self.histories.lock().await.values() {
            history.lock().await.clear();
        }
    }
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
    ai_enabled: Arc<std::sync::atomic::AtomicBool>,
    /// `ESTOP` 哨兵路径；存在时暂停新回合。
    estop_file: std::path::PathBuf,
    tools: Arc<ToolRegistry>,
    gate: Arc<Gate>,
    vfs: Arc<VfsStore>,
    messages: Arc<MessageStore>,
    sessions: Arc<SessionStore>,
    turn_buffer: Arc<TurnBufferStore>,
    qq: Option<Arc<QqClient>>,
    config: RunnerConfig,
    /// 已提交的全局 transcript（内存镜像）。
    history: Arc<Mutex<Vec<AgentMessage>>>,
    /// per_chat 的会话链与历史缓存；shared 模式不持有任何 per-chat 状态。
    scopes: ScopeCache,
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
    /// 周期检查互斥。
    review_lock: Mutex<()>,
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

impl Runner {
    /// 不带回退链构造。
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
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
    pub async fn new_with_fallbacks(
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
        let per_chat = config.session_scope == "per_chat";
        // 恢复崩溃前未提交的回合，再载入当前分段。
        if !per_chat {
            let recovered = turn_buffer.recover_open(&sessions, "shared").await?;
            if recovered > 0 {
                warn!(
                    recovered,
                    "recovered interrupted turns from the turn buffer"
                );
            }
        } else {
            // 按 buffer 里记录的作用域恢复：只知道 chat 列表不够，崩溃的回合可能
            // 属于一个还没有任何已提交分段的 chat。
            for scope in turn_buffer.open_scopes()? {
                let scoped = sessions.for_scope(&scope);
                if let Err(error) = turn_buffer.recover_open(&scoped, &scope).await {
                    warn!(%error, %scope, "cannot recover a per-chat turn");
                }
            }
        }
        if !per_chat {
            let current = sessions.current()?;
            let recovered_history = sessions.read_segment(&current.file)?;
            let mut shared = history.lock().await;
            // 只在此处初始化一次；重复构造会重置内存历史。
            if shared.is_empty() {
                *shared = recovered_history;
            }
        }

        let dynamic = prompt::load_hooks(&vfs, &config.hooks);

        let mut system = vec![SystemBlock {
            text: prompt::render_static(config.mode),
            cache: true,
        }];
        for block in dynamic.render() {
            system.push(SystemBlock {
                text: block,
                cache: false,
            });
        }

        let slot = Arc::new(std::sync::RwLock::new(ProviderSlot {
            reference: config.model.clone(),
            api_model: provider.model_id().to_owned(),
            provider,
            max_tokens: config.max_tokens,
            thinking: config.thinking.clone(),
        }));

        Ok(Arc::new(Self {
            slot,
            fallbacks,
            hub: std::sync::OnceLock::new(),
            ai_enabled,
            estop_file,
            tools,
            gate,
            vfs,
            messages,
            sessions,
            turn_buffer,
            qq,
            config,
            history,
            scopes: ScopeCache::new(per_chat),
            system: std::sync::RwLock::new(system),
            compaction_lock: Mutex::new(()),
            compaction_failures: std::sync::atomic::AtomicU32::new(0),
            turn_times: Mutex::new(HashMap::new()),
            unchecked_tokens: std::sync::atomic::AtomicU64::new(0),
            review_failures: std::sync::atomic::AtomicU32::new(0),
            review_lock: Mutex::new(()),
        }))
    }

    /// 共享历史句柄（`agent.*` 操作使用）。
    pub fn history(&self) -> Arc<Mutex<Vec<AgentMessage>>> {
        Arc::clone(&self.history)
    }

    /// chat → transcript 作用域名。
    fn chat_scope(chat: ChatId) -> String {
        match chat {
            ChatId::Group(id) => format!("group-{id}"),
            ChatId::Private(id) => format!("private-{id}"),
        }
    }

    /// 当前回合归属的作用域；shared 模式统一记 `shared`。
    fn scope_name(&self, chat: ChatId) -> String {
        if self.scopes.per_chat {
            Self::chat_scope(chat)
        } else {
            "shared".to_owned()
        }
    }

    /// 取该 chat 的分段存储；shared 模式返回全局存储。
    async fn session_for(&self, chat: ChatId) -> Arc<SessionStore> {
        if !self.scopes.per_chat {
            return Arc::clone(&self.sessions);
        }
        self.scopes
            .session_for(&Self::chat_scope(chat), &self.sessions)
            .await
    }

    /// 取该 chat 的历史句柄；首次访问时从对应分段载入。
    async fn history_for(&self, chat: ChatId) -> Arc<Mutex<Vec<AgentMessage>>> {
        if !self.scopes.per_chat {
            return Arc::clone(&self.history);
        }
        self.scopes
            .history_for(&Self::chat_scope(chat), &self.sessions)
            .await
    }

    /// 当前 system 块快照。
    pub fn system(&self) -> Vec<SystemBlock> {
        self.system
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 重新读取 hooks 并重建 system 块（压缩成功后调用）。
    fn rebuild_system(&self) {
        let mut system = vec![SystemBlock {
            text: prompt::render_static(self.config.mode),
            cache: true,
        }];
        for block in prompt::load_hooks(&self.vfs, &self.config.hooks).render() {
            system.push(SystemBlock {
                text: block,
                cache: false,
            });
        }
        *self
            .system
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = system;
    }

    /// 自动压缩：超过工作集上限时压缩，并把 `working` 换成压缩后的历史。
    ///
    /// 两种回合（普通消息与控制/monitor 注入）共用，避免只在一边做压缩。
    async fn maybe_auto_compact(
        &self,
        working: &mut Vec<AgentMessage>,
        history: &Arc<Mutex<Vec<AgentMessage>>>,
        sessions: &Arc<SessionStore>,
    ) {
        let estimated = crate::session::compaction::estimate_messages(working);
        if estimated <= self.compaction_budget() {
            return;
        }
        let failures = self
            .compaction_failures
            .load(std::sync::atomic::Ordering::Relaxed);
        if self.config.compaction_max_failures > 0
            && failures >= self.config.compaction_max_failures
        {
            warn!(
                failures,
                "automatic compaction is disabled after repeated failures"
            );
            return;
        }
        match self
            .compact_scope("auto", Arc::clone(history), Arc::clone(sessions))
            .await
        {
            Ok(_) => {
                self.compaction_failures
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                *working = history.lock().await.clone();
            }
            Err(error) => {
                self.compaction_failures
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                warn!(%error, "automatic compaction failed");
            }
        }
    }

    /// 组装运行上下文。
    fn runtime_context(&self, chat: ChatId, sessions: &SessionStore) -> RuntimeContext {
        let inbox = self
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
            session_scope: self.config.session_scope.clone(),
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

    /// 清空上下文：shared 轮转唯一链；per_chat 轮转所有已打开的链。
    pub async fn clear_all_context(&self) -> Result<serde_json::Value> {
        if !self.scopes.per_chat {
            let current = self.sessions.current()?;
            let next = self.sessions.rotate()?;
            self.history.lock().await.clear();
            return Ok(serde_json::json!({
                "cleared": true,
                "previous_session": current.file,
                "current_session": next.file,
            }));
        }
        let stores = self.scopes.all_sessions().await;
        let mut cleared = Vec::new();
        for sessions in stores {
            let current = sessions.current()?;
            let next = sessions.rotate()?;
            cleared.push(serde_json::json!({
                "previous_session": current.file,
                "current_session": next.file,
            }));
        }
        self.scopes.clear_histories().await;
        Ok(serde_json::json!({"cleared": true, "chains": cleared}))
    }

    /// 立即压缩：摘要旧消息，轮转新分段，写入摘要与保留窗口。
    ///
    /// 设计 §8.1：摘要块固定包含 `previous_session`，当前分段的路径由 runtime 注入。
    pub async fn compact_now(&self, reason: &str) -> Result<serde_json::Value> {
        let chat = ChatId::Private(self.config.owner_qq);
        let history = self.history_for(chat).await;
        let sessions = self.session_for(chat).await;
        self.compact_scope(reason, history, sessions).await
    }

    /// 在指定链上压缩。
    async fn compact_scope(
        &self,
        reason: &str,
        history: Arc<Mutex<Vec<AgentMessage>>>,
        sessions: Arc<SessionStore>,
    ) -> Result<serde_json::Value> {
        let _guard = self.compaction_lock.lock().await;
        let snapshot = history.lock().await.clone();
        let (old, keep) = crate::session::compaction::split_for_compaction(
            &snapshot,
            self.config.keep_recent_tokens as usize,
        );
        if old.is_empty() {
            return Ok(serde_json::json!({
                "compacted": false,
                "reason": "nothing older than the retention window",
            }));
        }
        if self.config.check_on_compaction {
            self.run_security_review(&snapshot).await;
        }
        // 压缩前把 durable facts 静默写进记忆，避免摘要把它们吞掉。
        if let Some(flusher) = &self.config.memory_flush
            && let Err(error) = flusher.flush(&snapshot).await
        {
            warn!(%error, "memory flush before compaction failed");
        }
        let input = crate::session::compaction::render_for_summary(&old, 80_000);
        let previous = sessions.current()?;
        // 摘要可以用独立模型；默认仍是主 provider。
        let (provider, model) = match &self.config.compaction {
            Some(compaction) => (Arc::clone(&compaction.provider), compaction.model.clone()),
            None => {
                let slot = self.provider_snapshot();
                (slot.provider, slot.api_model)
            }
        };
        let summary = crate::session::compaction::summarize(&*provider, &model, &input, 4096)
            .await
            .map_err(|error| anyhow::anyhow!("compaction request failed: {error}"))?;

        let summary_message = AgentMessage::System {
            text: format!(
                "[checkpoint summary | previous_session=/sessions/{} | reason={reason}]
{summary}",
                previous.file
            ),
        };
        let next = sessions.rotate()?;
        // 检查点与分段首行摘要同时落库；分段首行仍是权威。
        sessions
            .record_checkpoint(&previous.file, &summary)
            .map_err(|error| anyhow::anyhow!("cannot record the compaction checkpoint: {error}"))?;
        let mut new_history = vec![summary_message];
        new_history.extend(keep);
        // PostCompact recall：把记忆索引作为新分段的第一条上下文，避免压缩后
        // 新回合在 system prompt 重建前“失忆”。只读、不标记已读。
        if self.config.post_compact_recall
            && let Ok(index) = self
                .vfs
                .read_text_unrecorded(&crate::vfs::VfsPath::parse("/MEMORY.md").expect("valid"))
            && !index.trim().is_empty()
        {
            let mut text: String = index.chars().take(8_000).collect();
            if index.chars().count() > 8_000 {
                text.push('…');
            }
            new_history.push(AgentMessage::System {
                text: format!(
                    "<memory_context source=\"post_compact\">
{text}
</memory_context>"
                ),
            });
        }
        sessions.append(&new_history)?;
        let kept_messages = new_history.len() - 1;
        *history.lock().await = new_history;
        // PostCompact：hooks 可能依赖刚写入的记忆/技能，重新读盘并重建。
        self.rebuild_system();

        Ok(serde_json::json!({
            "compacted": true,
            "previous_session": previous.file,
            "current_session": next.file,
            "summary_bytes": summary.len(),
            "kept_messages": kept_messages,
        }))
    }

    /// 自动压缩阈值：配置的工作集上限与模型窗口减去预留后的较小者。
    fn compaction_budget(&self) -> usize {
        // `context_window = 0` 表示模型没有声明窗口；只用配置的工作集上限。
        let by_window = if self.config.context_window == 0 {
            self.config.turn_context_budget as usize
        } else {
            let reserve = u64::from(self.config.reserve_tokens)
                .saturating_add(u64::from(self.config.max_tokens));
            self.config.context_window.saturating_sub(reserve).max(1) as usize
        };
        by_window
            .min(self.config.turn_context_budget as usize)
            .max(self.config.keep_recent_tokens as usize + 1)
    }

    /// 累计新增上下文 token；到达阈值后在回合边界触发检查。
    fn accumulate_review_tokens(&self, tokens: usize) {
        if self.config.security_review.is_none() || self.config.check_every_tokens == 0 {
            return;
        }
        self.unchecked_tokens
            .fetch_add(tokens as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// 回合边界：达到阈值就同步检查一次。
    async fn maybe_security_review(&self, history: &Arc<Mutex<Vec<AgentMessage>>>) {
        if self.config.security_review.is_none() || self.config.check_every_tokens == 0 {
            return;
        }
        if self
            .unchecked_tokens
            .load(std::sync::atomic::Ordering::Relaxed)
            < self.config.check_every_tokens
        {
            return;
        }
        let _guard = self.review_lock.lock().await;
        // 拿到锁后重新确认：并发回合可能已经检查过。
        if self
            .unchecked_tokens
            .load(std::sync::atomic::Ordering::Relaxed)
            < self.config.check_every_tokens
        {
            return;
        }
        let snapshot = history.lock().await.clone();
        self.run_security_review(&snapshot).await;
    }

    /// 用独立模型检查窗口；只写安全事件，不写 transcript。
    async fn run_security_review(&self, history: &[AgentMessage]) {
        let Some(checker) = &self.config.security_review else {
            return;
        };
        let window = render_review_window(history, self.config.check_overlap_tokens);
        match checker.review(&window).await {
            Ok(outcome) => {
                self.review_failures
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                self.unchecked_tokens
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                let findings = outcome.findings.join("; ");
                let reason = format!(
                    "security review: {} risk, complied={}{}",
                    outcome.risk.as_str(),
                    outcome.complied,
                    if findings.is_empty() {
                        String::new()
                    } else {
                        format!("; {findings}")
                    }
                );
                let score = match outcome.risk {
                    crate::security::review::Risk::Low => 0.1,
                    crate::security::review::Risk::Medium => 0.4,
                    crate::security::review::Risk::High => 0.8,
                    crate::security::review::Risk::Critical => 1.0,
                };
                let blocked = matches!(
                    outcome.risk,
                    crate::security::review::Risk::High | crate::security::review::Risk::Critical
                ) && (outcome.complied
                    || outcome.risk == crate::security::review::Risk::Critical);
                self.gate.record_event(!blocked, score, "review", &reason);
                match outcome.risk {
                    crate::security::review::Risk::Low => debug!(%reason, "security review"),
                    crate::security::review::Risk::Medium => {
                        warn!(%reason, "security review raised medium risk")
                    }
                    crate::security::review::Risk::High if outcome.complied => {
                        warn!(%reason, "security review: injection appears to have worked");
                        self.gate.force_hold(&reason);
                    }
                    crate::security::review::Risk::High => {
                        warn!(%reason, "security review raised high risk")
                    }
                    crate::security::review::Risk::Critical => {
                        warn!(%reason, "security review paused the agent");
                        self.gate.pause_agent(&reason);
                    }
                }
            }
            Err(error) => {
                let failures = self
                    .review_failures
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                // 检查超时/失败按放行处理（设计 §5.9）；连续失败只告警。
                self.unchecked_tokens
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                warn!(failures, %error, "security review failed; outbound stays allowed");
            }
        }
    }

    /// 控制通道输入：`sbot chat` 注入的一句话。
    ///
    /// 作为 `<owner_context source="control:<id>">` 注入并立即跑一个回合；
    /// 不依赖任何 QQ chat，因此 `origin_chat` 标为 `control`。
    /// 共享槽位；runtime 用它实现模型/effort 热切换。
    pub fn provider_slot(&self) -> SharedProviderSlot {
        Arc::clone(&self.slot)
    }

    /// 注入子代理 hub；之后嵌套 spawn 才能为子代理注册 `Agent` 工具。
    pub fn set_hub(&self, hub: Arc<crate::swarm::AgentHub>) {
        let _ = self.hub.set(hub);
    }

    /// 触发批次里是否有主人本人的消息（转发/引用不算）。
    fn evidence_for_batch(
        &self,
        chat: ChatId,
        messages: &[crate::qq::event::QqMessage],
    ) -> TurnEvidence {
        let owner = self.config.owner_qq;
        let owner_intent = owner != 0
            && messages
                .iter()
                .any(|message| !message.is_self && message.sender.qq == Some(owner));
        TurnEvidence {
            owner_intent,
            origin_chat: Some(chat.to_string()),
        }
    }

    /// 取一份 provider 快照；调用方在 await 前调用，避免跨 await 持有锁。
    fn provider_snapshot(&self) -> ProviderSlot {
        self.slot
            .read()
            .expect("provider slot lock poisoned")
            .clone()
    }

    pub async fn run_control(&self, text: &str, control_id: i64) -> Result<LoopOutcome> {
        if !self.ai_enabled.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("AI calls are stopped; run `sbot ai start` first");
        }
        let message = AgentMessage::user(format!(
            "<owner_context source=\"control:{control_id}\">\n{text}\n</owner_context>"
        ));
        self.run_injected("control", message).await
    }

    /// monitor 投递：以 `[monitor: name | source | time]` 信封进入上下文。
    pub async fn run_event(&self, name: &str, source: &str, text: &str) -> Result<LoopOutcome> {
        if !self.ai_enabled.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("AI calls are stopped; run `sbot ai start` first");
        }
        if self.estop_file.exists() {
            anyhow::bail!("ESTOP is active");
        }
        let message = AgentMessage::user(format!(
            "[monitor: {} | {} | {}]\n{}",
            name,
            source,
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            text
        ));
        self.run_injected("monitor", message).await
    }

    /// 注入一条上下文消息并跑一个回合（控制通道与 monitor 共用）。
    async fn run_injected(&self, origin: &str, message: AgentMessage) -> Result<LoopOutcome> {
        if origin != "control" && self.gate.agent_paused() {
            debug!(%origin, "agent is paused by the security review; skipping this event");
            return Ok(LoopOutcome {
                stop_reason: crate::agent::StopReason::Aborted,
                usage: Default::default(),
                turns: 0,
            });
        }
        let chat = ChatId::Private(self.config.owner_qq);
        let sessions = self.session_for(chat).await;
        let history = self.history_for(chat).await;
        {
            let mut history = history.lock().await;
            history.push(message.clone());
        }
        sessions.append(&[message])?;

        let mut working = { history.lock().await.clone() };
        // 控制/monitor 回合同样可能很长：以前没有压缩，长会话会一直涨到超窗。
        self.maybe_auto_compact(&mut working, &history, &sessions)
            .await;
        let base = working.len();
        let runtime = RuntimeContext {
            origin_chat: origin.to_owned(),
            ..self.runtime_context(chat, &sessions)
        };
        let prompts = vec![AgentMessage::user(runtime.render())];
        let prompt_tokens = crate::session::compaction::estimate_messages(&prompts);

        let evidence = if origin == "control" {
            TurnEvidence::owner(origin)
        } else {
            TurnEvidence::monitor()
        };
        let executor: Arc<dyn crate::agent::ToolExecutor> = Arc::new(TurnExecutor {
            registry: Arc::clone(&self.tools),
            evidence,
        });
        let slot = self.provider_snapshot();
        let loop_ = AgentLoop::new_with_fallbacks(
            Arc::clone(&slot.provider),
            self.fallbacks.clone(),
            executor,
            Arc::clone(&self.gate),
            LoopConfig {
                model: slot.api_model.clone(),
                system: self.system(),
                max_tokens: slot.max_tokens,
                thinking: slot.thinking.clone(),
                server_tools: self.config.server_tools.clone(),
                max_steps: self.config.max_steps,
                retry: self.config.retry,
                max_state_chars: 32_000,
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
            },
        );
        let (tx, mut rx) = mpsc::channel(256);
        let logger = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if let AgentEvent::Error { message } = event {
                    warn!(%message, "agent error");
                }
            }
        });
        let mut steering = SteeringQueue::default();
        let outcome = loop_
            .run_with_steering(&mut working, prompts, &mut steering, None, &tx)
            .await;
        drop(tx);
        let _ = logger.await;

        let new_blocks: Vec<AgentMessage> = working[base..].to_vec();
        if !new_blocks.is_empty() {
            let turn_id = new_turn_id();
            let scope = self.scope_name(chat);
            for (seq, message) in new_blocks.iter().enumerate() {
                self.turn_buffer
                    .append(&turn_id, seq as u64, &scope, message)?;
            }
            self.turn_buffer.commit(&sessions, &turn_id).await?;
            history.lock().await.extend(new_blocks.clone());
        }
        self.accumulate_review_tokens(
            prompt_tokens + crate::session::compaction::estimate_messages(&new_blocks),
        );
        self.maybe_security_review(&history).await;
        Ok(outcome)
    }

    /// “上次回复以来的群消息”：提供上下文，但不触发回合。
    fn pending_history(&self, chat: ChatId, first_batch_id: Option<i64>) -> Option<String> {
        let recent = self
            .messages
            .recent(chat, self.config.chat_history_limit)
            .ok()?;
        let last_self = recent.iter().rposition(|message| message.is_self);
        let start = last_self.map(|index| index + 1).unwrap_or(0);
        let mut lines = Vec::new();
        for message in recent.iter().skip(start) {
            if message.is_self {
                continue;
            }
            if first_batch_id.is_some_and(|first| message.message_id >= first) {
                continue;
            }
            lines.push(format::render_inbound(message));
        }
        if lines.is_empty() {
            None
        } else {
            Some(format!(
                "[chat history since your last reply]\n{}",
                lines.join("\n")
            ))
        }
    }

    /// 跑一个完整回合。
    async fn execute(
        &self,
        input: TurnInput,
        mut steering_rx: mpsc::UnboundedReceiver<AgentMessage>,
    ) -> Result<()> {
        // `sbot ai stop` 与 `ESTOP` 都只暂停新回合；消息已经入库，下一回合可见。
        if !self.ai_enabled.load(std::sync::atomic::Ordering::Relaxed) {
            debug!(chat = %input.chat, "ai is stopped; skipping this turn");
            return Ok(());
        }
        if self.estop_file.exists() {
            debug!(chat = %input.chat, "ESTOP is active; skipping this turn");
            return Ok(());
        }
        if self.gate.agent_paused() {
            debug!(chat = %input.chat, "agent is paused by the security review; skipping this turn");
            return Ok(());
        }
        let chat = input.chat;
        let inbound_messages = input.messages.clone();
        let sessions = self.session_for(chat).await;
        let history = self.history_for(chat).await;
        let turn_id = new_turn_id();
        let first_batch_id = input.messages.first().map(|message| message.message_id);

        self.config.last_activity.store(
            chrono::Utc::now().timestamp(),
            std::sync::atomic::Ordering::Relaxed,
        );

        // 1) 入站消息立即进入 transcript 与共享历史。
        let mut inbound = Vec::with_capacity(input.messages.len());
        for message in &input.messages {
            let text = format::render_inbound(message);
            let agent_message = AgentMessage::user(text);
            inbound.push(agent_message.clone());
            sessions.append(&[agent_message])?;
            if let Err(error) =
                self.messages
                    .set_cursor(chat, message.message_id, &message.time.to_rfc3339())
            {
                warn!(%error, "cannot update chat cursor");
            }
        }

        // 2) 取历史副本；后续新块只写本地副本。
        let mut working = {
            let mut history = history.lock().await;
            for message in &inbound {
                history.push(message.clone());
            }
            history.clone()
        };

        // 2.5) 超过工作集上限时先压缩，再重新载入历史副本。
        self.maybe_auto_compact(&mut working, &history, &sessions)
            .await;
        // 压缩会改写 `working`，基点在压缩之后取：否则 `working[base..]` 既会越界，
        // 也会把保留的旧块重复提交。
        let base = working.len();

        // 2.6) 预算：消息已经入库，只跳过模型回合；下一个窗口/日期自动恢复。
        if let Some(budget) = &self.config.budget {
            if let Some(reason) = budget.check_daily().await {
                warn!(chat = %chat, %reason, "skipping this turn");
                return Ok(());
            }
            let limit = budget.chat_turns_per_hour();
            if limit > 0 {
                let mut turns = self.turn_times.lock().await;
                let queue = turns.entry(chat.to_string()).or_default();
                while queue
                    .front()
                    .is_some_and(|at| at.elapsed() >= Duration::from_secs(3600))
                {
                    queue.pop_front();
                }
                if queue.len() >= limit as usize {
                    warn!(
                        chat = %chat,
                        limit,
                        "chat turn budget exhausted; skipping this turn"
                    );
                    return Ok(());
                }
                queue.push_back(Instant::now());
            }
        }

        // 3) 组装本回合的即时提示。
        let mut prompts = vec![AgentMessage::user(
            self.runtime_context(chat, &sessions).render(),
        )];
        if let Some(pending) = self.pending_history(chat, first_batch_id) {
            prompts.push(AgentMessage::user(pending));
        }
        if input.dropped > 0 {
            prompts.push(AgentMessage::user(format!(
                "Note: {} earlier message(s) from this chat were dropped because the queue \
                 exceeded its limits. They were not read and cannot be recovered from context.",
                input.dropped
            )));
        }

        // 4) 运行循环。工具执行器带上本回合证据。
        let prompt_tokens = crate::session::compaction::estimate_messages(&prompts);
        let slot = self.provider_snapshot();
        let evidence = self.evidence_for_batch(chat, &inbound_messages);
        let executor: Arc<dyn crate::agent::ToolExecutor> = Arc::new(TurnExecutor {
            registry: Arc::clone(&self.tools),
            evidence,
        });
        let loop_ = AgentLoop::new_with_fallbacks(
            Arc::clone(&slot.provider),
            self.fallbacks.clone(),
            executor,
            Arc::clone(&self.gate),
            LoopConfig {
                model: slot.api_model.clone(),
                system: self.system(),
                max_tokens: slot.max_tokens,
                thinking: slot.thinking.clone(),
                server_tools: self.config.server_tools.clone(),
                max_steps: self.config.max_steps,
                retry: self.config.retry,
                max_state_chars: 32_000,
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
            },
        );
        let (events_tx, mut events_rx) = mpsc::channel(256);
        let logger = tokio::spawn(async move {
            while let Some(event) = events_rx.recv().await {
                match event {
                    AgentEvent::Security {
                        checkpoint,
                        allowed: false,
                        reason,
                    } => warn!(?checkpoint, %reason, "security gate blocked this turn"),
                    AgentEvent::Error { message } => warn!(%message, "agent error"),
                    _ => {}
                }
            }
        });
        let mut steering = SteeringQueue::default();
        let outcome = loop_
            .run_with_steering(
                &mut working,
                prompts,
                &mut steering,
                Some(&mut steering_rx),
                &events_tx,
            )
            .await;
        drop(events_tx);
        let _ = logger.await;
        if let Some(budget) = &self.config.budget {
            budget
                .record_usage(outcome.usage.input_tokens, outcome.usage.output_tokens)
                .await;
        }

        // 5) 提交新块：buffer -> transcript -> 共享历史。
        let new_blocks: Vec<AgentMessage> = working[base..].to_vec();
        if !new_blocks.is_empty() {
            let scope = self.scope_name(chat);
            for (seq, message) in new_blocks.iter().enumerate() {
                self.turn_buffer
                    .append(&turn_id, seq as u64, &scope, message)?;
            }
            self.turn_buffer.commit(&sessions, &turn_id).await?;
            let mut shared = history.lock().await;
            shared.extend(new_blocks.clone());
        }
        self.accumulate_review_tokens(
            prompt_tokens + crate::session::compaction::estimate_messages(&new_blocks),
        );
        self.maybe_security_review(&history).await;

        // 6) 已读标记（模拟真人；失败不影响回合结果）。
        if let Some(qq) = &self.qq {
            let _ = qq.mark_read(chat).await;
        }

        debug!(
            chat = %chat,
            stop = ?outcome.stop_reason,
            turns = outcome.turns,
            "turn completed"
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl TurnHandler for Runner {
    async fn handle(
        &self,
        input: TurnInput,
        steering: mpsc::UnboundedReceiver<AgentMessage>,
    ) -> Result<()> {
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
        let slot = self.provider_snapshot();
        let mut tools = self.tools.filtered(&request.tools);
        if request.read_only {
            tools = tools.read_only();
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
        let executor: Arc<dyn crate::agent::ToolExecutor> = Arc::new(TurnExecutor {
            registry: Arc::new(tools),
            evidence: request.evidence.clone(),
        });
        let tools = executor;
        let system = vec![SystemBlock {
            text: request.system,
            cache: false,
        }];
        let loop_ = AgentLoop::new_with_fallbacks(
            Arc::clone(&slot.provider),
            self.fallbacks.clone(),
            tools,
            Arc::clone(&self.gate),
            LoopConfig {
                model: slot.api_model,
                system,
                max_tokens: slot.max_tokens,
                thinking: slot.thinking,
                server_tools: Vec::new(),
                max_steps: request.max_steps,
                retry: self.config.retry,
                max_state_chars: 32_000,
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
            },
        );
        let mut history = request.history;
        history.push(AgentMessage::user(request.task));
        let (tx, mut rx) = mpsc::channel(256);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let mut steering = SteeringQueue::default();
        let outcome = loop_
            .run(&mut history, Vec::new(), &mut steering, &tx)
            .await;
        drop(tx);
        let _ = drain.await;

        if outcome.stop_reason == crate::agent::StopReason::Refusal {
            anyhow::bail!("the subagent request was refused by the provider");
        }
        let text = history
            .iter()
            .rev()
            .find_map(|message| match message {
                AgentMessage::Assistant(assistant) if !assistant.text().trim().is_empty() => {
                    Some(assistant.text())
                }
                _ => None,
            })
            .unwrap_or_default();
        if text.trim().is_empty() {
            anyhow::bail!("the subagent returned no text");
        }
        Ok(text)
    }
}

/// 回合结果摘要（供 CLI/日志使用）。
#[allow(dead_code)]
pub fn summarize(outcome: &LoopOutcome) -> String {
    format!(
        "stop={:?} turns={} tokens={}",
        outcome.stop_reason,
        outcome.turns,
        outcome.usage.input_tokens + outcome.usage.output_tokens
    )
}

/// 渲染周期检查窗口：从最新消息向前，直到字符预算。
fn render_review_window(history: &[AgentMessage], overlap_tokens: u64) -> String {
    let budget = 48_000usize.saturating_add((overlap_tokens as usize).saturating_mul(4));
    let mut out = String::new();
    for message in history.iter().rev() {
        let rendered = crate::agent::agent_loop::render_message(message);
        if out.len() + rendered.len() + 1 > budget {
            break;
        }
        out.insert_str(0, &rendered);
        out.insert(0, '\n');
    }
    out
}

/// `agent.*` 操作的运行时实现。
pub struct RuntimeOps {
    /// 压缩与上下文操作需要 runner；构造顺序决定了这里只能后置注入。
    pub runner: std::sync::OnceLock<Arc<Runner>>,
    pub gate: Arc<Gate>,
    pub vfs: Arc<VfsStore>,
    pub sessions: Arc<SessionStore>,
    pub messages: Arc<MessageStore>,
    pub db: Option<Arc<Database>>,
    pub history: Arc<Mutex<Vec<AgentMessage>>>,
    /// 当前模型引用；热切换时由 runtime 更新。
    pub model: Arc<std::sync::RwLock<String>>,
    pub owner_qq: u64,
    pub master_qq: u64,
    pub receive_scopes: Vec<String>,
    pub send_scopes: Vec<String>,
    pub estop_file: std::path::PathBuf,
}

#[async_trait::async_trait]
impl crate::tools::api::AgentOps for RuntimeOps {
    async fn status(&self) -> Result<serde_json::Value, String> {
        let usage = self.vfs.usage().map_err(|error| error.to_string())?;
        let events = self
            .db
            .as_ref()
            .and_then(|db| crate::security::events::recent(db, 5).ok())
            .unwrap_or_default();
        Ok(serde_json::json!({
            "hold": self.gate.hold_active(),
            "estop": self.estop_file.exists(),
            "model": *self.model.read().expect("model lock poisoned"),
            "owner_qq": self.owner_qq,
            "master_qq": self.master_qq,
            "receive_scopes": self.receive_scopes,
            "send_scopes": self.send_scopes,
            "vfs": {
                "active_bytes": usage.active_bytes,
                "trash_bytes": usage.trash_bytes,
                "nodes": usage.nodes,
            },
            "recent_security_events": events,
        }))
    }

    async fn resume(&self) -> Result<serde_json::Value, String> {
        if !self.gate.hold_active() {
            return Ok(serde_json::json!({"hold": false, "note": "not on hold"}));
        }
        // 复核真实内容：最近一次被判危险的增量 + 当前历史尾部 + hold 原因。
        // 旧实现只拿固定串 "resume request" 过闸，规则层看不到任何东西，
        // 于是任何 hold 都能被一次调用清掉。
        let mut judged = self.gate.flagged_content().unwrap_or_default();
        {
            let history = self.history.lock().await;
            let mut tail = String::new();
            for message in history.iter().rev() {
                let rendered = format!("{message:?}");
                if tail.len() + rendered.len() > 48_000 {
                    break;
                }
                tail.insert_str(0, &rendered);
                tail.insert(0, '\n');
            }
            judged.push_str(&tail);
        }
        if judged.trim().is_empty() {
            return Err(
                "The gate has no judged content to re-check; the hold stays in place.".to_owned(),
            );
        }
        let reason = self
            .gate
            .hold_reason()
            .unwrap_or_else(|| "security hold".to_owned());
        let verdict = self.gate.review(&reason, &judged).await;
        if verdict.dangerous {
            return Err(format!(
                "The gate still blocks this context: {}",
                verdict.reason
            ));
        }
        self.gate
            .clear_hold()
            .map_err(|error| format!("cannot clear the hold file: {error}"))?;
        Ok(serde_json::json!({
            "hold": false,
            "reason": verdict.reason,
        }))
    }

    async fn quarantine(&self, params: &serde_json::Value) -> Result<serde_json::Value, String> {
        let text = params
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "agent.quarantine requires \"text\"".to_owned())?;
        let reason = params
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("flagged by the agent");
        let name = format!(
            "/quarantine/{}-{}.md",
            chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ"),
            std::process::id()
        );
        let path = crate::vfs::VfsPath::parse(&name).map_err(|error| error.to_string())?;
        let body = format!("# Quarantined content\n\nreason: {reason}\n\n---\n\n{text}\n");
        self.vfs
            .write_text(&path, &body, None)
            .map_err(|error| error.to_string())?;
        Ok(serde_json::json!({"quarantined": path.to_string()}))
    }

    async fn compact(&self) -> Result<serde_json::Value, String> {
        let runner = self
            .runner
            .get()
            .ok_or_else(|| "the runner is not ready yet".to_owned())?;
        runner
            .compact_now("manual")
            .await
            .map_err(|error| error.to_string())
    }

    async fn clear_context(&self) -> Result<serde_json::Value, String> {
        let runner = self
            .runner
            .get()
            .ok_or_else(|| "the runner is not ready yet".to_owned())?;
        runner
            .clear_all_context()
            .await
            .map_err(|error| error.to_string())
    }
}

/// 从消息存储构造待注入的运行上下文（供其他模块复用）。
#[allow(dead_code)]
pub fn inbox_summary(messages: &MessageStore) -> String {
    messages
        .unread_counts()
        .map(|counts| {
            if counts.is_empty() {
                "nothing unread".to_owned()
            } else {
                counts
                    .iter()
                    .map(|(chat, count)| format!("{chat}: {count}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        })
        .unwrap_or_else(|_| "unknown".to_owned())
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
            backfilled: false,
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
                session_scope: "shared".to_owned(),
                receive_scopes: vec!["*".to_owned()],
                send_scopes: vec!["*".to_owned()],
                gate_mode: GateMode::Auto,
                max_tokens: 256,
                thinking: ThinkingOptions::default(),
                max_steps: 8,
                retry: crate::agent::agent_loop::RetryPolicy::default(),
                chat_history_limit: 10,
                hooks: crate::config::HooksConfig::default().context,
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
            .unwrap();

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
                session_scope: "shared".to_owned(),
                receive_scopes: vec!["*".to_owned()],
                send_scopes: vec!["*".to_owned()],
                gate_mode: GateMode::Auto,
                max_tokens: 256,
                thinking: ThinkingOptions::default(),
                max_steps: 4,
                retry: crate::agent::agent_loop::RetryPolicy::default(),
                chat_history_limit: 10,
                hooks: crate::config::HooksConfig::default().context,
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
            .unwrap();
        // 回合被跳过：没有 assistant 块。
        let current = runner.sessions.current().unwrap();
        let transcript = runner.sessions.read_segment(&current.file).unwrap();
        assert!(
            !transcript
                .iter()
                .any(|message| matches!(message, AgentMessage::Assistant(_)))
        );
        let _ = Usage::default();
        let _ = messages.usage().unwrap();
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
        let provider = Arc::new(FakeProvider {
            script: StdMutex::new(
                vec![vec![
                    ProviderEvent::TextDelta("## Facts\ncompacted".to_owned()),
                    ProviderEvent::Stop {
                        reason: StopReason::EndTurn,
                        refusal: None,
                    },
                ]]
                .into(),
            ),
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
                session_scope: "shared".to_owned(),
                receive_scopes: vec!["*".to_owned()],
                send_scopes: vec!["*".to_owned()],
                gate_mode: GateMode::Auto,
                max_tokens: 256,
                thinking: ThinkingOptions::default(),
                max_steps: 4,
                retry: crate::agent::agent_loop::RetryPolicy::default(),
                chat_history_limit: 10,
                hooks: crate::config::HooksConfig::default().context,
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
        }
        let previous = sessions.current().unwrap();
        let value = runner.compact_now("test").await.unwrap();
        assert_eq!(value["compacted"], true);
        assert_eq!(value["previous_session"], previous.file);
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
        assert!(new_history.len() < 21);
    }

    #[tokio::test]
    async fn per_chat_keeps_separate_transcripts_and_histories() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::storage::Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([9u8; 32]));
        let vfs = Arc::new(
            crate::vfs::sqlite::VfsStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                Limits::default(),
            )
            .unwrap(),
        );
        let sessions = Arc::new(
            crate::session::SessionStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let message_store = Arc::new(
            crate::storage::messages::MessageStore::open(
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
                        ProviderEvent::TextDelta("first".to_owned()),
                        ProviderEvent::Stop {
                            reason: StopReason::EndTurn,
                            refusal: None,
                        },
                    ],
                    vec![
                        ProviderEvent::TextDelta("second".to_owned()),
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
        let runner = Runner::new(
            provider,
            tools,
            gate,
            vfs,
            message_store,
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
                session_scope: "per_chat".to_owned(),
                receive_scopes: vec!["*".to_owned()],
                send_scopes: vec!["*".to_owned()],
                gate_mode: GateMode::Auto,
                max_tokens: 256,
                thinking: ThinkingOptions::default(),
                max_steps: 2,
                retry: crate::agent::agent_loop::RetryPolicy::default(),
                chat_history_limit: 10,
                hooks: crate::config::HooksConfig::default().context,
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
            },
            history,
            Arc::new(AtomicBool::new(true)),
            dir.path().join("ESTOP"),
        )
        .await
        .unwrap();

        for (chat, id, text) in [
            (ChatId::Group(1), 1, "group one"),
            (ChatId::Private(2), 2, "private two"),
        ] {
            let (_tx, rx) = mpsc::unbounded_channel();
            runner
                .handle(
                    TurnInput {
                        chat,
                        messages: vec![inbound(chat, id, text)],
                        dropped: 0,
                    },
                    rx,
                )
                .await
                .unwrap();
        }

        let files = sessions.list().unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|file| file.file.starts_with("group-1-")));
        assert!(files.iter().any(|file| file.file.starts_with("private-2-")));
        let histories = runner.scopes.histories.lock().await;
        assert_eq!(histories.len(), 2);
        let group = histories.get("group-1").unwrap().lock().await.clone();
        let private = histories.get("private-2").unwrap().lock().await.clone();
        assert!(group.iter().any(
            |message| matches!(message, AgentMessage::User { text } if text.contains("group one"))
        ));
        assert!(!group.iter().any(
            |message| matches!(message, AgentMessage::User { text } if text.contains("private two"))
        ));
        assert!(private.iter().any(
            |message| matches!(message, AgentMessage::User { text } if text.contains("private two"))
        ));
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
                session_scope: "shared".to_owned(),
                receive_scopes: vec!["*".to_owned()],
                send_scopes: vec!["*".to_owned()],
                gate_mode: GateMode::Auto,
                max_tokens: 256,
                thinking: ThinkingOptions::default(),
                max_steps: 4,
                retry: crate::agent::agent_loop::RetryPolicy::default(),
                chat_history_limit: 10,
                hooks: crate::config::HooksConfig::default().context,
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
            .unwrap();

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
        let provider = Arc::new(FakeProvider {
            script: StdMutex::new(
                vec![vec![
                    ProviderEvent::TextDelta("ok".to_owned()),
                    ProviderEvent::Stop {
                        reason: StopReason::EndTurn,
                        refusal: None,
                    },
                ]]
                .into(),
            ),
        });
        let tools = Arc::new(crate::tools::ToolRegistry::new(Arc::clone(&vfs), None));
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
                session_scope: "shared".to_owned(),
                receive_scopes: vec!["*".to_owned()],
                send_scopes: vec!["*".to_owned()],
                gate_mode: GateMode::Auto,
                max_tokens: 256,
                thinking: ThinkingOptions::default(),
                max_steps: 4,
                retry: crate::agent::agent_loop::RetryPolicy::default(),
                chat_history_limit: 10,
                hooks: crate::config::HooksConfig::default().context,
                server_tools: Vec::new(),
                turn_context_budget: 120_000,
                keep_recent_tokens: 20_000,
                compaction_max_failures: 3,
                security_review: Some(Arc::new(FixedReview(outcome))),
                check_every_tokens: 1,
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
            .unwrap();
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
        assert!(!gate.agent_paused());
    }

    #[tokio::test]
    async fn critical_risk_pauses_new_turns() {
        let (runner, gate, _dir) = runner_with_review(crate::security::review::ReviewOutcome {
            risk: crate::security::review::Risk::Critical,
            complied: false,
            categories: vec!["exfiltration".to_owned()],
            findings: Vec::new(),
        })
        .await;
        one_turn(&runner, ChatId::Group(1)).await;
        assert!(gate.hold_active());
        assert!(gate.agent_paused());

        // 暂停后新的 QQ 回合被跳过；控制通道仍可工作（这里只验证 QQ 回合）。
        let before = runner.history().lock().await.len();
        one_turn(&runner, ChatId::Group(1)).await;
        assert_eq!(runner.history().lock().await.len(), before);
    }
}
