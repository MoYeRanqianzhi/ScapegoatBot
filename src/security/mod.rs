//! 安全子系统。
//!
//! 设计依据：设计 §5.9 / §5.10。
//!
//! - [`gate`]：输入闸 / 输出闸、规则层与 Jev 判定器、`hold` 文件；
//! - [`events`]：安全事件审计表。
//!
//! 闸按 `[security.guard].mode` 运行：`shadow` 只记录，`enforce` 命中即 hold；
//! `agent.resume` 的重开路径接上。

pub mod action_gate;
pub mod events;
pub mod gate;
pub mod review;

pub use events::Checkpoint;
pub use gate::{Gate, GateDecision, GateError, JevJudge, RuleJudge, SafetyJudge, Verdict};
