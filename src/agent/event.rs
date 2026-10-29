//! Agent 事件流。
//!
//! 设计依据：设计 §3.1。
//!
//! 事件同时用于：CLI/日志观测、测试断言、以及 Phase 5 的输入/输出闸挂钩。
//! 事件只描述“发生了什么”，不包含策略判断。

use serde::{Deserialize, Serialize};

use super::message::{AssistantMessage, StopReason, Usage};

/// 一个 agent 事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    /// 一次运行开始。
    AgentStart,
    /// 内层循环的一轮开始。
    TurnStart { turn: u32 },
    /// 一次模型请求开始。
    MessageStart,
    /// 流式增量。
    MessageUpdate { kind: DeltaKind, delta: String },
    /// 一条完整的助手消息。
    MessageEnd { message: AssistantMessage },
    /// 工具开始执行。
    ToolExecutionStart { id: String, name: String },
    /// 工具执行中的增量输出（目前只有文本）。
    ToolExecutionUpdate { id: String, delta: String },
    /// 工具结束。
    ToolExecutionEnd {
        id: String,
        name: String,
        is_error: bool,
    },
    /// 一轮结束。
    TurnEnd { reason: StopReason },
    /// 运行结束。
    AgentEnd { reason: StopReason, usage: Usage },
    /// 安全闸判定（`shadow` 模式只记录）。
    Security {
        checkpoint: SecurityCheckpoint,
        allowed: bool,
        reason: String,
    },
    /// 发生错误（严重到终止本轮）。
    Error { message: String },
}

/// 增量类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaKind {
    Text,
    Thinking,
}

/// 安全闸位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecurityCheckpoint {
    Input,
    Output,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_tagged_for_logging() {
        let json = serde_json::to_value(AgentEvent::TurnStart { turn: 3 }).unwrap();
        assert_eq!(json["event"], "turn_start");
        assert_eq!(json["turn"], 3);
    }

    #[test]
    fn tool_events_roundtrip() {
        let event = AgentEvent::ToolExecutionEnd {
            id: "t1".to_owned(),
            name: "Read".to_owned(),
            is_error: false,
        };
        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: AgentEvent = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, event);
    }
}
