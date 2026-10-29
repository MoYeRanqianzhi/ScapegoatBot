//! OpenAI Responses 协议（`protocol = "openai"`）。
//!
//! 设计依据：设计 §9.3。
//!
//! 与 Chat Completions 的差异：
//! - 输入是 `input` 数组，函数调用与结果都是独立 item；
//! - 流事件带 `response.*` 前缀；
//! - 用量在 `response.completed` 里。

use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::agent::message::{RefusalDetails, StopReason, Usage};
use crate::config::models::Protocol;

use super::sse::SseParser;
use super::{
    ContentBlock, EventReceiver, LlmRequest, Provider, ProviderError, ProviderErrorKind,
    ProviderEvent, Role, ThinkingOptions,
};

/// Responses provider。
pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model_id: String,
}

impl OpenAiProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model_id: impl Into<String>,
        timeout_secs: u64,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
            .build()
            .map_err(|error| ProviderError::new(ProviderErrorKind::Config, error.to_string()))?;
        Ok(Self {
            client,
            base_url: base_url.into(),
            api_key: api_key.into(),
            model_id: model_id.into(),
        })
    }

    pub fn build_body(request: &LlmRequest) -> Value {
        let mut body = json!({
            "model": request.model,
            "max_output_tokens": request.max_tokens,
            "stream": true,
            "input": input_items(&request.messages),
        });

        let instructions = request
            .system
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        if !instructions.is_empty() {
            body["instructions"] = json!(instructions);
        }

        let mut tools: Vec<Value> = request
            .tools
            .iter()
            .map(|spec| {
                json!({
                    "type": "function",
                    "name": spec.name,
                    "description": spec.description,
                    "parameters": spec.input_schema,
                })
            })
            .collect();
        for kind in &request.server_tools {
            tools.push(super::server_tools::openai_decl(*kind));
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }

        apply_reasoning(&mut body, &request.thinking);
        body
    }
}

fn apply_reasoning(body: &mut Value, thinking: &ThinkingOptions) {
    let Some(effort) = &thinking.effort else {
        return;
    };
    let mapped = match effort.as_str() {
        "low" => "low",
        "medium" => "medium",
        "high" | "xhigh" | "max" => "high",
        _ => return,
    };
    body["reasoning"] = json!({ "effort": mapped });
}

fn input_items(messages: &[super::ProviderMessage]) -> Vec<Value> {
    let mut out = Vec::new();
    for message in messages {
        match message.role {
            Role::User => {
                let mut parts: Vec<Value> = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => {
                            parts.push(json!({ "type": "input_text", "text": text }))
                        }
                        ContentBlock::Image {
                            mime_type,
                            data_base64,
                        } => parts.push(json!({
                            "type": "input_image",
                            "image_url": format!("data:{mime_type};base64,{data_base64}"),
                        })),
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            let output = if *is_error {
                                format!("Error: {content}")
                            } else {
                                content.clone()
                            };
                            out.push(json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": output,
                            }));
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    out.push(json!({ "role": "user", "content": parts }));
                }
            }
            Role::Assistant => {
                let mut parts: Vec<Value> = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => {
                            parts.push(json!({ "type": "output_text", "text": text }))
                        }
                        ContentBlock::ToolUse { id, name, input } => out.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": serde_json::to_string(input)
                                .unwrap_or_else(|_| "{}".to_owned()),
                        })),
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    out.push(json!({ "role": "assistant", "content": parts }));
                }
            }
        }
    }
    out
}

#[derive(Debug, Default)]
struct StreamState {
    usage: Usage,
    stop_sent: bool,
    saw_tool_call: bool,
    /// 当前打开的 function_call：`(调用序号, call_id, 已累计参数长度)`。
    open_call: Option<(u32, String, usize)>,
    /// 下一个调用的序号；Responses 事件本身不带 index。
    next_index: u32,
}

