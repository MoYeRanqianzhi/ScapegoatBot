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

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use std::sync::Arc;
use std::time::Duration;

use crate::config::auth::AuthFile;
use crate::config::models::{ModelsFile, Protocol};
use crate::llm::types::{RefusalDetails, StopReason, Usage};

use self::sse::{SseEvent, SseParser, Utf8Stream};

pub mod anthropic;
pub mod chat;
pub mod metered;
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

mod slot;
pub use slot::{ProviderSlot, SharedProviderSlot};

/// 思考配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
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
    /// 把对话历史也标为可缓存（Anthropic 在最后一个块上打缓存断点）。
    ///
    /// 多步循环的下一次请求会命中这段前缀；一次性请求（摘要、审查、分类）不会复用，
    /// 写缓存只多付一笔写入费，所以由调用方决定。OpenAI 兼容接口自动缓存，忽略此项。
    pub cache_history: bool,
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
    /// `Retry-After` 建议的等待时间（毫秒）；由重试循环尊重，并受 `retry_max_ms` 上限约束。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        // 超时只来自 408 / 504（网关或服务端在等待中放弃），与 5xx 一样值得重试。
        let retryable = matches!(
            kind,
            ProviderErrorKind::Network
                | ProviderErrorKind::RateLimit
                | ProviderErrorKind::Server
                | ProviderErrorKind::Timeout
        );
        Self {
            kind,
            message: message.into(),
            status: None,
            retryable,
            retry_after_ms: None,
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
        Self::new(kind, truncate_chars(body, 800)).with_status(status)
    }

    /// 由完整 HTTP 响应构造：在状态码之外读取 `Retry-After`。
    ///
    /// 支持两种合法形式：秒数（可带小数）与 HTTP-date（RFC 7231）。
    /// 无法解析时忽略，回退到指数退避。
    pub fn from_response(
        status: u16,
        headers: Option<&reqwest::header::HeaderMap>,
        body: &str,
    ) -> Self {
        let mut error = Self::from_status(status, body);
        if let Some(headers) = headers
            && let Some(raw) = headers
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
        {
            error.retry_after_ms = parse_retry_after(raw);
        }
        error
    }
}

/// 解析 `Retry-After`；返回值单位毫秒。
fn parse_retry_after(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if let Ok(seconds) = raw.parse::<f64>() {
        if seconds.is_finite() && seconds >= 0.0 {
            return Some((seconds * 1000.0) as u64);
        }
        return None;
    }
    // HTTP-date：只接受未来时间；过去时间按 0 处理。
    let date = chrono::DateTime::parse_from_rfc2822(raw).ok()?;
    let millis = date.timestamp_millis() - chrono::Utc::now().timestamp_millis();
    Some(millis.max(0) as u64)
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

/// 截断过长文本，避免把整页 HTML 塞进上下文/日志；按字符而不是字节计数。
pub fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let truncated: String = text.chars().take(limit).collect();
    format!("{truncated}…")
}

/// 从模型输出里取出最外层的 JSON 对象文本（第一个 `{` 到最后一个 `}`）。
///
/// 判定器、审查器与记忆复盘都要求模型回 JSON，但模型常在外面加解释或代码围栏。
/// 输出里可能先出现 `}` 再出现 `{`（例如 `"} {"`）；这时没有对象，返回 `None`，
/// 而不是用倒置的区间切片导致 panic。`{`/`}` 都是 ASCII，下标一定落在字符边界上。
pub(crate) fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| &text[start..=end])
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

/// [`complete_text`] 的失败原因。
///
/// 只需要一段错误文字的调用方用 `Display`（`?` / `to_string()`）；需要区分超时、断流的
/// 调用方（内容闸、压缩摘要）按变体匹配。
#[derive(Debug)]
pub enum CompletionError {
    /// `Provider::stream` 调用本身失败（网络、鉴权、状态码等）。
    Request(ProviderError),
    /// 流内 `ProviderEvent::Error`。
    Stream(ProviderError),
    /// 超过调用方给出的总时限。
    TimedOut,
    /// 事件通道关闭，但没有收到 `Stop`。
    Ended,
}

impl std::fmt::Display for CompletionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(error) | Self::Stream(error) => write!(f, "{error}"),
            Self::TimedOut => f.write_str("the completion timed out"),
            Self::Ended => f.write_str("the stream ended without a stop event"),
        }
    }
}

impl std::error::Error for CompletionError {}

