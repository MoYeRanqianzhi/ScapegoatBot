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

pub use agent_loop::{AgentLoop, LoopConfig, LoopOutcome, SteeringQueue};
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

/// 工具执行器。
///
/// Phase 4 的 `ToolRegistry` 实现它；Phase 2 的测试用 fake 实现。
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
