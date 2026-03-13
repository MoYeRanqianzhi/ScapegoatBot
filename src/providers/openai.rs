// src/providers/openai.rs
// OpenAI 兼容 API 提供商实现
// 支持两种 API 协议（通过 wire_api 配置切换）：
//   1. Chat Completions — POST /v1/chat/completions（经典协议，兼容大多数渠道）
//   2. Responses — POST /v1/responses（OpenAI 新一代协议）
//
// 所有请求均使用 stream: true 模式，防止反向代理/CDN 的网关超时（HTTP 504）。
// 流式传输保证服务器在生成过程中持续发送数据块，保持连接活跃。

use super::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    NormalizedStopReason, Provider, ProviderCapabilities, TokenUsage, ToolCall as ProviderToolCall,
    ToolSpec, ToolsPayload,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

// ============================================================================
// API 协议类型
// ============================================================================

/// API 协议选择
/// 不同渠道/提供商可能仅支持其中一种端点
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireApi {
    /// 经典 Chat Completions 协议：POST /v1/chat/completions
    /// 兼容大多数 OpenAI 兼容服务（DeepSeek、Moonshot、Ollama 等）
    ChatCompletions,
    /// 新一代 Responses 协议：POST /v1/responses
    /// OpenAI 于 2025 年推出，支持更丰富的 agentic 功能
    Responses,
}

impl WireApi {
    /// 从配置字符串解析协议类型
    /// 不区分大小写，不匹配时默认回退为 ChatCompletions
    pub fn from_config(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "responses" => Self::Responses,
            _ => Self::ChatCompletions,
        }
    }
}

// ============================================================================
// 提供商结构体
// ============================================================================

/// OpenAI 兼容 API 提供商
pub struct OpenAiProvider {
    /// API 基础 URL（不含尾部斜杠），如 "https://api.openai.com/v1"
    base_url: String,
    /// API 认证凭据（Bearer token）
    credential: Option<String>,
    /// 可选的 max_tokens 覆盖值
    max_tokens_override: Option<u32>,
    /// 可复用的 HTTP 客户端（内含连接池，已配置超时）
    client: Client,
    /// API 协议类型（chat_completions 或 responses）
    wire_api: WireApi,
}

// ============================================================================
// Chat Completions 请求结构体
// ============================================================================

/// CC: 基础聊天补全请求体（不含工具调用）
#[derive(Debug, Serialize)]
struct CcSimpleRequest {
    model: String,
    messages: Vec<CcApiMessage>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<CcStreamOptions>,
}

/// CC: 带原生工具调用的聊天补全请求体
#[derive(Debug, Serialize)]
struct CcNativeRequest {
    model: String,
    messages: Vec<CcNativeMessage>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<CcToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<CcStreamOptions>,
}

/// CC: 流式传输选项
#[derive(Debug, Serialize)]
struct CcStreamOptions {
    include_usage: bool,
}

/// CC: 基础消息格式
#[derive(Debug, Serialize)]
struct CcApiMessage {
    role: String,
    content: String,
}

/// CC: 原生消息格式（支持工具调用字段和多模态内容）
#[derive(Debug, Serialize)]
struct CcNativeMessage {
    role: String,
    /// 消息内容：纯文本时为 JSON 字符串，多模态时为 content array
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<CcToolCallPayload>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
}

/// CC: 工具规范（嵌套 function 对象格式）
#[derive(Debug, Serialize, Deserialize)]
struct CcToolSpec {
    #[serde(rename = "type")]
    kind: String,
    function: CcToolFunction,
}

#[derive(Debug, Serialize, Deserialize)]
struct CcToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

/// CC: 工具调用载荷
#[derive(Debug, Serialize, Deserialize)]
struct CcToolCallPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    function: CcFunctionCall,
}

#[derive(Debug, Serialize, Deserialize)]
struct CcFunctionCall {
    name: String,
    arguments: String,
}

// ============================================================================
// Responses API 请求结构体
// ============================================================================