/// 单轮纯文本补全：可选 system、单条 user 消息、无工具、默认思考配置。
///
/// 返回 `Stop` 之前累计的文本；空文本不在这里判断，由调用方按各自的语义处理。
/// `timeout` 从事件接收端建立后开始计时，覆盖整个收流过程。
pub async fn complete_text(
    provider: &dyn Provider,
    model: &str,
    system: Vec<SystemBlock>,
    prompt: String,
    max_tokens: u32,
    timeout: Option<Duration>,
) -> Result<String, CompletionError> {
    let request = LlmRequest {
        model: model.to_owned(),
        system,
        messages: vec![ProviderMessage {
            role: Role::User,
            content: vec![ContentBlock::Text { text: prompt }],
        }],
        tools: Vec::new(),
        max_tokens,
        thinking: ThinkingOptions::default(),
        server_tools: Vec::new(),
        cache_history: false,
    };
    let mut receiver = provider
        .stream(request)
        .await
        .map_err(CompletionError::Request)?;
    let deadline = timeout.map(|timeout| tokio::time::Instant::now() + timeout);
    let mut text = String::new();
    loop {
        let event = match deadline {
            Some(deadline) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(CompletionError::TimedOut);
                }
                tokio::time::timeout_at(deadline, receiver.recv())
                    .await
                    .map_err(|_| CompletionError::TimedOut)?
            }
            None => receiver.recv().await,
        };
        let Some(event) = event else {
            return Err(CompletionError::Ended);
        };
        match event {
            ProviderEvent::TextDelta(delta) => text.push_str(&delta),
            ProviderEvent::Stop { .. } => return Ok(text),
            ProviderEvent::Error(error) => return Err(CompletionError::Stream(error)),
            _ => {}
        }
    }
}

/// 协议差异中的鉴权头部分。
pub(crate) enum Auth {
    /// `x-api-key` 加 `anthropic-version`。
    Anthropic(&'static str),
    /// `authorization: Bearer <key>`。
    Bearer,
}

/// 三种 HTTP 协议共享的外壳：客户端、基址、密钥与模型 id。
///
/// 协议实现只保留请求体构造与流解码；请求头差异通过 [`Auth`] 传入，
/// 状态码映射、事件通道与后台任务都在这里统一处理。
pub(crate) struct HttpProviderCore {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model_id: String,
    /// 等待响应头的上限（models.json 的 `timeout_secs`）。
    response_timeout: Duration,
}

impl HttpProviderCore {
    /// `timeout_secs` 只约束“发出请求到拿到响应头”。
    ///
    /// 不能用 `ClientBuilder::timeout`：reqwest 的总超时一直计到响应体读完，
    /// 长思考的流式回复超过它会被拦腰截断，再被当成网络错误整段重试（每次都计费）。
    /// 流式响应体由 [`pump`] 的逐块空闲超时（30s）保护，整个回合另有 wall-clock 预算。
    pub(crate) fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model_id: impl Into<String>,
        timeout_secs: u64,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(timeout_secs.clamp(1, 30)))
            .build()
            .map_err(|error| ProviderError::new(ProviderErrorKind::Config, error.to_string()))?;
        Ok(Self {
            client,
            base_url: base_url.into(),
            api_key: api_key.into(),
            model_id: model_id.into(),
            response_timeout: Duration::from_secs(timeout_secs.max(1)),
        })
    }

    pub(crate) fn model_id(&self) -> &str {
        &self.model_id
    }

    /// 发起流式请求；成功时把响应流交给后台的 [`pump`]。
    pub(crate) async fn send_stream<S: StreamDecoder + Send + 'static>(
        &self,
        path: &str,
        auth: Auth,
        body: &Value,
        state: S,
    ) -> Result<EventReceiver, ProviderError> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let request = self
            .client
            .post(url)
            .header("content-type", "application/json");
        let request = match auth {
            Auth::Anthropic(version) => request
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", version),
            Auth::Bearer => request.header("authorization", format!("Bearer {}", self.api_key)),
        };
        // 只给“拿到响应头”计时；响应体是流，由 `pump` 的空闲超时负责。
        let response = tokio::time::timeout(self.response_timeout, request.json(body).send())
            .await
            .map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Network,
                    format!(
                        "no response headers within {}s",
                        self.response_timeout.as_secs()
                    ),
                )
            })?
            .map_err(|error| ProviderError::new(ProviderErrorKind::Network, error.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::from_response(
                status.as_u16(),
                Some(&headers),
                &text,
            ));
        }

        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            pump(response, tx, state).await;
        });
        Ok(rx)
    }
}

/// 协议各自的 SSE 解码与 EOF 收尾。
///
/// [`pump`] 只负责共享的读流管线；各协议的差异（事件字段、延迟 stop、
/// 无 stop 时的错误文案）由各自的 `StreamState` 实现。
pub(crate) trait StreamDecoder {
    /// 处理一个 SSE 事件，返回要发送给上层的事件。
    fn decode(&mut self, event: SseEvent) -> Vec<ProviderEvent>;

    /// SSE 正常结束后的收尾事件；已经发过 `Stop` 时通常为空。
    fn finish(&mut self) -> Vec<ProviderEvent>;
}

