//! Agent 内部消息模型。
//!
//! 设计依据：设计 §3.1。
//!
//! 内部表示与 provider 协议解耦：`AssistantMessage` 保存文本、思考与工具调用，
//! `ToolResultMessage` 保存成对的工具结果。转换到具体协议由 `providers` 负责。

use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// 停止原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    #[default]
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    /// provider 安全分类器拒绝；不重试、不换模型。
    Refusal,
    /// 服务端工具暂停，需要继续请求。
    PauseTurn,
    Error,
    Aborted,
}

/// 用量。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl Usage {
    /// 累加（一次回合会有多次请求）。
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }

    /// 按 `models.json` 的价格估算美元金额。
    pub fn cost_usd(&self, input_per_mtok: f64, output_per_mtok: f64) -> f64 {
        let input =
            (self.input_tokens + self.cache_write_tokens) as f64 * input_per_mtok / 1_000_000.0;
        let output = self.output_tokens as f64 * output_per_mtok / 1_000_000.0;
        input + output
    }
}

/// provider 拒绝的细节。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RefusalDetails {
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub explanation: String,
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