/// Responses API 请求体
#[derive(Debug, Serialize)]
struct RespRequest {
    /// 模型名称
    model: String,
    /// 输入项列表（消息、工具调用、工具结果等）
    input: Vec<RespInputItem>,
    /// 系统级指令（替代 Chat Completions 的 system 消息）
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    /// 工具定义列表
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<RespToolSpec>>,
    /// 工具选择策略
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    /// 采样温度
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    /// 最大输出 token 数（Responses API 使用 max_output_tokens 而非 max_tokens）
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    /// 启用流式传输
    stream: bool,
    /// 禁用服务端存储（我们自行管理对话历史）
    store: bool,
}

/// Responses API 输入项（使用 serde 内部标签联合体）
/// 三种类型：消息、函数调用（来自上一轮助手输出）、函数调用结果
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum RespInputItem {
    /// 角色消息（user / assistant / system / developer）
    #[serde(rename = "message")]
    Message { role: String, content: String },
    /// 助手上一轮的函数调用（需要回传以保持上下文）
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// 函数调用结果（通过 call_id 关联）
    #[serde(rename = "function_call_output")]
    FunctionCallOutput { call_id: String, output: String },
    /// 多模态消息（含图片等非文本内容）
    /// content 为 serde_json::Value 数组，包含 input_text 和 input_image 项
    #[serde(rename = "message")]
    MultimodalMessage {
        role: String,
        content: Vec<serde_json::Value>,
    },
}

/// Responses API 工具定义（扁平结构，name/description/parameters 在顶层）
#[derive(Debug, Serialize)]
struct RespToolSpec {
    #[serde(rename = "type")]
    kind: String,
    name: String,
    description: String,
    parameters: serde_json::Value,
}

// ============================================================================
// 共享结构体
// ============================================================================

/// Token 用量信息（两种 API 共用）
#[derive(Debug, Deserialize)]
struct ApiUsageInfo {
    /// CC: prompt_tokens / Responses: input_tokens
    #[serde(default, alias = "input_tokens")]
    prompt_tokens: Option<u64>,
    /// CC: completion_tokens / Responses: output_tokens
    #[serde(default, alias = "output_tokens")]
    completion_tokens: Option<u64>,
}

// ============================================================================
// Chat Completions SSE 流式响应
// ============================================================================

/// CC: SSE 数据块
#[derive(Debug, Deserialize)]
struct CcStreamChunk {
    #[serde(default)]
    choices: Vec<CcStreamChoice>,
    #[serde(default)]
    usage: Option<ApiUsageInfo>,
}

#[derive(Debug, Deserialize)]
struct CcStreamChoice {
    delta: CcStreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CcStreamDelta {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<CcStreamToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct CcStreamToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    function: Option<CcStreamFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct CcStreamFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

// ============================================================================
// Chat Completions 流式累加器
// ============================================================================

/// 跨 chunk 累加的工具调用
struct AccToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// CC 流式响应累加器
struct CcStreamAccumulator {
    content: String,
    reasoning_content: String,
    tool_calls: Vec<AccToolCall>,
    finish_reason: Option<String>,
    usage: Option<ApiUsageInfo>,
}

impl CcStreamAccumulator {
    fn new() -> Self {
        Self {
            content: String::new(),
            reasoning_content: String::new(),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
        }
    }

    /// 处理一个 CC SSE chunk
    fn process_chunk(&mut self, chunk: CcStreamChunk) {
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage);
        }
        for choice in chunk.choices {
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
            let delta = choice.delta;
            if let Some(c) = delta.content {
                self.content.push_str(&c);
            }
            if let Some(r) = delta.reasoning_content {
                self.reasoning_content.push_str(&r);
            }
            if let Some(tcs) = delta.tool_calls {
                for tc in tcs {
                    while self.tool_calls.len() <= tc.index {
                        self.tool_calls.push(AccToolCall {
                            id: String::new(),
                            name: String::new(),
                            arguments: String::new(),
                        });
                    }
                    let acc = &mut self.tool_calls[tc.index];
                    if let Some(id) = tc.id {
                        acc.id = id;
                    }
                    if let Some(f) = tc.function {
                        if let Some(n) = f.name {
                            acc.name = n;
                        }
                        if let Some(a) = f.arguments {
                            acc.arguments.push_str(&a);
                        }
                    }
                }
            }
        }
    }
}

// ============================================================================
// Responses API 流式累加器
// ============================================================================

/// Responses API 流式累加的工具调用
struct RespAccToolCall {
    call_id: String,
    name: String,
    arguments: String,
}

