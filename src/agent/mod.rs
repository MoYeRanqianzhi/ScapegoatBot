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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnEvidence {
    pub owner_intent: bool,
    pub origin_chat: Option<String>,
}

impl TurnEvidence {
    pub fn owner(origin: impl Into<String>) -> Self {
        Self {
            owner_intent: true,
            origin_chat: Some(origin.into()),
        }
    }

    pub fn chat(origin: impl Into<String>) -> Self {
        Self {
            owner_intent: false,
            origin_chat: Some(origin.into()),
        }
    }

    pub fn monitor() -> Self {
        Self {
            owner_intent: false,
            origin_chat: Some("monitor".to_owned()),
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
