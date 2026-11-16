//! Agent 事件流。
//!
//! 设计依据：设计 §3.1。
//!
//! 事件同时用于：CLI/日志观测、测试断言与安全闸观测。
//! 事件只描述“发生了什么”，不包含策略判断。

use serde::{Deserialize, Serialize};

use super::message::{AgentMessage, AssistantMessage, StopReason, Usage};

/// 一个 agent 事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    /// 一次运行开始。
    AgentStart,
    /// 内层循环的一轮开始。
    TurnStart { turn: u32 },
    /// 一次模型请求开始。请求失败后重试（或换回退模型）时再发一次：这之前收到的
    /// `MessageUpdate` 属于失败的那次尝试，作废。
    MessageStart,
    /// 流式增量，属于最近一次 `MessageStart` 开始的那次尝试。
    MessageUpdate { kind: DeltaKind, delta: String },
    /// 一条完整的助手消息。
    MessageEnd { message: AssistantMessage },
    /// 一条消息并入了本回合上下文（提示、助手消息、工具结果、steering）。
    /// runner 据此把块实时写进 turn buffer。
    Appended { message: AgentMessage },
    /// 工具开始执行。
    ToolExecutionStart { id: String, name: String },
    /// 工具结束。
    ToolExecutionEnd {
        id: String,
        name: String,
        is_error: bool,
    },
    /// 运行结束。
    AgentEnd { reason: StopReason, usage: Usage },
    /// 安全闸判定（`shadow` 模式只记录）。
    Security {
        checkpoint: SecurityCheckpoint,
        allowed: bool,
        reason: String,
    },
    /// 发生错误（严重到终止本轮）。重试之后成功的请求不发它。
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
