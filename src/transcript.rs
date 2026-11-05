//! Agent 内部消息模型。
//!
//! 设计依据：设计 §3.1。
//!
//! 内部表示与 provider 协议解耦：`AssistantMessage` 保存文本、思考与工具调用，
//! `ToolResultMessage` 保存成对的工具结果。转换到具体协议由 `providers` 负责。

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::llm::types::{RefusalDetails, StopReason, Usage};

/// 一次工具调用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// 协议侧的调用 id；本地生成的工具结果必须原样带回。
    pub id: String,
    pub name: String,
    /// 已解析的参数对象。
    pub arguments: Value,
}

/// 工具执行结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// 进入上下文的文本。
    pub content: String,
    pub is_error: bool,
    /// 工具附带的图片；随 transcript 持久化，重启后仍能恢复。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub mime: String,
    pub data_base64: String,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            images: Vec::new(),
        }
    }

    pub fn error_with_images(content: impl Into<String>, images: Vec<ImageAttachment>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            images,
        }
    }

    pub fn with_images(content: impl Into<String>, images: Vec<ImageAttachment>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            images,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            images: Vec::new(),
        }
    }
}

/// 助手消息的内容块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantBlock {
    Text {
        text: String,
    },
    /// 思考内容；是否保留由 provider 决定（Anthropic 需要原样回传签名块）。
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolCall(ToolCall),
    /// 服务端工具调用（provider 执行），历史回传需要保留。
    ServerToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// 服务端工具结果原文。
    ServerToolResult {
        tool_use_id: String,
        content: Value,
    },
}

/// 助手消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Vec<AssistantBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
    /// provider 侧拒绝的细节（`category` / `explanation`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<RefusalDetails>,
}

impl AssistantMessage {
    /// 文本块拼接结果。
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                AssistantBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// 工具调用列表。
    pub fn tool_calls(&self) -> Vec<&ToolCall> {
        self.content
            .iter()
            .filter_map(|block| match block {
                AssistantBlock::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect()
    }
}

/// 工具结果消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageAttachment>,
}

/// 内部消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum AgentMessage {
    System {
        text: String,
    },
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageAttachment>,
    },
    Assistant(AssistantMessage),
    Tool(ToolResultMessage),
}

impl AgentMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            text: text.into(),
            images: Vec::new(),
        }
    }

    pub fn user_with_images(text: impl Into<String>, images: Vec<ImageAttachment>) -> Self {
        Self::User {
            text: text.into(),
            images,
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self::System { text: text.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_owned(),
            name: "Read".to_owned(),
            arguments: serde_json::json!({"path": "/AGENTS.md"}),
        }
    }

    #[test]
    fn assistant_text_and_tool_calls() {
        let message = AssistantMessage {
            content: vec![
                AssistantBlock::Text {
                    text: "hello ".to_owned(),
                },
                AssistantBlock::ToolCall(call("t1")),
                AssistantBlock::Text {
                    text: "world".to_owned(),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            refusal: None,
        };
        assert_eq!(message.text(), "hello world");
        assert_eq!(message.tool_calls().len(), 1);
        assert_eq!(message.tool_calls()[0].id, "t1");
    }

    #[test]
    fn usage_accumulates_and_costs() {
        let mut usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            ..Usage::default()
        };
        usage.add(Usage {
            input_tokens: 500_000,
            ..Usage::default()
        });
        assert_eq!(usage.input_tokens, 1_500_000);
        // 1.5M 输入 @ $4/M + 0.1M 输出 @ $20/M = 6 + 2 = 8
        assert!((usage.cost_usd(4.0, 20.0) - 8.0).abs() < f64::EPSILON);
    }

    #[test]
    fn messages_serialize_with_role_tag() {
        let json = serde_json::to_value(AgentMessage::user("hi")).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["text"], "hi");
    }

    #[test]
    fn tool_result_images_survive_serialization() {
        let message = AgentMessage::Tool(ToolResultMessage {
            tool_call_id: "t1".to_owned(),
            tool_name: "Read".to_owned(),
            content: "image".to_owned(),
            is_error: false,
            images: vec![ImageAttachment {
                mime: "image/png".to_owned(),
                data_base64: "aGVsbG8=".to_owned(),
            }],
        });
        let encoded = serde_json::to_string(&message).unwrap();
        assert!(encoded.contains("aGVsbG8="));
        let decoded: AgentMessage = serde_json::from_str(&encoded).unwrap();
        let AgentMessage::Tool(tool) = decoded else {
            panic!("expected tool message");
        };
        assert_eq!(tool.images.len(), 1);
        assert_eq!(tool.images[0].mime, "image/png");
    }
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
