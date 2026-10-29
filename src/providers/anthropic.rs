//! Anthropic Messages 协议（默认）。
//!
//! 设计依据：设计 §3.2 / §9.3；协议版本与字段以官方当前文档为准：
//! `anthropic-version: 2026-10-21`，思考用 `thinking: {type: "adaptive"}`，
//! effort 用 `output_config.effort`（`budget_tokens` 在 Opus 5.5 上会 400）。

use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::agent::message::{RefusalDetails, StopReason, Usage};
use crate::config::models::Protocol;

use super::sse::SseParser;
use super::{
    ContentBlock, EventReceiver, LlmRequest, Provider, ProviderError, ProviderEvent, Role,
    SystemBlock, ThinkingOptions, map_stop_reason, server_tools,
};

/// Anthropic API 版本头。
pub const API_VERSION: &str = "2026-10-21";
/// 默认服务端工具最大调用次数。
const SERVER_TOOL_MAX_USES: u32 = 10;

/// Anthropic provider。
pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model_id: String,
}

impl AnthropicProvider {
    /// 构造；`timeout_secs` 来自 `models.json` 的 provider 配置。
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model_id: impl Into<String>,
        timeout_secs: u64,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
            .build()
            .map_err(|error| {
                ProviderError::new(super::ProviderErrorKind::Config, error.to_string())
            })?;
        Ok(Self {
            client,
            base_url: base_url.into(),
            api_key: api_key.into(),
            model_id: model_id.into(),
        })
    }

    /// 请求体构造，独立成函数便于单元测试。
    pub fn build_body(request: &LlmRequest) -> Value {
        let mut body = json!({
            "model": request.model,
            "max_tokens": request.max_tokens,
            "stream": true,
            "messages": messages_json(&request.messages),
        });

        if !request.system.is_empty() {
            body["system"] = Value::Array(system_json(&request.system));
        }

        let mut tools = Vec::new();
        for spec in &request.tools {
            tools.push(json!({
                "name": spec.name,
                "description": spec.description,
                "input_schema": spec.input_schema,
            }));
        }
        for kind in &request.server_tools {
            tools.push(server_tools::anthropic_decl(*kind, SERVER_TOOL_MAX_USES));
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }

        apply_thinking(&mut body, &request.thinking);
        body
    }
}

/// 思考与 effort。
fn apply_thinking(body: &mut Value, thinking: &ThinkingOptions) {
    if thinking.enabled {
        // adaptive 是当前模型唯一可用的形态；display=summarized 让思考以摘要形式回传。
        body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
    }
    if let Some(effort) = &thinking.effort {
        body["output_config"] = json!({ "effort": effort });
    }
}

fn system_json(blocks: &[SystemBlock]) -> Vec<Value> {
    blocks
        .iter()
        .map(|block| {
            let mut value = json!({ "type": "text", "text": block.text });
            if block.cache {
                value["cache_control"] = json!({ "type": "ephemeral" });
            }
            value
        })
        .collect()
}

fn messages_json(messages: &[super::ProviderMessage]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for message in messages {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let content: Vec<Value> = message.content.iter().map(block_json).collect();
        out.push(json!({ "role": role, "content": content }));
    }
    out
}

fn block_json(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
        ContentBlock::Image {
            mime_type,
            data_base64,
        } => json!({
            "type": "image",
            "source": { "type": "base64", "media_type": mime_type, "data": data_base64 },
        }),
        ContentBlock::ToolUse { id, name, input } => json!({
            "type": "tool_use", "id": id, "name": name, "input": input,
        }),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error,
        }),
        ContentBlock::Thinking { text, signature } => json!({
            "type": "thinking",
            "thinking": text,
            "signature": signature.clone().unwrap_or_default(),
        }),
        ContentBlock::ServerToolUse { id, name, input } => json!({
            "type": "server_tool_use", "id": id, "name": name, "input": input,
        }),
        ContentBlock::ServerToolResult { content, .. } => {
            // 原文就是 Anthropic 的结果块（web_search_tool_result 等），逐字回传。
            content.clone()
        }
    }
}