impl StreamState {
    fn handle(&mut self, data: &str) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        let Ok(payload) = serde_json::from_str::<Value>(data) else {
            return out;
        };
        let kind = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        match kind {
            "response.output_item.added" => {
                let item = &payload["item"];
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .or_else(|| item.get("id").and_then(Value::as_str))
                        .unwrap_or_default()
                        .to_owned();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    self.saw_tool_call = true;
                    let index = self.next_index;
                    self.next_index += 1;
                    self.open_call = Some((index, call_id.clone(), 0));
                    out.push(ProviderEvent::ToolCallStart {
                        index,
                        id: call_id,
                        name,
                    });
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                    let index = {
                        let Some((index, _, length)) = &mut self.open_call else {
                            return out;
                        };
                        *length += delta.len();
                        *index
                    };
                    out.push(ProviderEvent::ToolCallArgumentsDelta {
                        index,
                        delta: delta.to_owned(),
                    });
                }
            }
            "response.function_call_arguments.done" => {
                // 若服务端只给完整参数（没有 delta），在这里补发一次。
                if let Some(arguments) = payload.get("arguments").and_then(Value::as_str)
                    && let Some((index, _, length)) = &self.open_call
                    && *length == 0
                    && !arguments.is_empty()
                {
                    out.push(ProviderEvent::ToolCallArgumentsDelta {
                        index: *index,
                        delta: arguments.to_owned(),
                    });
                }
                self.open_call = None;
            }
            "response.output_text.delta" => {
                if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                    out.push(ProviderEvent::TextDelta(delta.to_owned()));
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                    out.push(ProviderEvent::ThinkingDelta(delta.to_owned()));
                }
            }
            "response.completed" => {
                if let Some(usage) = payload.pointer("/response/usage") {
                    self.usage = Usage {
                        input_tokens: usage
                            .get("input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        output_tokens: usage
                            .get("output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        cache_read_tokens: usage
                            .pointer("/input_tokens_details/cached_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        cache_write_tokens: 0,
                    };
                    out.push(ProviderEvent::Usage(self.usage));
                }
                self.emit_stop(&mut out, StopReason::EndTurn);
            }
            "response.incomplete" => {
                self.emit_stop(&mut out, StopReason::MaxTokens);
            }
            "response.failed" | "error" => {
                let message = payload
                    .pointer("/response/error/message")
                    .or_else(|| payload.pointer("/error/message"))
                    .and_then(Value::as_str)
                    .unwrap_or("provider reported an error");
                out.push(ProviderEvent::Error(ProviderError::new(
                    ProviderErrorKind::Server,
                    message,
                )));
                self.emit_stop(&mut out, StopReason::Error);
            }
            _ => {}
        }

        // 响应对象带的 status 也能结束流（部分兼容实现不发 response.completed）。
        if let Some(status) = payload.pointer("/response/status").and_then(Value::as_str) {
            match status {
                "completed" => self.emit_stop(&mut out, StopReason::EndTurn),
                "incomplete" => self.emit_stop(&mut out, StopReason::MaxTokens),
                "failed" => self.emit_stop(&mut out, StopReason::Error),
                _ => {}
            }
        }

        out
    }

    fn emit_stop(&mut self, out: &mut Vec<ProviderEvent>, reason: StopReason) {
        if self.stop_sent {
            return;
        }
        self.stop_sent = true;
        let reason = if reason == StopReason::EndTurn && self.saw_tool_call {
            StopReason::ToolUse
        } else {
            reason
        };
        out.push(ProviderEvent::Stop {
            reason,
            refusal: None::<RefusalDetails>,
        });
    }
}

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    fn protocol(&self) -> Protocol {
        Protocol::Openai
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn stream(&self, request: LlmRequest) -> Result<EventReceiver, ProviderError> {
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let body = Self::build_body(&request);

        let response = self
            .client
            .post(url)
            .header("authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
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
            pump(response, tx).await;
        });
        Ok(rx)
    }
}

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
                        ProviderErrorKind::Network,
                        error.to_string(),
                    )))
                    .await;
                return;
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        for event in parser.push(&text) {
            for outgoing in state.handle(&event.data) {
                if sender.send(outgoing).await.is_err() {
                    return;
                }
            }
        }
    }
    for event in parser.finish() {
        for outgoing in state.handle(&event.data) {
            if sender.send(outgoing).await.is_err() {
                return;
            }
        }
    }
    if !state.stop_sent {
        let _ = sender
            .send(ProviderEvent::Error(ProviderError::new(
                ProviderErrorKind::Network,
                "stream ended without a stop event",
            )))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{SystemBlock, ToolSpec};

    fn request() -> LlmRequest {
        LlmRequest {
            model: "gpt-test".to_owned(),
            system: vec![SystemBlock {
                text: "sys".to_owned(),
                cache: false,
            }],
            messages: vec![super::super::ProviderMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hi".to_owned(),
                }],
            }],
            tools: vec![ToolSpec {
                name: "Read".to_owned(),
                description: "read".to_owned(),
                input_schema: json!({"type":"object"}),
            }],
            max_tokens: 512,
            thinking: ThinkingOptions {
                enabled: true,
                effort: Some("xhigh".to_owned()),
            },
            server_tools: vec![super::super::ServerToolKind::WebSearch],
        }
    }

    #[test]
    fn body_uses_instructions_flat_tools_and_reasoning() {
        let body = OpenAiProvider::build_body(&request());
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "Read");
        assert_eq!(body["tools"][1]["type"], "web_search");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["max_output_tokens"], 512);
    }

    fn feed(state: &mut StreamState, value: Value) -> Vec<ProviderEvent> {
        state.handle(&value.to_string())
    }

    #[test]
    fn function_call_stream() {
        let mut state = StreamState::default();
        let events = feed(
            &mut state,
            json!({
                "type": "response.output_item.added",
                "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Read"}
            }),
        );
        assert_eq!(
            events,
            vec![ProviderEvent::ToolCallStart {
                index: 0,
                id: "call_1".to_owned(),
                name: "Read".to_owned()
            }]
        );

        let events = feed(
            &mut state,
            json!({"type": "response.function_call_arguments.delta", "delta": "{\"path\""}),
        );
        assert_eq!(
            events,
            vec![ProviderEvent::ToolCallArgumentsDelta {
                index: 0,
                delta: "{\"path\"".to_owned()
            }]
        );

        let events = feed(
            &mut state,
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 3, "output_tokens": 4}}}),
        );
        assert!(matches!(events[0], ProviderEvent::Usage(_)));
        assert_eq!(
            events[1],
            ProviderEvent::Stop {
                reason: StopReason::ToolUse,
                refusal: None
            }
        );
    }

    #[test]
    fn text_stream_and_incomplete() {
        let mut state = StreamState::default();
        let events = feed(
            &mut state,
            json!({"type": "response.output_text.delta", "delta": "hello"}),
        );
        assert_eq!(events, vec![ProviderEvent::TextDelta("hello".to_owned())]);

        let events = feed(&mut state, json!({"type": "response.incomplete"}));
        assert_eq!(
            events,
            vec![ProviderEvent::Stop {
                reason: StopReason::MaxTokens,
                refusal: None
            }]
        );
    }

    #[test]
    fn failure_is_reported_and_stops() {
        let mut state = StreamState::default();
        let events = feed(
            &mut state,
            json!({"type": "response.failed", "response": {"error": {"message": "bad"}}}),
        );
        assert!(matches!(events[0], ProviderEvent::Error(_)));
        assert_eq!(
            events[1],
            ProviderEvent::Stop {
                reason: StopReason::Error,
                refusal: None
            }
        );
    }
}
