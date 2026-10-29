//! Provider：协议无关的请求/事件类型与三种协议的实现。
//!
//! 设计依据：设计 §3.2 / §9.3 / §10。
//!
//! - `anthropic`：Anthropic Messages（默认）；
//! - `chat`：OpenAI Chat Completions 兼容协议；
//! - `openai`：OpenAI Responses。
//!
//! 统一约束：
//! - 一律流式；部分流失败时由调用方丢弃，不落盘。
//! - `stop_reason = refusal` 不重试、不换模型（设计 §3.2）。
//! - 服务端工具（web search / fetch）由协议适配层声明与归一化。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use std::sync::Arc;

use crate::agent::message::{RefusalDetails, StopReason, Usage};
use crate::config::auth::AuthFile;
use crate::config::models::{ModelsFile, Protocol};

pub mod anthropic;
pub mod chat;
pub mod openai;
pub mod server_tools;
pub mod sse;

/// 角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// provider 消息的内容块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// 图片以 base64 传入；由 agent 在需要时从 VFS 读取。
    Image {
        mime_type: String,
        data_base64: String,
    },
    /// 助手发起的本地工具调用。
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// 本地工具结果。
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// 助手思考块（Anthropic 需要原样回传签名）。
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// 服务端工具调用（由 provider 执行，历史回传时需要保留）。
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

/// 一条 provider 消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

/// 系统提示块；`cache` 决定是否打 `cache_control`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemBlock {
    pub text: String,
    pub cache: bool,
}

/// 工具声明（本地工具）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// 思考配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[derive(Default)]
pub struct ThinkingOptions {
    pub enabled: bool,
    /// 统一 effort 级别；`None` 表示不发送。
    pub effort: Option<String>,
}


/// 服务端工具类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerToolKind {
    WebSearch,
    WebFetch,
}

/// 一次 LLM 请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmRequest {
    /// API model id。
    pub model: String,
    pub system: Vec<SystemBlock>,
    pub messages: Vec<ProviderMessage>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
    pub thinking: ThinkingOptions,
    pub server_tools: Vec<ServerToolKind>,
}

/// provider 流事件。
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    /// 文本增量。
    TextDelta(String),
    /// 思考增量。
    ThinkingDelta(String),
    /// 思考签名增量（Anthropic）。
    ThinkingSignature(String),
    /// 工具调用开始；`index` 用于同一条消息里多个并行调用。
    ToolCallStart {
        index: u32,
        id: String,
        name: String,
    },
    /// 工具参数 JSON 增量；调用方按 `index` 聚合后解析。
    ToolCallArgumentsDelta { index: u32, delta: String },
    /// 服务端工具调用（provider 执行，原样进入历史）。
    ServerToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// 服务端工具结果（原样进入历史）。
    ServerToolResult { tool_use_id: String, content: Value },
    /// 用量更新。
    Usage(Usage),
    /// 停止。
    Stop {
        reason: StopReason,
        refusal: Option<RefusalDetails>,
    },
    /// 流内错误事件。
    Error(ProviderError),
}

/// provider 错误类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Network,
    Auth,
    RateLimit,
    InvalidRequest,
    Server,
    Timeout,
    Refused,
    Config,
}

/// provider 错误。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
    pub status: Option<u16>,
    pub retryable: bool,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        let retryable = matches!(
            kind,
            ProviderErrorKind::Network | ProviderErrorKind::RateLimit | ProviderErrorKind::Server
        );
        Self {
            kind,
            message: message.into(),
            status: None,
            retryable,
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    /// 由 HTTP 状态码推断错误类别。
    pub fn from_status(status: u16, body: &str) -> Self {
        let kind = match status {
            401 | 403 => ProviderErrorKind::Auth,
            429 => ProviderErrorKind::RateLimit,
            408 | 504 => ProviderErrorKind::Timeout,
            400 | 404 | 413 | 422 => ProviderErrorKind::InvalidRequest,
            _ if status >= 500 => ProviderErrorKind::Server,
            _ => ProviderErrorKind::InvalidRequest,
        };
        Self::new(kind, truncate(body, 800)).with_status(status)
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(f, "{:?} (HTTP {status}): {}", self.kind, self.message),
            None => write!(f, "{:?}: {}", self.kind, self.message),
        }
    }
}

