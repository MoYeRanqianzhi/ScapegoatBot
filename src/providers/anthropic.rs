//! Anthropic Messages 协议（默认）。
//!
//! 设计依据：设计 §3.2 / §9.3；协议版本与字段以官方当前文档为准：
//! `anthropic-version: 2026-10-21`，思考用 `thinking: {type: "adaptive"}`，
//! effort 用 `output_config.effort`（`budget_tokens` 在 Opus 5.5 上会 400）。

use serde_json::{Value, json};

use crate::config::models::Protocol;
use crate::llm::types::{RefusalDetails, StopReason, Usage};

use super::{
    Auth, ContentBlock, EventReceiver, HttpProviderCore, LlmRequest, Provider, ProviderError,
    ProviderEvent, Role, SystemBlock, ThinkingOptions, map_stop_reason, server_tools,
};

/// Anthropic API 版本头。
pub const API_VERSION: &str = "2026-10-21";
/// 默认服务端工具最大调用次数。
const SERVER_TOOL_MAX_USES: u32 = 10;

/// Anthropic provider。
pub struct AnthropicProvider {
    core: HttpProviderCore,
}

impl AnthropicProvider {
    /// 构造；`timeout_secs` 来自 `models.json` 的 provider 配置。
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model_id: impl Into<String>,
        timeout_secs: u64,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            core: HttpProviderCore::new(base_url, api_key, model_id, timeout_secs)?,
        })
    }

    /// 请求体构造，独立成函数便于单元测试。
    pub fn build_body(request: &LlmRequest) -> Value {
        let mut messages = messages_json(&request.messages);
        if request.cache_history {
            mark_history_cacheable(&mut messages);
        }
        let mut body = json!({
            "model": request.model,
            "max_tokens": request.max_tokens,
            "stream": true,
            "messages": messages,
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

/// 历史 → `messages`。
///
/// 回放时跳过 API 会以 400 拒绝的块：空白文本块、没有签名的 thinking 块（例如回退链里
/// 别的协议产生的推理摘要，或流中断时没拿到签名）。一条消息因此变空时整条跳过——
/// Messages API 会把相邻的同角色消息合并成一轮，跳过不会破坏轮次交替；空消息里也
/// 不可能有 `tool_use`，工具调用与结果的配对不受影响。
fn messages_json(messages: &[super::ProviderMessage]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for message in messages {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let content: Vec<Value> = message
            .content
            .iter()
            .filter(|block| replayable(block))
            .map(block_json)
            .collect();
        if content.is_empty() {
            continue;
        }
        out.push(json!({ "role": role, "content": content }));
    }
    out
}

/// 这个块能否原样回放给 Anthropic。
fn replayable(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Text { text } => !text.trim().is_empty(),
        ContentBlock::Thinking { signature, .. } => signature
            .as_deref()
            .is_some_and(|signature| !signature.is_empty()),
        _ => true,
    }
}

/// 在最后一条消息的最后一个块上打缓存断点，让整段对话历史成为可缓存前缀。
///
/// 下一次请求只在末尾追加了几个块；服务端会在断点之前约 20 个块边界内查找已写入的
/// 前缀，于是工具循环的每一步都命中上一步写下的缓存。thinking 块不能带
/// `cache_control`，遇到时不打断点（静态 system 前缀的断点仍在）。
fn mark_history_cacheable(messages: &mut [Value]) {
    let Some(block) = messages
        .last_mut()
        .and_then(|message| message.get_mut("content"))
        .and_then(Value::as_array_mut)
        .and_then(|content| content.last_mut())
    else {
        return;
    };
    let kind = block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if matches!(kind, "thinking" | "redacted_thinking") {
        return;
    }
    block["cache_control"] = json!({ "type": "ephemeral" });
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
    /// 正在接收参数的服务端工具调用（`server_tool_use` 块）。
    server_tool: Option<PendingServerTool>,
}

/// 服务端工具调用的参数和普通工具一样，通过 `input_json_delta` 分段到达
/// （`content_block_start` 里的 `input` 是空对象）；块结束时才拼出完整参数。
#[derive(Debug)]
struct PendingServerTool {
    index: u32,
    id: String,
    name: String,
    /// `content_block_start` 自带的 `input`；没有收到任何分段时用它。
    initial: Value,
    json: String,
}

impl PendingServerTool {
    /// 拼好的参数事件；分段 JSON 解析失败时退回块开始时的 `input` 并记日志。
    fn into_event(self) -> ProviderEvent {
        let input = if self.json.trim().is_empty() {
            self.initial
        } else {
            match serde_json::from_str(&self.json) {
                Ok(input) => input,
                Err(error) => {
                    tracing::warn!(%error, tool = %self.name, "cannot parse server tool input");
                    self.initial
                }
            }
        };
        ProviderEvent::ServerToolUse {
            id: self.id,
            name: self.name,
            input,
        }
    }
}

impl StreamState {
    /// 处理一个 SSE 事件，返回要发送给上层的事件。
    ///
    /// 事件名优先取 SSE 的 `event:` 行，没有时用 payload 的 `type` 字段；非 JSON 的
    /// `data`（例如 `[DONE]`）与 `ping` 等未知事件不产生输出。
    fn handle(&mut self, event: super::sse::SseEvent) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        let Ok(payload) = serde_json::from_str::<Value>(&event.data) else {
            return out;
        };
        let kind = event
            .event
            .or_else(|| str_field(&payload, "type").map(str::to_owned))
            .unwrap_or_default();
        match kind.as_str() {
            "message_start" => {
                if let Some(usage) = payload.pointer("/message/usage") {
                    self.usage = parse_usage(usage);
                    out.push(ProviderEvent::Usage(self.usage));
                }
            }
            "content_block_start" => self.block_start(&payload, &mut out),
            "content_block_delta" => self.block_delta(&payload, &mut out),
            "content_block_stop" => {
                let index = block_index(&payload);
                if self
                    .server_tool
                    .as_ref()
                    .is_some_and(|pending| pending.index == index)
                    && let Some(pending) = self.server_tool.take()
                {
                    out.push(pending.into_event());
                }
            }
            "message_delta" => self.message_delta(&payload, &mut out),
            "message_stop" => {
                let reason = self.stop_reason.unwrap_or(StopReason::EndTurn);
                let refusal = self.refusal.take();
                self.push_stop(reason, refusal, &mut out);
            }
            "error" => {
                let message = payload
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("provider reported an error");
                out.push(ProviderEvent::Error(ProviderError::new(
                    super::ProviderErrorKind::Server,
                    message.to_owned(),
                )));
                self.push_stop(StopReason::Error, None, &mut out);
            }
            _ => {}
        }
        out
    }

    /// `content_block_start`：文字与 thinking 块可能自带开头内容；`tool_use` 开始一个工具
    /// 调用；`server_tool_use` 的参数要等分段到齐，先挂起；服务端工具的结果块原样转交。
    fn block_start(&mut self, payload: &Value, out: &mut Vec<ProviderEvent>) {
        let block = &payload["content_block"];
        let index = block_index(payload);
        match str_field(block, "type").unwrap_or_default() {
            "text" => {
                if let Some(text) = non_empty(block, "text") {
                    out.push(ProviderEvent::TextDelta(text.to_owned()));
                }
            }
            "thinking" => {
                if let Some(text) = non_empty(block, "thinking") {
                    out.push(ProviderEvent::ThinkingDelta(text.to_owned()));
                }
                if let Some(signature) = non_empty(block, "signature") {
                    out.push(ProviderEvent::ThinkingSignature(signature.to_owned()));
                }
            }
            "tool_use" => out.push(ProviderEvent::ToolCallStart {
                index,
                id: str_field(block, "id").unwrap_or_default().to_owned(),
                name: str_field(block, "name").unwrap_or_default().to_owned(),
            }),
            "server_tool_use" => {
                // 上一个没有收到结束事件的调用先发出去，不丢。
                if let Some(previous) = self.server_tool.take() {
                    out.push(previous.into_event());
                }
                self.server_tool = Some(PendingServerTool {
                    index,
                    id: str_field(block, "id").unwrap_or_default().to_owned(),
                    name: str_field(block, "name").unwrap_or_default().to_owned(),
                    initial: block.get("input").cloned().unwrap_or(Value::Null),
                    json: String::new(),
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

    /// `content_block_delta`：文字 / thinking / 签名分段直接转发；`input_json_delta` 属于
    /// 挂起的服务端工具时留在这里拼接，否则是普通工具调用的参数分段。
    fn block_delta(&mut self, payload: &Value, out: &mut Vec<ProviderEvent>) {
        let delta = &payload["delta"];
        let index = block_index(payload);
        match str_field(delta, "type").unwrap_or_default() {
            "text_delta" => {
                if let Some(text) = str_field(delta, "text") {
                    out.push(ProviderEvent::TextDelta(text.to_owned()));
                }
            }
            "thinking_delta" => {
                if let Some(text) = str_field(delta, "thinking") {
                    out.push(ProviderEvent::ThinkingDelta(text.to_owned()));
                }
            }
            "signature_delta" => {
                if let Some(signature) = str_field(delta, "signature") {
                    out.push(ProviderEvent::ThinkingSignature(signature.to_owned()));
                }
            }
            "input_json_delta" => {
                let Some(partial) = str_field(delta, "partial_json") else {
                    return;
                };
                match &mut self.server_tool {
                    Some(pending) if pending.index == index => pending.json.push_str(partial),
                    _ => out.push(ProviderEvent::ToolCallArgumentsDelta {
                        index,
                        delta: partial.to_owned(),
                    }),
                }
            }
            _ => {}
        }
    }

    /// `message_delta`：更新输出 token 数，记下停止原因与拒绝详情（`Stop` 在
    /// `message_stop` 时才发出）。
    fn message_delta(&mut self, payload: &Value, out: &mut Vec<ProviderEvent>) {
        if let Some(usage) = payload.get("usage") {
            if let Some(output) = usage.get("output_tokens").and_then(Value::as_u64) {
                self.usage.output_tokens = output;
            }
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
        // 拒绝但没有详情时也给一个空的详情，上层据此区分拒绝与普通结束。
        if self.stop_reason == Some(StopReason::Refusal) && self.refusal.is_none() {
            self.refusal = Some(RefusalDetails::default());
        }
    }

    /// 发出 `Stop`；一个流只发一次（`error` 之后的 `message_stop` 不再重复）。
    fn push_stop(
        &mut self,
        reason: StopReason,
        refusal: Option<RefusalDetails>,
        out: &mut Vec<ProviderEvent>,
    ) {
        if !self.stop_sent {
            self.stop_sent = true;
            out.push(ProviderEvent::Stop { reason, refusal });
        }
    }
}

/// 内容块事件的 `index`；缺失或不是整数时按 0 处理。
fn block_index(payload: &Value) -> u32 {
    payload.get("index").and_then(Value::as_u64).unwrap_or(0) as u32
}

/// `value[key]` 的字符串值。
fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `value[key]` 的非空字符串值。
fn non_empty<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    str_field(value, key).filter(|text| !text.is_empty())
}

impl super::StreamDecoder for StreamState {
    fn decode(&mut self, event: super::sse::SseEvent) -> Vec<ProviderEvent> {
        self.handle(event)
    }

    fn finish(&mut self) -> Vec<ProviderEvent> {
        // 收到一半的服务端工具调用也要交出去：结果块可能已经到了，配对不能断。
        let mut out: Vec<ProviderEvent> = self
            .server_tool
            .take()
            .map(PendingServerTool::into_event)
            .into_iter()
            .collect();
        if !self.stop_sent {
            out.push(ProviderEvent::Error(ProviderError::new(
                super::ProviderErrorKind::Network,
                "stream ended without a stop event",
            )));
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
        self.core.model_id()
    }

    async fn stream(&self, request: LlmRequest) -> Result<EventReceiver, ProviderError> {
        let body = Self::build_body(&request);
        self.core
            .send_stream(
                "/v1/messages",
                Auth::Anthropic(API_VERSION),
                &body,
                StreamState::default(),
            )
            .await
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
            cache_history: false,
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

    /// thinking 块（开始时自带的文字与签名、后续分段）、text 块自带的开头文字、`error`
    /// 事件只发一次 `Stop`，以及非 JSON 数据与缺少 `event:` 行时按 `type` 字段分派。
    #[test]
    fn thinking_text_starts_and_error_events() {
        let mut state = StreamState::default();
        let events = feed(
            &mut state,
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "thinking", "thinking": "hm", "signature": "s0"}
            }),
        );
        assert_eq!(
            events,
            vec![
                ProviderEvent::ThinkingDelta("hm".to_owned()),
                ProviderEvent::ThinkingSignature("s0".to_owned()),
            ]
        );
        let events = feed(
            &mut state,
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "thinking_delta", "thinking": "more"}}),
        );
        assert_eq!(
            events,
            vec![ProviderEvent::ThinkingDelta("more".to_owned())]
        );
        let events = feed(
            &mut state,
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "signature_delta", "signature": "s1"}}),
        );
        assert_eq!(
            events,
            vec![ProviderEvent::ThinkingSignature("s1".to_owned())]
        );
        // 空的开头文字不产生事件。
        let events = feed(
            &mut state,
            json!({"type": "content_block_start", "index": 1,
                   "content_block": {"type": "text", "text": ""}}),
        );
        assert!(events.is_empty());
        let events = feed(
            &mut state,
            json!({"type": "content_block_start", "index": 2,
                   "content_block": {"type": "text", "text": "hi"}}),
        );
        assert_eq!(events, vec![ProviderEvent::TextDelta("hi".to_owned())]);

        // 非 JSON 的 data 忽略；没有 `event:` 行时用 payload 的 `type`。
        assert!(
            state
                .handle(SseEvent {
                    event: None,
                    data: "[DONE]".to_owned()
                })
                .is_empty()
        );
        let events = state.handle(SseEvent {
            event: None,
            data: json!({"type": "error", "error": {"message": "overloaded"}}).to_string(),
        });
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(
            matches!(&events[0], ProviderEvent::Error(error) if error.to_string().contains("overloaded")),
            "{events:?}"
        );
        assert_eq!(
            events[1],
            ProviderEvent::Stop {
                reason: StopReason::Error,
                refusal: None
            }
        );
        // `Stop` 已发出：之后的 `message_stop` 与第二个 `error` 都不再发 `Stop`。
        assert!(feed(&mut state, json!({"type": "message_stop"})).is_empty());
        let events = feed(&mut state, json!({"type": "error", "error": {}}));
        assert_eq!(events.len(), 1, "{events:?}");
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
    fn server_tool_input_is_assembled_from_deltas() {
        let mut state = StreamState::default();
        let start = feed(
            &mut state,
            json!({
                "type": "content_block_start", "index": 1,
                "content_block": {"type": "server_tool_use", "id": "srvtoolu_1",
                                  "name": "web_search", "input": {}}
            }),
        );
        assert!(
            start.is_empty(),
            "the call is emitted once its input is complete"
        );
        for part in ["{\"query\": \"rust ", "2024 edition\"}"] {
            let events = feed(
                &mut state,
                json!({"type": "content_block_delta", "index": 1,
                       "delta": {"type": "input_json_delta", "partial_json": part}}),
            );
            assert!(
                events.is_empty(),
                "server tool arguments are not tool-call deltas"
            );
        }
        let events = feed(
            &mut state,
            json!({"type": "content_block_stop", "index": 1}),
        );
        assert_eq!(
            events,
            vec![ProviderEvent::ServerToolUse {
                id: "srvtoolu_1".to_owned(),
                name: "web_search".to_owned(),
                input: json!({"query": "rust 2024 edition"}),
            }]
        );
    }

    #[test]
    fn unreplayable_blocks_are_dropped_and_history_is_cacheable() {
        let mut request = request();
        request.messages = vec![
            super::super::ProviderMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hi".to_owned(),
                }],
            },
            // 只有空白文本和没签名的 thinking：整条跳过。
            super::super::ProviderMessage {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        text: "summary from another protocol".to_owned(),
                        signature: None,
                    },
                    ContentBlock::Text {
                        text: "  ".to_owned(),
                    },
                ],
            },
            super::super::ProviderMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "again".to_owned(),
                }],
            },
        ];
        request.cache_history = true;
        let body = AnthropicProvider::build_body(&request);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(messages.iter().all(|message| message["role"] == "user"));
        // 断点打在最后一个块上；一次性请求不打。
        assert_eq!(
            messages[1]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        request.cache_history = false;
        let body = AnthropicProvider::build_body(&request);
        assert!(
            body["messages"][1]["content"][0]
                .get("cache_control")
                .is_none()
        );
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
