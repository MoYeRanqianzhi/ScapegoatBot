//! Agent：消息、事件、循环与工具执行契约。
//!
//! 设计依据：设计 §3。
//!
//! 本模块不依赖具体 provider 或工具实现：
//! - provider 走 [`crate::providers::Provider`]；
//! - 工具走 [`ToolExecutor`]；
//! - 安全走 [`crate::security::Gate`]。
//!
//! `loop.rs` 是 Rust 关键字，文件仍按设计命名，模块名用 `agent_loop`。

#[path = "loop.rs"]
pub mod agent_loop;
pub mod event;
pub mod hooks;
pub mod message;
pub mod queue;
pub mod runner;

pub use agent_loop::{AgentLoop, FallbackProvider, LoopConfig, LoopOutcome, SteeringQueue};
pub use event::{AgentEvent, DeltaKind, SecurityCheckpoint};
pub use message::{
    AgentMessage, AssistantBlock, AssistantMessage, RefusalDetails, StopReason, ToolCall,
    ToolResult, ToolResultMessage, Usage,
};

use crate::providers::ToolSpec;

/// 工具执行模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    /// 与同批其它工具并行。
    Parallel,
    /// 强制整批串行（有副作用或共享状态）。
    Sequential,
}

/// 一次回合的运行时证据。
///
/// 模型自报的“主人同意了”不算证据；这里只放 runtime 能核实的事实：
/// - `owner_intent`：本回合由控制通道触发，或触发批次里有 owner_qq 的非 self 消息；
/// - `origin_chat`：触发本回合的 chat（`group:<id>` / `private:<qq>` / `control` /
///   `monitor`）。同 chat 回复属于“有明确理由”，跨 chat 发送才需要 owner intent。
#[derive(Debug, Clone, Default)]
pub struct TurnEvidence {
    pub owner_intent: bool,
    pub origin_chat: Option<String>,
    /// 本回合进入上下文的内容片段及其来源 chat；跨 chat 泄露检查用。
    pub context_sources: std::sync::Arc<std::sync::Mutex<Vec<ContextSource>>>,
}

/// 一条带来源 chat 的上下文片段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSource {
    pub chat: String,
    pub text: String,
}

impl TurnEvidence {
    /// 记录一段上下文来源；只保留有限的条目与长度。
    pub fn record_source(&self, chat: impl Into<String>, text: impl Into<String>) {
        let text: String = text.into().chars().take(8_000).collect();
        if text.trim().is_empty() {
            return;
        }
        let mut sources = self
            .context_sources
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if sources.len() >= 32 {
            sources.remove(0);
        }
        sources.push(ContextSource {
            chat: chat.into(),
            text,
        });
    }
}

impl TurnEvidence {
    pub fn owner(origin: impl Into<String>) -> Self {
        Self {
            owner_intent: true,
            origin_chat: Some(origin.into()),
            ..Self::default()
        }
    }

    pub fn chat(origin: impl Into<String>) -> Self {
        Self {
            owner_intent: false,
            origin_chat: Some(origin.into()),
            ..Self::default()
        }
    }

    pub fn monitor() -> Self {
        Self {
            owner_intent: false,
            origin_chat: Some("monitor".to_owned()),
            ..Self::default()
        }
    }
}

/// 工具执行器。
///
/// 运行时的 `ToolRegistry` 实现它；测试用 fake 实现。
#[async_trait::async_trait]
pub trait ToolExecutor: Send + Sync {
    /// 当前可用工具声明，顺序稳定（影响 prompt 缓存）。
    fn specs(&self) -> Vec<ToolSpec>;

    /// 某个工具的执行模式；未知工具按并行处理。
    fn execution_mode(&self, _name: &str) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    /// 执行一次调用。实现方必须保证不 panic；错误通过 [`ToolResult::error`] 返回。
    async fn execute(&self, call: &ToolCall) -> ToolResult;
}