/// 持续读取网络流并转发解析后的事件。
///
/// 30s 空闲超时、UTF-8 增量解码、SSE 尾部 flush 与发送失败退出都在这里；
/// 协议差异全部交给 [`StreamDecoder`]。
async fn pump<S: StreamDecoder>(
    response: reqwest::Response,
    tx: mpsc::Sender<ProviderEvent>,
    mut state: S,
) {
    let mut stream = response.bytes_stream();
    let mut parser = SseParser::new();
    // 网络 chunk 可能切在多字节字符中间，必须做增量解码。
    let mut decoder = Utf8Stream::new();

    loop {
        let chunk = match tokio::time::timeout(Duration::from_secs(30), stream.next()).await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(_) => {
                let _ = tx
                    .send(ProviderEvent::Error(ProviderError::new(
                        ProviderErrorKind::Network,
                        "stream idle timeout (30s)",
                    )))
                    .await;
                return;
            }
        };
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = tx
                    .send(ProviderEvent::Error(ProviderError::new(
                        ProviderErrorKind::Network,
                        error.to_string(),
                    )))
                    .await;
                return;
            }
        };
        let text = decoder.push(&bytes);
        for event in parser.push(&text) {
            for outgoing in state.decode(event) {
                if tx.send(outgoing).await.is_err() {
                    return;
                }
            }
        }
    }

    let tail = decoder.finish();
    if !tail.is_empty() {
        for event in parser.push(&tail) {
            for outgoing in state.decode(event) {
                if tx.send(outgoing).await.is_err() {
                    return;
                }
            }
        }
    }
    for event in parser.finish() {
        for outgoing in state.decode(event) {
            if tx.send(outgoing).await.is_err() {
                return;
            }
        }
    }
    for outgoing in state.finish() {
        let _ = tx.send(outgoing).await;
    }
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

/// 构造 provider 并装上计量：这个模型的每次请求都按它自己的价格记进 `sink`。
///
/// 运行时的模型请求都应经由这里构造（主模型、回退链、压缩、审查、热切换），
/// 否则用量不会进入每日预算（见 [`metered`]）。
pub fn build_metered(
    models: &ModelsFile,
    auth: &AuthFile,
    reference: &str,
    sink: &Arc<dyn metered::UsageSink>,
) -> Result<Arc<dyn Provider>, ProviderError> {
    let provider = build_provider(models, auth, reference)?;
    // `build_provider` 成功说明模型存在；价格取自同一条定义。
    let cost = models
        .find(reference)
        .map(|found| found.model.cost)
        .unwrap_or_default();
    Ok(metered::Metered::wrap(provider, cost, Arc::clone(sink)))
}

/// 把 Anthropic 的 `stop_reason` 与 Chat Completions 的 `finish_reason` 映射为内部原因。
///
/// 已知值来自各家的类型定义：Anthropic SDK 的 `StopReason`（含
/// `model_context_window_exceeded`）、OpenAI 的 finish_reason、DeepSeek 文档里的
/// `insufficient_system_resource` / `aborted`。认不出的值按 `Error` 处理并记日志：
/// 当成正常结束会执行一个可能不完整的回复里的工具调用。
pub fn map_stop_reason(raw: &str) -> StopReason {
    match raw {
        "end_turn" | "stop" => StopReason::EndTurn,
        "tool_use" | "tool_calls" | "function_call" => StopReason::ToolUse,
        // 输出到了上限，或上下文窗口被占满：回复都可能被截断。
        "max_tokens" | "length" | "model_context_window_exceeded" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "refusal" | "content_filter" => StopReason::Refusal,
        "pause_turn" => StopReason::PauseTurn,
        // DeepSeek：推理资源不足或生成被中断。
        "insufficient_system_resource" | "aborted" => StopReason::Error,
        other => {
            tracing::warn!(
                reason = other,
                "unknown stop reason; treating the response as failed"
            );
            StopReason::Error
        }
    }
}

/// 流事件里 `value[key]` 的字符串值；字段缺失或不是字符串时为 `None`。
///
/// 各协议的流解码器大量读取 `type`、`delta`、`id` 等字符串字段，共用这一个写法。
pub(super) fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_objects_are_extracted_from_fences_and_prose() {
        let text = "```json\n{\"memories\": [], \"style\": \"\"}\n```";
        assert_eq!(
            extract_json_object(text),
            Some("{\"memories\": [], \"style\": \"\"}")
        );
        assert_eq!(
            extract_json_object("结论如下：{\"allow\": true}。"),
            Some("{\"allow\": true}")
        );
        assert_eq!(extract_json_object("no object here"), None);
    }

    /// 判定器的输入含攻击者可控的聊天内容；`} {` 这类输出以前会让切片区间倒置而 panic。
    #[test]
    fn inverted_braces_do_not_panic() {
        assert_eq!(extract_json_object("} {"), None);
        assert_eq!(extract_json_object("}{"), None);
        assert_eq!(extract_json_object("}"), None);
        assert_eq!(extract_json_object("{"), None);
    }

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
        assert_eq!(
            map_stop_reason("model_context_window_exceeded"),
            StopReason::MaxTokens
        );
        assert_eq!(
            map_stop_reason("insufficient_system_resource"),
            StopReason::Error
        );
        // 认不出的值不能当成正常结束。
        assert_eq!(map_stop_reason("something-new"), StopReason::Error);
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
        assert!(ProviderError::from_status(408, "slow").retryable);
        assert!(ProviderError::from_status(504, "gateway").retryable);
        assert!(!ProviderError::from_status(400, "bad").retryable);
        assert!(
            ProviderError::from_status(400, &"x".repeat(2000))
                .message
                .ends_with('…')
        );
    }
}