/// Responses API 流式响应累加器
/// 使用 serde_json::Value 解析各类事件，保持灵活性和前向兼容
struct RespStreamAccumulator {
    content: String,
    reasoning_content: String,
    tool_calls: Vec<RespAccToolCall>,
    status: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    error_message: Option<String>,
}

impl RespStreamAccumulator {
    fn new() -> Self {
        Self {
            content: String::new(),
            reasoning_content: String::new(),
            tool_calls: Vec::new(),
            status: None,
            input_tokens: None,
            output_tokens: None,
            error_message: None,
        }
    }

    /// 处理一个 Responses API SSE 事件
    /// 事件格式：`event: <type>\ndata: <json>`
    fn process_event(&mut self, event_type: &str, data: &serde_json::Value) {
        match event_type {
            // 文本内容增量
            "response.output_text.delta" => {
                if let Some(delta) = data.get("delta").and_then(|d| d.as_str()) {
                    self.content.push_str(delta);
                }
            }
            // 新输出项添加（可能是 message 或 function_call）
            "response.output_item.added" => {
                if let Some(item) = data.get("item") {
                    if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                        let call_id = item
                            .get("call_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        self.tool_calls.push(RespAccToolCall {
                            call_id,
                            name,
                            arguments: String::new(),
                        });
                    }
                }
            }
            // 函数调用参数增量
            "response.function_call_arguments.delta" => {
                let call_id = data.get("call_id").and_then(|c| c.as_str()).unwrap_or("");
                let delta = data.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                if let Some(tc) = self
                    .tool_calls
                    .iter_mut()
                    .find(|tc| tc.call_id == call_id)
                {
                    tc.arguments.push_str(delta);
                }
            }
            // 输出项完成（包含完整的 function_call 数据）
            // 参考 codex: response.output_item.done 是获取完整工具调用的权威来源
            // 用完整数据覆盖 delta 累积的结果，防止 delta 累积失败导致参数丢失
            "response.output_item.done" => {
                if let Some(item) = data.get("item") {
                    if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                        let call_id = item
                            .get("call_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let arguments = item
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        // 查找已有的 delta 累积条目并用完整数据覆盖
                        if let Some(tc) = self
                            .tool_calls
                            .iter_mut()
                            .find(|tc| tc.call_id == call_id && !call_id.is_empty())
                        {
                            // 用完整数据覆盖 delta 累积的结果
                            tc.name = name;
                            tc.arguments = arguments;
                        } else {
                            // 没有匹配的 delta 条目（或 call_id 为空），直接添加完整条目
                            // 先移除可能存在的同名空 call_id 条目（delta 累积失败的残留）
                            self.tool_calls.retain(|tc| {
                                !(tc.name == name && tc.call_id.is_empty())
                            });
                            self.tool_calls.push(RespAccToolCall {
                                call_id,
                                name,
                                arguments,
                            });
                        }
                    }
                }
            }
            // 推理内容增量（思考型模型摘要）
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = data.get("delta").and_then(|d| d.as_str()) {
                    self.reasoning_content.push_str(delta);
                }
            }
            // 响应完成（包含最终 usage）
            // 参考 codex: status/usage 嵌套在 data.response 下
            "response.completed" => {
                let resp = data.get("response");
                self.status = resp
                    .and_then(|r| r.get("status"))
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
                    .or(Some("completed".to_string()));
                if let Some(usage) = resp.and_then(|r| r.get("usage")) {
                    self.input_tokens = usage.get("input_tokens").and_then(|v| v.as_u64());
                    self.output_tokens = usage.get("output_tokens").and_then(|v| v.as_u64());
                }
            }
            // 响应不完整（如达到 token 上限）
            // 参考 codex: incomplete_details 嵌套在 data.response 下
            "response.incomplete" => {
                self.status = Some("incomplete".to_string());
                // 提取不完整原因用于日志
                if let Some(reason) = data
                    .get("response")
                    .and_then(|r| r.get("incomplete_details"))
                    .and_then(|d| d.get("reason"))
                    .and_then(|r| r.as_str())
                {
                    tracing::warn!("Resp: 响应不完整，原因: {reason}");
                }
            }
            // 响应失败
            // 参考 codex: error 嵌套在 data.response 下
            "response.failed" => {
                self.status = Some("failed".to_string());
                if let Some(err) = data
                    .get("response")
                    .and_then(|r| r.get("error"))
                {
                    self.error_message = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .map(|s| s.to_string());
                    // 提取错误码用于日志
                    let code = err
                        .get("code")
                        .and_then(|c| c.as_str())
                        .unwrap_or("unknown");
                    tracing::error!(
                        "Resp: 响应失败 code={}, msg={}",
                        code,
                        self.error_message.as_deref().unwrap_or("(无)")
                    );
                }
            }
            // 忽略其他事件（前向兼容）
            _ => {}
        }
    }
}

