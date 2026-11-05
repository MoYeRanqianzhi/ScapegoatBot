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
pub use crate::hooks;
pub mod message;
pub mod queue;
pub mod runner;

pub use agent_loop::{AgentLoop, FallbackProvider, LoopConfig, LoopOutcome, SteeringQueue};
pub use event::{AgentEvent, DeltaKind, SecurityCheckpoint};
pub use message::{
    AgentMessage, AssistantBlock, AssistantMessage, RefusalDetails, StopReason, ToolCall,
    ToolResult, ToolResultMessage, Usage,
};

pub use crate::transcript::{ContextSource, TurnEvidence};

pub use crate::tools::{ExecutionMode, ToolExecutor};
