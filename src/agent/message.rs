//! 兼容重导出：消息模型已移到 `crate::transcript`。
//!
//! 保留该路径供 agent 内部与既有调用方使用；新代码直接依赖 `crate::transcript`，
//! 这样 session/swarm 等下层模块不再依赖 `agent`。

pub use crate::transcript::*;