// ============================================================================
// API 错误响应
// ============================================================================

/// 两种 API 共用的错误响应格式
#[derive(Debug, Deserialize)]
struct ApiErrorResponse {
    error: ApiErrorDetail,
}

#[derive(Debug, Deserialize)]
struct ApiErrorDetail {
    message: String,
    #[serde(default, rename = "type")]
    error_type: Option<String>,
}

// ============================================================================
// 多模态内容构建辅助函数
// ============================================================================

/// 将 ChatMessage 转换为 CC API 的 content 字段
/// - 无图片时返回纯字符串（或 None）
/// - 有图片时返回 content array（文本 + image_url 项）
fn build_cc_content(msg: &ChatMessage) -> Option<serde_json::Value> {
    if msg.images.is_empty() {
        // 纯文本：保持原有逻辑
        if msg.content.is_empty() && msg.role != "tool" {
            None
        } else {
            Some(serde_json::Value::String(msg.content.clone()))
        }
    } else {
        // 多模态：构建 content array
        let mut parts = Vec::new();
        // 文本部分（如果有）
        if !msg.content.is_empty() {
            parts.push(serde_json::json!({
                "type": "text",
                "text": msg.content
            }));
        }
        // 图片部分
        for img in &msg.images {
            parts.push(serde_json::json!({
                "type": "image_url",
                "image_url": {
                    "url": img.to_data_url()
                }
            }));
        }
        Some(serde_json::Value::Array(parts))
    }
}

/// 将 ChatMessage 转换为 Responses API 的输入项列表
/// 带图片的工具结果会生成多个输入项（文本 output + 图片 input_image）
fn build_resp_tool_output_items(msg: &ChatMessage) -> Vec<RespInputItem> {
    let call_id = msg.tool_call_id.clone().unwrap_or_default();
    let mut items = Vec::new();
    // 文本 output（始终发送，即使为空）
    items.push(RespInputItem::FunctionCallOutput {
        call_id: call_id.clone(),
        output: msg.content.clone(),
    });
    // 图片作为后续 user message 的 content_parts 注入
    // Responses API 不直接支持 function_call_output 中嵌入图片，
    // 所以我们将图片作为紧随其后的 user 消息发送
    if !msg.images.is_empty() {
        let mut parts = Vec::new();
        parts.push(serde_json::json!({
            "type": "input_text",
            "text": format!("[工具 {} 返回的图片]", call_id)
        }));
        for img in &msg.images {
            parts.push(serde_json::json!({
                "type": "input_image",
                "image_url": img.to_data_url()
            }));
        }
        items.push(RespInputItem::MultimodalMessage {
            role: "user".to_string(),
            content: parts,
        });
    }
    items
}

// ============================================================================
// OpenAiProvider 实现
// ============================================================================

impl OpenAiProvider {
    /// 创建一个新的 OpenAI 兼容提供商实例
    ///
    /// # 参数
    /// - `base_url`: API 基础 URL，None 时默认为 "https://api.openai.com/v1"
    /// - `credential`: API Key（Bearer token），None 时不发送认证头
    /// - `max_tokens_override`: 可选的 max_tokens 覆盖值
    /// - `timeout_secs`: HTTP 请求超时时间（秒）
    /// - `wire_api_str`: API 协议类型字符串（"chat_completions" 或 "responses"）
    pub fn new(
        base_url: Option<&str>,
        credential: Option<&str>,
        max_tokens_override: Option<u32>,
        timeout_secs: u64,
        wire_api_str: &str,
    ) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .unwrap_or_else(|_| Client::new());

