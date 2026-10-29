//! OpenAI Chat Completions 兼容协议（`protocol = "chat"`）。
//!
//! 设计依据：设计 §9.3。DeepSeek 等兼容服务走这条路径。
//!
//! 与 Anthropic 的差异：
//! - 系统提示是 `role: "system"` 消息；
//! - 工具调用在 `tool_calls` 里，参数以字符串分片下发；
//! - 工具结果用 `role: "tool"` + `tool_call_id`；
//! - 没有服务端工具，web 工具走用户配置的 HTTP 后端（Phase 4）。

use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::agent::message::{StopReason, Usage};
use crate::config::models::Protocol;

use super::sse::SseParser;
use super::{
    ContentBlock, EventReceiver, LlmRequest, Provider, ProviderError, ProviderErrorKind,
    ProviderEvent, Role, ThinkingOptions, map_stop_reason,
};

/// Chat Completions provider。
pub struct ChatProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model_id: String,
}

impl ChatProvider {
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

    /// 请求体构造。
    pub fn build_body(request: &LlmRequest) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        let system = system_text(&request.system);
        if !system.is_empty() {
            messages.push(json!({ "role": "system", "content": system }));
        }
        messages.extend(provider_messages(&request.messages));

        let mut body = json!({
            "model": request.model,
            "max_tokens": request.max_tokens,
            "stream": true,
            "messages": messages,
        });
        // 让兼容服务在流末尾返回用量；不支持的服务会忽略该字段。
        body["stream_options"] = json!({ "include_usage": true });

        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|spec| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": spec.name,
                            "description": spec.description,
                            "parameters": spec.input_schema,
                        }
                    })
                })
                .collect();
            body["tools"] = Value::Array(tools);
        }

        apply_reasoning(&mut body, &request.thinking);
        body
    }
}

fn system_text(blocks: &[super::SystemBlock]) -> String {
    blocks
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Chat 协议只有 `reasoning_effort`（low/medium/high），高等级向下取整。
fn apply_reasoning(body: &mut Value, thinking: &ThinkingOptions) {
    let Some(effort) = &thinking.effort else {
        return;
    };
    if !thinking.enabled && effort.is_empty() {
        return;
    }
    let mapped = match effort.as_str() {
        "low" => "low",
        "medium" => "medium",
        "high" | "xhigh" | "max" => "high",
        _ => return,
    };
    body["reasoning_effort"] = json!(mapped);
}

fn provider_messages(messages: &[super::ProviderMessage]) -> Vec<Value> {
    let mut out = Vec::new();
    for message in messages {
        match message.role {
            Role::User => {
                // Chat 协议没有独立的 tool 内容块：连续的 tool_result 要拆成多条 `role: tool`。
                let mut text_parts: Vec<String> = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => text_parts.push(text.clone()),
                        ContentBlock::Image { .. } => {
                            // Chat 的图片格式在不同兼容服务间差异很大；Phase 4 再按模型能力启用。
                            text_parts.push(
                                "[image omitted: this protocol does not accept inline images]"
                                    .to_owned(),
                            );
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            let body = if *is_error {
                                format!("Error: {content}")
                            } else {
                                content.clone()
                            };
                            out.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": body,
                            }));
                        }
                        _ => {}
                    }
                }
                if !text_parts.is_empty() {
                    out.push(json!({ "role": "user", "content": text_parts.join("\n\n") }));
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text: value } => text.push_str(value),
                        ContentBlock::ToolUse { id, name, input } => tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".to_owned()),
                            }
                        })),
                        // 思考块不回传：兼容服务普遍不接受。
                        _ => {}
                    }
                }
                let mut message = json!({ "role": "assistant" });
                if !text.is_empty() {
                    message["content"] = json!(text);
                }
                if !tool_calls.is_empty() {
                    message["tool_calls"] = Value::Array(tool_calls);
                }
                out.push(message);
            }
        }
    }
    out
}

#[derive(Debug, Default)]
struct StreamState {
    usage: Usage,
    stop_reason: Option<StopReason>,
    stop_sent: bool,
}

impl StreamState {
    fn handle(&mut self, data: &str) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        if data.trim() == "[DONE]" {
            self.emit_stop(&mut out);
            return out;
        }
        let Ok(payload) = serde_json::from_str::<Value>(data) else {
            return out;
        };