/// 流处理过程中的累计状态。
#[derive(Debug, Default)]
struct StreamState {
    usage: Usage,
    stop_sent: bool,
    stop_reason: Option<StopReason>,
    refusal: Option<RefusalDetails>,
}

impl StreamState {
    /// 处理一个 SSE 事件，返回要发送给上层的事件。
    fn handle(&mut self, event: super::sse::SseEvent) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        let payload: Value = match serde_json::from_str(&event.data) {
            Ok(value) => value,
            Err(_) => {
                // 非 JSON 的 data（例如 `[DONE]`）直接忽略。
                return out;
            }
        };

        let kind = event
            .event
            .clone()
            .or_else(|| {
                payload
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_default();

        match kind.as_str() {
            "message_start" => {
                if let Some(usage) = payload.pointer("/message/usage") {
                    self.usage = parse_usage(usage);
                    out.push(ProviderEvent::Usage(self.usage));
                }
            }
            "content_block_start" => {
                let block = &payload["content_block"];
                let block_index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                let block_type = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match block_type {
                    "text" => {
                        if let Some(text) = block.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            out.push(ProviderEvent::TextDelta(text.to_owned()));
                        }
                    }
                    "thinking" => {
                        if let Some(text) = block.get("thinking").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            out.push(ProviderEvent::ThinkingDelta(text.to_owned()));
                        }
                        if let Some(signature) = block.get("signature").and_then(Value::as_str)
                            && !signature.is_empty()
                        {
                            out.push(ProviderEvent::ThinkingSignature(signature.to_owned()));
                        }
                    }
                    "tool_use" => {
                        out.push(ProviderEvent::ToolCallStart {
                            index: block_index,
                            id: block["id"].as_str().unwrap_or_default().to_owned(),
                            name: block["name"].as_str().unwrap_or_default().to_owned(),
                        });
                    }
                    "server_tool_use" => {
                        out.push(ProviderEvent::ServerToolUse {
                            id: block["id"].as_str().unwrap_or_default().to_owned(),
                            name: block["name"].as_str().unwrap_or_default().to_owned(),
                            input: block.get("input").cloned().unwrap_or(Value::Null),
                        });
                    }
                    "web_search_tool_result" | "web_fetch_tool_result" => {
                        if let Some((tool_use_id, content)) =
                            server_tools::normalize_anthropic_result(block)
                        {
                            out.push(ProviderEvent::ServerToolResult {
                                tool_use_id,
                                content,
                            });
                        }
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let delta = &payload["delta"];
                let block_index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                let delta_type = delta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match delta_type {
                    "text_delta" => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            out.push(ProviderEvent::TextDelta(text.to_owned()));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            out.push(ProviderEvent::ThinkingDelta(text.to_owned()));
                        }
                    }
                    "signature_delta" => {
                        if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                            out.push(ProviderEvent::ThinkingSignature(signature.to_owned()));
                        }
                    }
                    "input_json_delta" => {
                        if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                            out.push(ProviderEvent::ToolCallArgumentsDelta {
                                index: block_index,
                                delta: partial.to_owned(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(usage) = payload.get("usage") {
                    let output = usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(self.usage.output_tokens);
                    self.usage.output_tokens = output;
                    out.push(ProviderEvent::Usage(self.usage));
                }
                if let Some(reason) = payload
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(map_stop_reason(reason));
                }
                // `stop_details` 可能出现在 delta 或顶层，两处都兼容。
                let details = payload
                    .pointer("/delta/stop_details")
                    .or_else(|| payload.get("stop_details"));
                if let Some(details) = details {
                    self.refusal = Some(refusal_from(details));
                }
                if let Some(reason) = self.stop_reason
                    && reason == StopReason::Refusal
                    && self.refusal.is_none()
                {
                    self.refusal = Some(RefusalDetails::default());
                }
            }
            "message_stop" => {
                if !self.stop_sent {
                    self.stop_sent = true;
                    out.push(ProviderEvent::Stop {
                        reason: self.stop_reason.unwrap_or(StopReason::EndTurn),
                        refusal: self.refusal.take(),
                    });
                }
            }
            "error" => {
                let message = payload
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("provider reported an error")
                    .to_owned();
                out.push(ProviderEvent::Error(ProviderError::new(
                    super::ProviderErrorKind::Server,
                    message,
                )));
                if !self.stop_sent {
                    self.stop_sent = true;
                    out.push(ProviderEvent::Stop {
                        reason: StopReason::Error,
                        refusal: None,
                    });
                }
            }
            // ping 与其它事件不产生输出。
            _ => {}
        }
        out
    }
}

fn refusal_from(details: &Value) -> RefusalDetails {
    RefusalDetails {
        category: details
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        explanation: details
            .get("explanation")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }
}

fn parse_usage(value: &Value) -> Usage {
    Usage {
        input_tokens: value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_tokens: value
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write_tokens: value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    fn protocol(&self) -> Protocol {
        Protocol::Anthropic
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn stream(&self, request: LlmRequest) -> Result<EventReceiver, ProviderError> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let body = Self::build_body(&request);

        let response = self
            .client
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                ProviderError::new(super::ProviderErrorKind::Network, error.to_string())
            })?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::from_status(status.as_u16(), &text));
        }

        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            pump(response, tx).await;
        });
        Ok(rx)
    }
}