        let wire_api = WireApi::from_config(wire_api_str);
        tracing::info!(
            "LLM 提供商初始化: wire_api={:?}, timeout={}s",
            wire_api,
            timeout_secs
        );

        Self {
            base_url: base_url
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            credential: credential.map(ToString::to_string),
            max_tokens_override: max_tokens_override.filter(|v| *v > 0),
            client,
            wire_api,
        }
    }

    /// 构建 Authorization 头
    fn auth_header(&self) -> Option<String> {
        self.credential
            .as_ref()
            .map(|key| format!("Bearer {key}"))
    }

    // ========================================================================
    // Chat Completions 路径
    // ========================================================================

    /// CC: 转换工具规范
    fn cc_convert_tools(tools: Option<&[ToolSpec]>) -> Option<Vec<CcToolSpec>> {
        tools.map(|items| {
            items
                .iter()
                .map(|s| CcToolSpec {
                    kind: "function".to_string(),
                    function: CcToolFunction {
                        name: s.name.clone(),
                        description: s.description.clone(),
                        parameters: s.parameters.clone(),
                    },
                })
                .collect()
        })
    }

    /// CC: 解析 SSE 响应（格式：`data: {json}\n`）
    fn parse_cc_sse(response_text: &str) -> CcStreamAccumulator {
        let mut acc = CcStreamAccumulator::new();
        for line in response_text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                break;
            }
            match serde_json::from_str::<CcStreamChunk>(data) {
                Ok(chunk) => acc.process_chunk(chunk),
                Err(e) => tracing::warn!("CC: 跳过无法解析的 chunk: {e}"),
            }
        }
        acc
    }

    /// CC: 发送基础请求（流式），返回文本
    async fn cc_send_simple(
        &self,
        messages: Vec<CcApiMessage>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let body = CcSimpleRequest {
            model: model.to_string(),
            messages,
            temperature,
            max_tokens: self.max_tokens_override,
            stream: true,
            stream_options: Some(CcStreamOptions {
                include_usage: true,
            }),
        };
        let url = format!("{}/chat/completions", self.base_url);
        tracing::info!("CC 请求: POST {} (model={}, stream=true)", url, model);
        let mut req = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }

        let response = req.send().await?;
        let status = response.status();
        tracing::info!("CC 响应: HTTP {}", status);
        let text = response.text().await?;
        if !status.is_success() {
            let msg = serde_json::from_str::<ApiErrorResponse>(&text)
                .map(|e| e.error.message)
                .unwrap_or_else(|_| text.clone());
            anyhow::bail!("OpenAI API 错误: HTTP {status}: {msg}");
        }

        let acc = Self::parse_cc_sse(&text);
        // 输出 token 用量
        if let Some(ref usage) = acc.usage {
            tracing::info!(
                "CC 用量: input={}, output={}",
                usage.prompt_tokens.unwrap_or(0),
                usage.completion_tokens.unwrap_or(0),
            );
        }
        Ok(if !acc.content.is_empty() {
            acc.content
        } else {
            acc.reasoning_content
        })
    }

    /// CC: 发送带工具调用的请求（流式），返回结构化响应
    async fn cc_send_native(
        &self,
        messages: Vec<CcNativeMessage>,
        tools: Option<Vec<CcToolSpec>>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let tool_count = tools.as_ref().map(|t| t.len()).unwrap_or(0);
        let body = CcNativeRequest {
            model: model.to_string(),
            messages,
            temperature,
            max_tokens: self.max_tokens_override,
            tools,
            tool_choice: None,
            stream: true,
            stream_options: Some(CcStreamOptions {
                include_usage: true,
            }),
        };
        let url = format!("{}/chat/completions", self.base_url);
        tracing::info!(
            "CC 请求: POST {} (model={}, tools={}, stream=true)",
            url, model, tool_count
        );
        let mut req = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }

        let response = req.send().await?;
        let status = response.status();
        tracing::info!("CC 响应: HTTP {}", status);
        let text = response.text().await?;
        if !status.is_success() {
            let msg = serde_json::from_str::<ApiErrorResponse>(&text)
                .map(|e| e.error.message)
                .unwrap_or_else(|_| text.clone());
            anyhow::bail!("OpenAI API 错误: HTTP {status}: {msg}");
        }

        let acc = Self::parse_cc_sse(&text);
        let usage = acc.usage.map(|u| {
            tracing::info!(
                "CC 用量: input={}, output={}",
                u.prompt_tokens.unwrap_or(0),
                u.completion_tokens.unwrap_or(0),
            );
            TokenUsage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
            }
        });
        let text_content = if !acc.content.is_empty() {
            Some(acc.content)
        } else if !acc.reasoning_content.is_empty() {
            Some(acc.reasoning_content.clone())
        } else {
            None
        };
        let reasoning_content = if !acc.reasoning_content.is_empty() {
            Some(acc.reasoning_content)
        } else {
            None
        };
        let tool_calls: Vec<ProviderToolCall> = acc
            .tool_calls
            .into_iter()
            .filter(|tc| !tc.id.is_empty())
            .map(|tc| ProviderToolCall {
                id: tc.id,
                name: tc.name,
                arguments: tc.arguments,
            })
            .collect();
        let raw_stop = acc.finish_reason;
        let stop_reason = raw_stop
            .as_deref()
            .map(NormalizedStopReason::from_openai_finish_reason);
        // 输出响应摘要
        if !tool_calls.is_empty() {
            let names: Vec<&str> = tool_calls.iter().map(|tc| tc.name.as_str()).collect();
            tracing::info!("CC 结果: {} 个工具调用 {:?}", tool_calls.len(), names);
        } else if let Some(ref t) = text_content {
            tracing::info!("CC 结果: 文本 {} 字符", t.len());
        } else {
            tracing::info!("CC 结果: 无内容");
        }

        Ok(ProviderChatResponse {
            text: text_content,
            tool_calls,
            usage,
            reasoning_content,
            stop_reason,
            raw_stop_reason: raw_stop,
        })
    }

    // ========================================================================
    // Responses API 路径
    // ========================================================================

    /// Resp: 转换工具规范（扁平结构：name/description/parameters 在顶层）
    fn resp_convert_tools(tools: Option<&[ToolSpec]>) -> Option<Vec<RespToolSpec>> {
        tools.map(|items| {
            items
                .iter()
                .map(|s| RespToolSpec {
                    kind: "function".to_string(),
                    name: s.name.clone(),
                    description: s.description.clone(),
                    parameters: s.parameters.clone(),
                })
                .collect()
        })
    }

    /// Resp: 将 ChatMessage 列表转换为 Responses API 输入格式
    /// 返回 (instructions, input_items)
    /// system/developer 消息被提取为 instructions，其余转为对应的输入项类型
    fn messages_to_resp_input(messages: &[ChatMessage]) -> (Option<String>, Vec<RespInputItem>) {
        let mut instructions: Option<String> = None;
        let mut input = Vec::new();

        for msg in messages {
            match msg.role.as_str() {
                // system / developer 消息 → instructions 参数
                "system" | "developer" => {
                    instructions = Some(msg.content.clone());
                }
                // 工具结果 → function_call_output 输入项（可能含图片）
                "tool" => {
                    if msg.tool_call_id.is_some() {
                        let items = build_resp_tool_output_items(msg);
                        input.extend(items);
                    }
                }
                // user / assistant 消息
                _ => {
                    // 文本内容 → message 输入项
                    if !msg.content.is_empty() {
                        input.push(RespInputItem::Message {
                            role: msg.role.clone(),
                            content: msg.content.clone(),
                        });
                    }
                    // 助手的工具调用 → function_call 输入项（需要回传以保持上下文）
                    for tc in &msg.tool_calls {
                        input.push(RespInputItem::FunctionCall {
                            call_id: tc.id.clone(),
                            name: tc.name.clone(),
                            arguments: tc.arguments.clone(),
                        });
                    }
                }
            }
        }

        (instructions, input)
    }

    /// Resp: 解析 SSE 响应（格式：`event: <type>\ndata: <json>`）
    fn parse_resp_sse(response_text: &str) -> RespStreamAccumulator {
        let mut acc = RespStreamAccumulator::new();
        let mut current_event = String::new();

        for line in response_text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // 解析事件类型行
            if let Some(evt) = line.strip_prefix("event: ") {
                current_event = evt.trim().to_string();
            }
            // 解析数据行
            else if let Some(data) = line.strip_prefix("data: ") {
                let data = data.trim();
                if !current_event.is_empty() {
                    match serde_json::from_str::<serde_json::Value>(data) {
                        Ok(v) => acc.process_event(&current_event, &v),
                        Err(e) => {
                            tracing::warn!("Resp: 跳过无法解析的事件数据: {e}");
                        }
                    }
                    current_event.clear();
                }
            }
        }

        acc
    }

    /// Resp: 发送请求并解析流式响应，返回统一的 ProviderChatResponse
    async fn resp_send(
        &self,
        input: Vec<RespInputItem>,
        tools: Option<Vec<RespToolSpec>>,
        instructions: Option<String>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let tool_count = tools.as_ref().map(|t| t.len()).unwrap_or(0);
        let input_count = input.len();
        let body = RespRequest {
            model: model.to_string(),
            input,
            instructions,
            tools,
            tool_choice: None,
            temperature: Some(temperature),
            max_output_tokens: self.max_tokens_override,
            stream: true,
            store: false, // 我们自行管理对话历史
        };
        let url = format!("{}/responses", self.base_url);
        tracing::info!(
            "Resp 请求: POST {} (model={}, input_items={}, tools={}, stream=true)",
            url, model, input_count, tool_count
        );
        let mut req = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }

        let response = req.send().await?;
        let status = response.status();
        tracing::info!("Resp 响应: HTTP {}", status);
        let text = response.text().await?;
        if !status.is_success() {
            let msg = serde_json::from_str::<ApiErrorResponse>(&text)
                .map(|e| e.error.message)
                .unwrap_or_else(|_| text.clone());
            anyhow::bail!("OpenAI Responses API 错误: HTTP {status}: {msg}");
        }

        // 解析 SSE 流式响应
        let acc = Self::parse_resp_sse(&text);

        // 检查是否有流内错误
        if acc.status.as_deref() == Some("failed") {
            let err = acc
                .error_message
                .unwrap_or_else(|| "unknown error".to_string());
            anyhow::bail!("OpenAI Responses API 错误: {err}");
        }

        // 转换为统一响应格式
        let usage = if acc.input_tokens.is_some() || acc.output_tokens.is_some() {
            tracing::info!(
                "Resp 用量: input={}, output={}",
                acc.input_tokens.unwrap_or(0),
                acc.output_tokens.unwrap_or(0),
            );
            Some(TokenUsage {
                input_tokens: acc.input_tokens,
                output_tokens: acc.output_tokens,
            })
        } else {
            None
        };
        let text_content = if !acc.content.is_empty() {
            Some(acc.content)
        } else if !acc.reasoning_content.is_empty() {
            Some(acc.reasoning_content.clone())
        } else {
            None
        };
        let reasoning_content = if !acc.reasoning_content.is_empty() {
            Some(acc.reasoning_content)
        } else {
            None
        };
        let tool_calls: Vec<ProviderToolCall> = acc
            .tool_calls
            .into_iter()
            .filter(|tc| !tc.call_id.is_empty())
            .map(|tc| ProviderToolCall {
                id: tc.call_id,
                name: tc.name,
                arguments: tc.arguments,
            })
            .collect();
        // Responses API 的停止原因来自 status 字段
        let has_tool_calls = !tool_calls.is_empty();
        // 输出响应摘要
        if has_tool_calls {
            let names: Vec<&str> = tool_calls.iter().map(|tc| tc.name.as_str()).collect();
            tracing::info!("Resp 结果: {} 个工具调用 {:?}", tool_calls.len(), names);
        } else if let Some(ref t) = text_content {
            tracing::info!("Resp 结果: 文本 {} 字符", t.len());
        } else {
            tracing::info!("Resp 结果: 无内容");
        }
        let stop_reason = if has_tool_calls {
            Some(NormalizedStopReason::ToolCall)
        } else {
            match acc.status.as_deref() {
                Some("completed") => Some(NormalizedStopReason::EndTurn),
                Some("incomplete") => Some(NormalizedStopReason::MaxTokens),
                _ => None,
            }
        };
        let raw_stop = acc.status;

        Ok(ProviderChatResponse {
            text: text_content,
            tool_calls,
            usage,
            reasoning_content,
            stop_reason,
            raw_stop_reason: raw_stop,
        })
    }
}