        if let Some(error) = payload.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider reported an error");
            out.push(ProviderEvent::Error(ProviderError::new(
                ProviderErrorKind::Server,
                message,
            )));
            // 错误事件必须把 stop 原因标成 Error，而不是默认 EndTurn。
            self.stop_reason = Some(StopReason::Error);
            self.emit_stop(&mut out);
            return out;
        }

        if let Some(usage) = payload.get("usage").filter(|value| !value.is_null()) {
            self.usage = Usage {
                input_tokens: usage
                    .get("prompt_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                output_tokens: usage
                    .get("completion_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache_read_tokens: usage
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache_write_tokens: 0,
            };
            out.push(ProviderEvent::Usage(self.usage));
        }

        let Some(choice) = payload.get("choices").and_then(|choices| choices.get(0)) else {
            return out;
        };
        let delta = &choice["delta"];

        if let Some(text) = delta.get("content").and_then(Value::as_str)
            && !text.is_empty() {
                out.push(ProviderEvent::TextDelta(text.to_owned()));
            }
        if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str)
            && !reasoning.is_empty() {
                out.push(ProviderEvent::ThinkingDelta(reasoning.to_owned()));
            }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
                let name = call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                // 只有同时拿到 id 与 name 才是“调用开始”；后续分片只带 arguments。
                if !id.is_empty() && !name.is_empty() {
                    out.push(ProviderEvent::ToolCallStart {
                        index,
                        id: id.to_owned(),
                        name: name.to_owned(),
                    });
                }
                if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str)
                    && !arguments.is_empty() {
                        out.push(ProviderEvent::ToolCallArgumentsDelta {
                            index,
                            delta: arguments.to_owned(),
                        });
                    }
            }
        }

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop_reason = Some(map_stop_reason(reason));
        }
        out
    }

    fn emit_stop(&mut self, out: &mut Vec<ProviderEvent>) {
        if self.stop_sent {
            return;
        }
        self.stop_sent = true;
        out.push(ProviderEvent::Stop {
            reason: self.stop_reason.take().unwrap_or(StopReason::EndTurn),
            refusal: None,
        });
    }
}

#[async_trait::async_trait]
impl Provider for ChatProvider {
    fn protocol(&self) -> Protocol {
        Protocol::Chat
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn stream(&self, request: LlmRequest) -> Result<EventReceiver, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
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
        let mut tail = Vec::new();
        state.emit_stop(&mut tail);
        for outgoing in tail {
            let _ = sender.send(outgoing).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{SystemBlock, ToolSpec};

    fn request() -> LlmRequest {
        LlmRequest {
            model: "deepseek-chat".to_owned(),
            system: vec![SystemBlock {
                text: "sys".to_owned(),
                cache: false,
            }],
            messages: vec![
                super::super::ProviderMessage {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "hi".to_owned(),
                    }],
                },
                super::super::ProviderMessage {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "t1".to_owned(),
                        name: "Read".to_owned(),
                        input: json!({"path": "/x"}),
                    }],
                },
                super::super::ProviderMessage {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "t1".to_owned(),
                        content: "ok".to_owned(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![ToolSpec {
                name: "Read".to_owned(),
                description: "read".to_owned(),
                input_schema: json!({"type":"object"}),
            }],
            max_tokens: 256,
            thinking: ThinkingOptions {
                enabled: true,
                effort: Some("max".to_owned()),
            },
            server_tools: Vec::new(),
        }
    }

    #[test]
    fn body_uses_system_tool_and_reasoning_effort() {
        let body = ChatProvider::build_body(&request());
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["messages"][3]["tool_call_id"], "t1");
        assert_eq!(body["tools"][0]["type"], "function");
        // `max` 向下映射到 `high`。
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn streaming_text_tool_and_usage() {
        let mut state = StreamState::default();
        let events = state.handle(r#"{"choices":[{"delta":{"content":"he"}}]}"#);
        assert_eq!(events, vec![ProviderEvent::TextDelta("he".to_owned())]);

        let events = state.handle(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"Read","arguments":"{\"pa"}}]}}]}"#,
        );
        assert_eq!(
            events,
            vec![
                ProviderEvent::ToolCallStart {
                    index: 0,
                    id: "call_1".to_owned(),
                    name: "Read".to_owned()
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    index: 0,
                    delta: "{\"pa".to_owned()
                },
            ]
        );

        let events = state.handle(
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":6}}"#,
        );
        assert!(matches!(events[0], ProviderEvent::Usage(_)));
        assert!(!state.stop_sent);

        let events = state.handle("[DONE]");
        assert_eq!(
            events,
            vec![ProviderEvent::Stop {
                reason: StopReason::ToolUse,
                refusal: None
            }]
        );
    }

    #[test]
    fn content_filter_maps_to_refusal() {
        let mut state = StreamState::default();
        state.handle(r#"{"choices":[{"delta":{},"finish_reason":"content_filter"}]}"#);
        let events = state.handle("[DONE]");
        assert_eq!(
            events,
            vec![ProviderEvent::Stop {
                reason: StopReason::Refusal,
                refusal: None
            }]
        );
    }

    #[test]
    fn error_object_terminates_stream() {
        let mut state = StreamState::default();
        let events = state.handle(r#"{"error":{"message":"boom"}}"#);
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