/// 持续读取网络流并转发解析后的事件。
async fn pump(response: reqwest::Response, tx: mpsc::Sender<ProviderEvent>) {
    let mut stream = response.bytes_stream();
    let mut parser = SseParser::new();
    let mut state = StreamState::default();
    let sender = Arc::new(tx);

    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = sender
                    .send(ProviderEvent::Error(ProviderError::new(
                        super::ProviderErrorKind::Network,
                        error.to_string(),
                    )))
                    .await;
                return;
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        for event in parser.push(&text) {
            for outgoing in state.handle(event) {
                if sender.send(outgoing).await.is_err() {
                    return;
                }
            }
        }
    }

    for event in parser.finish() {
        for outgoing in state.handle(event) {
            if sender.send(outgoing).await.is_err() {
                return;
            }
        }
    }

    if !state.stop_sent {
        let _ = sender
            .send(ProviderEvent::Error(ProviderError::new(
                super::ProviderErrorKind::Network,
                "stream ended without a stop event",
            )))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ServerToolKind;
    use crate::providers::sse::SseEvent;

    fn request() -> LlmRequest {
        LlmRequest {
            model: "claude-opus-5-5".to_owned(),
            system: vec![SystemBlock {
                text: "sys".to_owned(),
                cache: true,
            }],
            messages: vec![super::super::ProviderMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hi".to_owned(),
                }],
            }],
            tools: vec![super::super::ToolSpec {
                name: "Read".to_owned(),
                description: "read".to_owned(),
                input_schema: json!({"type":"object"}),
            }],
            max_tokens: 1024,
            thinking: ThinkingOptions {
                enabled: true,
                effort: Some("high".to_owned()),
            },
            server_tools: vec![ServerToolKind::WebSearch],
        }
    }

    #[test]
    fn body_includes_system_tools_and_thinking() {
        let body = AnthropicProvider::build_body(&request());
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["stream"], true);
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["tools"][0]["name"], "Read");
        assert_eq!(body["tools"][1]["type"], "web_search_20260209");
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn tool_roundtrip_blocks_are_byte_stable() {
        let blocks = [
            ContentBlock::ToolUse {
                id: "t1".to_owned(),
                name: "Read".to_owned(),
                input: json!({"path": "/x"}),
            },
            ContentBlock::ToolResult {
                tool_use_id: "t1".to_owned(),
                content: "ok".to_owned(),
                is_error: false,
            },
            ContentBlock::Thinking {
                text: "think".to_owned(),
                signature: Some("sig".to_owned()),
            },
        ];
        let rendered: Vec<Value> = blocks.iter().map(block_json).collect();
        assert_eq!(rendered[0]["type"], "tool_use");
        assert_eq!(rendered[1]["type"], "tool_result");
        assert_eq!(rendered[2]["signature"], "sig");
    }

    fn feed(state: &mut StreamState, data: Value) -> Vec<ProviderEvent> {
        state.handle(SseEvent {
            event: Some(data["type"].as_str().unwrap_or_default().to_owned()),
            data: serde_json::to_string(&data).unwrap(),
        })
    }

    #[test]
    fn stream_text_usage_and_stop() {
        let mut state = StreamState::default();
        let events = feed(
            &mut state,
            json!({
                "type": "message_start",
                "message": {"usage": {"input_tokens": 12, "cache_read_input_tokens": 3}}
            }),
        );
        assert!(matches!(events[0], ProviderEvent::Usage(_)));

        let events = feed(
            &mut state,
            json!({
                "type": "content_block_delta",
                "delta": {"type": "text_delta", "text": "hello"}
            }),
        );
        assert_eq!(events, vec![ProviderEvent::TextDelta("hello".to_owned())]);

        let events = feed(
            &mut state,
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 7}
            }),
        );
        assert_eq!(
            events,
            vec![ProviderEvent::Usage(Usage {
                input_tokens: 12,
                output_tokens: 7,
                cache_read_tokens: 3,
                cache_write_tokens: 0,
            })]
        );

        let events = feed(&mut state, json!({"type": "message_stop"}));
        assert_eq!(
            events,
            vec![ProviderEvent::Stop {
                reason: StopReason::EndTurn,
                refusal: None
            }]
        );
        // 重复 stop 不再发送。
        assert!(feed(&mut state, json!({"type": "message_stop"})).is_empty());
    }

    #[test]
    fn tool_use_stream_is_split_into_start_and_arguments() {
        let mut state = StreamState::default();
        let events = feed(
            &mut state,
            json!({
                "type": "content_block_start",
                "content_block": {"type": "tool_use", "id": "t1", "name": "Read", "input": {}}
            }),
        );
        assert_eq!(
            events,
            vec![ProviderEvent::ToolCallStart {
                index: 0,
                id: "t1".to_owned(),
                name: "Read".to_owned()
            }]
        );
        let events = feed(
            &mut state,
            json!({
                "type": "content_block_delta",
                "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}
            }),
        );
        assert_eq!(
            events,
            vec![ProviderEvent::ToolCallArgumentsDelta {
                index: 0,
                delta: "{\"path\":".to_owned()
            }]
        );
    }

    #[test]
    fn refusal_details_are_captured() {
        let mut state = StreamState::default();
        // 没有 usage 字段时不产生事件，但拒绝细节要被记住。
        let _ = feed(
            &mut state,
            json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": "refusal",
                    "stop_details": {"category": "cyber", "explanation": "no"}
                }
            }),
        );

        let events = feed(&mut state, json!({"type": "message_stop"}));
        match &events[0] {
            ProviderEvent::Stop { reason, refusal } => {
                assert_eq!(*reason, StopReason::Refusal);
                assert_eq!(refusal.as_ref().unwrap().category, "cyber");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn server_tool_results_are_preserved() {
        let mut state = StreamState::default();
        let events = feed(
            &mut state,
            json!({
                "type": "content_block_start",
                "content_block": {
                    "type": "web_search_tool_result",
                    "tool_use_id": "srvtoolu_1",
                    "content": [{"type": "web_search_result", "url": "https://example.com"}]
                }
            }),
        );
        match &events[0] {
            ProviderEvent::ServerToolResult {
                tool_use_id,
                content,
            } => {
                assert_eq!(tool_use_id, "srvtoolu_1");
                assert_eq!(content["type"], "web_search_tool_result");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