// ============================================================================
// Provider Trait 实现
// ============================================================================

#[async_trait]
impl Provider for OpenAiProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
        }
    }

    fn convert_tools(&self, tools: &[ToolSpec]) -> ToolsPayload {
        let native: Vec<serde_json::Value> = tools
            .iter()
            .map(|spec| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": spec.name,
                        "description": spec.description,
                        "parameters": spec.parameters,
                    }
                })
            })
            .collect();
        ToolsPayload::OpenAI { tools: native }
    }

    /// 带可选系统提示的单轮对话
    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        match self.wire_api {
            WireApi::ChatCompletions => {
                let mut messages = Vec::new();
                if let Some(system) = system_prompt {
                    messages.push(CcApiMessage {
                        role: "system".to_string(),
                        content: system.to_string(),
                    });
                }
                messages.push(CcApiMessage {
                    role: "user".to_string(),
                    content: message.to_string(),
                });
                self.cc_send_simple(messages, model, temperature).await
            }
            WireApi::Responses => {
                let instructions = system_prompt.map(|s| s.to_string());
                let input = vec![RespInputItem::Message {
                    role: "user".to_string(),
                    content: message.to_string(),
                }];
                let resp = self
                    .resp_send(input, None, instructions, model, temperature)
                    .await?;
                Ok(resp.text.unwrap_or_default())
            }
        }
    }

    /// 多轮对话（不含工具调用）
    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        match self.wire_api {
            WireApi::ChatCompletions => {
                let api_messages: Vec<CcApiMessage> = messages
                    .iter()
                    .map(|m| CcApiMessage {
                        role: m.role.clone(),
                        content: m.content.clone(),
                    })
                    .collect();
                self.cc_send_simple(api_messages, model, temperature).await
            }
            WireApi::Responses => {
                let (instructions, input) = Self::messages_to_resp_input(messages);
                let resp = self
                    .resp_send(input, None, instructions, model, temperature)
                    .await?;
                Ok(resp.text.unwrap_or_default())
            }
        }
    }

    /// 结构化聊天 API（支持工具调用）
    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let has_tools = request.tools.is_some_and(|t| !t.is_empty());

        // ---- Responses API 路径 ----
        if self.wire_api == WireApi::Responses {
            let (instructions, input) = Self::messages_to_resp_input(request.messages);
            let tools = Self::resp_convert_tools(request.tools);
            return self
                .resp_send(input, tools, instructions, model, temperature)
                .await;
        }

        // ---- Chat Completions 路径 ----
        if has_tools {
            // 将 ChatMessage 转换为 CC 原生消息格式
            let native_messages: Vec<CcNativeMessage> = request
                .messages
                .iter()
                .map(|m| {
                    let tool_calls = if m.tool_calls.is_empty() {
                        None
                    } else {
                        Some(
                            m.tool_calls
                                .iter()
                                .map(|tc| CcToolCallPayload {
                                    id: Some(tc.id.clone()),
                                    kind: Some("function".to_string()),
                                    function: CcFunctionCall {
                                        name: tc.name.clone(),
                                        arguments: tc.arguments.clone(),
                                    },
                                })
                                .collect(),
                        )
                    };
                    CcNativeMessage {
                        role: m.role.clone(),
                        content: build_cc_content(m),
                        tool_call_id: m.tool_call_id.clone(),
                        tool_calls,
                        reasoning_content: None,
                    }
                })
                .collect();
            let native_tools = Self::cc_convert_tools(request.tools);
            self.cc_send_native(native_messages, native_tools, model, temperature)
                .await
        } else {
            let text = self
                .chat_with_history(request.messages, model, temperature)
                .await?;
            Ok(ProviderChatResponse {
                text: Some(text),
                tool_calls: Vec::new(),
                usage: None,
                reasoning_content: None,
                stop_reason: None,
                raw_stop_reason: None,
            })
        }
    }
}

// ============================================================================
// 便捷函数
// ============================================================================

/// 从环境变量创建默认配置的 OpenAI 提供商
pub fn from_env() -> OpenAiProvider {
    let api_key = std::env::var("OPENAI_API_KEY").ok();
    let base_url = std::env::var("OPENAI_BASE_URL").ok();
    OpenAiProvider::new(
        base_url.as_deref(),
        api_key.as_deref(),
        None,
        300,
        "chat_completions",
    )
}
