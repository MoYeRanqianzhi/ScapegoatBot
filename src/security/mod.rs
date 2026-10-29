//! 安全子系统。
//!
//! 设计依据：设计 §5.9 / §5.10。
//!
//! - [`gate`]：输入闸 / 输出闸、规则层与 Jev 判定器、`hold` 文件；
//! - [`events`]：安全事件审计表。
//!
//! Phase 2 只接入 agent loop 与 `shadow` 模式；Phase 5 切到 `enforce` 并把
//! `agent.resume` 的重开路径接上。

pub mod events;
pub mod gate;

pub use events::Checkpoint;
pub use gate::{Gate, GateDecision, GateError, JevJudge, RuleJudge, SafetyJudge, Verdict};