impl std::error::Error for ProviderError {}

/// 截断过长的错误正文，避免把整页 HTML 塞进上下文/日志。
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let truncated: String = text.chars().take(limit).collect();
    format!("{truncated}…")
}

/// 事件接收端。
pub type EventReceiver = mpsc::Receiver<ProviderEvent>;

/// provider 接口。
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// 协议名，用于日志与回退链。
    fn protocol(&self) -> crate::config::models::Protocol;

    /// API model id。
    fn model_id(&self) -> &str;

    /// 发起流式请求；返回事件接收端，任务在后台读取网络流。
    async fn stream(&self, request: LlmRequest) -> Result<EventReceiver, ProviderError>;
}

/// 按 `provider/id` 构造 provider。
///
/// 失败是配置问题：模型不在清单、凭据缺失、协议未知——都在启动或 spawn 时暴露，
/// 不在请求中途才报错。
pub fn build_provider(
    models: &ModelsFile,
    auth: &AuthFile,
    reference: &str,
) -> Result<Arc<dyn Provider>, ProviderError> {
    let found = models.find(reference).ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::Config,
            format!("model {reference:?} is not defined in models.json"),
        )
    })?;
    let key = auth
        .provider_key(found.provider_name)
        .and_then(|secret| secret.value())
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Config,
                format!(
                    "no API key for provider {:?} in auth.json",
                    found.provider_name
                ),
            )
        })?;
    let base_url = found.provider.base_url.clone();
    let model_id = found.model.id.clone();
    let timeout = found.provider.timeout_secs;

    let provider: Arc<dyn Provider> = match found.provider.api {
        Protocol::Anthropic => Arc::new(anthropic::AnthropicProvider::new(
            base_url, key, model_id, timeout,
        )?),
        Protocol::Chat => Arc::new(chat::ChatProvider::new(base_url, key, model_id, timeout)?),
        Protocol::Openai => Arc::new(openai::OpenAiProvider::new(
            base_url, key, model_id, timeout,
        )?),
    };
    Ok(provider)
}

/// 把 Anthropic / OpenAI 的 stop_reason 字符串映射为内部原因。
pub fn map_stop_reason(raw: &str) -> StopReason {
    match raw {
        "end_turn" | "stop" => StopReason::EndTurn,
        "tool_use" | "tool_calls" | "function_call" => StopReason::ToolUse,
        "max_tokens" | "length" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "refusal" | "content_filter" => StopReason::Refusal,
        "pause_turn" => StopReason::PauseTurn,
        _ => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_mapping_covers_both_protocols() {
        assert_eq!(map_stop_reason("end_turn"), StopReason::EndTurn);
        assert_eq!(map_stop_reason("stop"), StopReason::EndTurn);
        assert_eq!(map_stop_reason("tool_use"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("tool_calls"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("max_tokens"), StopReason::MaxTokens);
        assert_eq!(map_stop_reason("length"), StopReason::MaxTokens);
        assert_eq!(map_stop_reason("refusal"), StopReason::Refusal);
        assert_eq!(map_stop_reason("content_filter"), StopReason::Refusal);
        assert_eq!(map_stop_reason("pause_turn"), StopReason::PauseTurn);
        assert_eq!(map_stop_reason("something-new"), StopReason::EndTurn);
    }

    #[test]
    fn http_errors_are_classified() {
        assert_eq!(
            ProviderError::from_status(401, "no").kind,
            ProviderErrorKind::Auth
        );
        assert_eq!(
            ProviderError::from_status(429, "slow").kind,
            ProviderErrorKind::RateLimit
        );
        assert_eq!(
            ProviderError::from_status(500, "oops").kind,
            ProviderErrorKind::Server
        );
        assert!(ProviderError::from_status(500, "oops").retryable);
        assert!(!ProviderError::from_status(400, "bad").retryable);
        assert!(
            ProviderError::from_status(400, &"x".repeat(2000))
                .message
                .ends_with('…')
        );
    }
}
