// src/providers/anthropic.rs
// Anthropic Messages API 提供商实现
// 对接 Anthropic 官方 API（Claude 系列模型），支持：
//   - SSE 流式传输（所有请求强制 stream: true，防止网关超时）
//   - 原生工具调用（tool_use / tool_result 内容块）
//   - 多模态输入（base64 图片嵌入用户消息和工具结果）
//   - Prompt 缓存（系统提示、对话末尾、工具定义三层 cache_control）
//   - 双路认证（常规 API key 和 OAuth setup token）
//
// 参考 zeroclaw 的 Anthropic 实现，适配 ScapegoatBot 的架构差异：
// - ScapegoatBot 的 ChatMessage 有原生 tool_calls / tool_call_id / images 字段
// - 所有 API 请求使用 SSE 流式传输（zeroclaw 用阻塞调用）
// - Provider trait 已有 from_anthropic_stop_reason() 和 ToolsPayload::Anthropic 钩子

use super::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    NormalizedStopReason, Provider, ProviderCapabilities, TokenUsage,
    ToolCall as ProviderToolCall, ToolSpec, ToolsPayload,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;
use std::time::Duration;

// ============================================================================
// 提供商结构体
// ============================================================================

/// Anthropic Messages API 提供商
/// 对接 https://api.anthropic.com/v1/messages 端点
pub struct AnthropicProvider {
    /// API 基础 URL（不含尾部斜杠），默认 "https://api.anthropic.com"
    base_url: String,
    /// API 认证凭据（API key 或 OAuth setup token）
    credential: Option<String>,
    /// 最大输出 token 数（Anthropic API 强制要求此字段）
    max_tokens: u32,
    /// 可复用的 HTTP 客户端（内含连接池，已配置超时）
    client: Client,
}

// ============================================================================
// Anthropic 线格式类型（私有，仅用于序列化请求体）
// ============================================================================

/// Anthropic Messages API 请求体
#[derive(Debug, Serialize)]
struct AnthropicRequest {
    /// 模型名称（如 "claude-sonnet-4-20250514"）
    model: String,
    /// 最大输出 token 数（Anthropic 强制要求）
    max_tokens: u32,
    /// 系统提示（顶层字段，与 messages 分离 — Anthropic 的关键差异）
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<SystemPrompt>,
    /// 对话消息列表（严格 user/assistant 交替）
    messages: Vec<NativeMessage>,
    /// 生成温度
    temperature: f64,
    /// 可用工具定义列表
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<NativeToolSpec>>,
    /// 始终为 true — 强制 SSE 流式传输
    stream: bool,
}

/// 系统提示格式（支持缓存控制）
/// Anthropic 的系统提示可以是纯字符串或带 cache_control 的文本块数组
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum SystemPrompt {
    /// 短文本直接作为字符串传递
    Plain(String),
    /// 长文本（> 3072 字节）使用 block 数组 + cache_control: ephemeral
    Blocks(Vec<SystemBlock>),
}

/// 系统提示文本块（带可选缓存控制）
#[derive(Debug, Serialize)]
struct SystemBlock {
    /// 块类型，始终为 "text"
    #[serde(rename = "type")]
    block_type: String,
    /// 文本内容
    text: String,
    /// 缓存控制标记（ephemeral = 会话级缓存）
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

/// 对话消息（Anthropic 原生格式）
#[derive(Debug, Serialize)]
struct NativeMessage {
    /// 角色：仅 "user" 或 "assistant"（Anthropic 不支持 system/tool 角色）
    role: String,
    /// 内容块列表（文本、图片、工具调用、工具结果均为内容块）
    content: Vec<ContentBlockOut>,
}

/// 输出内容块（请求体中发送给 API 的格式）
/// 使用 serde tag = "type" 实现 Anthropic 的内容块多态
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ContentBlockOut {
    /// 文本块
    #[serde(rename = "text")]
    Text {
        /// 文本内容
        text: String,
        /// 缓存控制（对话末尾的最后一个块添加以启用 prompt 缓存）
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// 图片块（base64 内联）
    #[serde(rename = "image")]
    Image {
        /// 图片数据源
        source: ImageSource,
    },
    /// 工具调用块（assistant 消息中，LLM 请求执行工具）
    #[serde(rename = "tool_use")]
    ToolUse {
        /// 工具调用唯一 ID（用于关联后续的 tool_result）
        id: String,
        /// 工具名称
        name: String,
        /// 工具参数（已解析的 JSON Value，非字符串）
        input: serde_json::Value,
        /// 缓存控制
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// 工具结果块（user 消息中，回传工具执行结果）
    #[serde(rename = "tool_result")]
    ToolResult {
        /// 对应的 tool_use ID
        tool_use_id: String,
        /// 结果内容（纯文本或多模态块数组）
        content: ToolResultContent,
        /// 缓存控制
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

/// 工具结果内容（支持纯文本和多模态）
/// 使用 serde untagged 实现：字符串 → Text，数组 → Blocks
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ToolResultContent {
    /// 纯文本结果（大多数工具使用）
    Text(String),
    /// 多模态结果（文本 + 图片，如 ReadImage 工具）
    Blocks(Vec<ToolResultBlock>),
}

/// 工具结果中的内容块（多模态时使用）
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ToolResultBlock {
    /// 文本块
    #[serde(rename = "text")]
    Text {
        /// 文本内容
        text: String,
    },
    /// 图片块
    #[serde(rename = "image")]
    Image {
        /// 图片数据源
        source: ImageSource,
    },
}

/// 图片数据源（base64 内联编码）
#[derive(Debug, Serialize)]
struct ImageSource {
    /// 数据类型，始终为 "base64"
    #[serde(rename = "type")]
    kind: &'static str,
    /// MIME 类型（如 "image/png"、"image/jpeg"）
    media_type: String,
    /// base64 编码的图片数据
    data: String,
}

/// 工具定义规范（Anthropic 原生格式）
/// 注意：Anthropic 使用 input_schema 而非 OpenAI 的 parameters
#[derive(Debug, Serialize)]
struct NativeToolSpec {
    /// 工具名称
    name: String,
    /// 工具描述
    description: String,
    /// 参数的 JSON Schema（Anthropic 字段名为 input_schema）
    input_schema: serde_json::Value,
    /// 缓存控制（最后一个工具添加以批量缓存所有工具定义）
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

/// 缓存控制标记
/// Anthropic 的 prompt 缓存通过在内容块上添加 cache_control 实现
/// ephemeral = 会话级缓存，在相同前缀的后续请求中命中
#[derive(Debug, Clone, Serialize)]
struct CacheControl {
    /// 缓存类型，当前仅支持 "ephemeral"
    #[serde(rename = "type")]
    cache_type: String,
}

impl CacheControl {
    /// 创建 ephemeral 缓存控制标记
    fn ephemeral() -> Self {
        Self {
            cache_type: "ephemeral".to_string(),
        }
    }
}

// ============================================================================
// API 错误响应格式
// ============================================================================

/// Anthropic API 错误响应
/// 格式：{ "type": "error", "error": { "type": "...", "message": "..." } }
#[derive(Debug, serde::Deserialize)]
struct ApiErrorResponse {
    /// 错误详情
    error: ApiErrorDetail,
}

/// API 错误详情
#[derive(Debug, serde::Deserialize)]
struct ApiErrorDetail {
    /// 人类可读的错误消息
    message: String,
}

// ============================================================================
// SSE 流式累加器
// ============================================================================

/// 流式累加中的工具调用
struct AccToolCall {
    /// 工具调用 ID
    id: String,
    /// 工具名称
    name: String,
    /// JSON 格式的工具参数（从 input_json_delta 逐步累加）
    json_input: String,
}

/// Anthropic SSE 流式响应累加器
/// 逐事件累加文本内容、工具调用、用量统计和停止原因
///
/// Anthropic SSE 格式使用 `event: <type>\ndata: <json>` 双行结构，
/// 与 OpenAI 的 `data: {json}` 单行格式不同
struct AnthropicStreamAccumulator {
    /// 累积的文本内容
    content: String,
    /// 累积的工具调用列表
    tool_calls: Vec<AccToolCall>,
    /// 停止原因（end_turn / tool_use / max_tokens 等）
    stop_reason: Option<String>,
    /// 输入 token 数（来自 message_start 事件）
    input_tokens: Option<u64>,
    /// 输出 token 数（来自 message_delta 事件）
    output_tokens: Option<u64>,
    /// 流内错误消息
    error_message: Option<String>,
}

impl AnthropicStreamAccumulator {
    /// 创建空的累加器
    fn new() -> Self {
        Self {
            content: String::new(),
            tool_calls: Vec::new(),
            stop_reason: None,
            input_tokens: None,
            output_tokens: None,
            error_message: None,
        }
    }

    /// 处理一个 Anthropic SSE 事件
    ///
    /// Anthropic SSE 事件类型及作用：
    /// - message_start: 初始消息元数据 + usage.input_tokens
    /// - content_block_start: 开始新内容块（text 或 tool_use，含 index）
    /// - content_block_delta: 增量数据（text_delta.text 或 input_json_delta.partial_json）
    /// - content_block_stop: 结束当前内容块
    /// - message_delta: stop_reason + usage.output_tokens
    /// - message_stop: 流结束
    /// - error: 流内错误
    fn process_event(&mut self, event_type: &str, data: &serde_json::Value) {
        match event_type {
            // 消息开始：提取输入 token 用量
            "message_start" => {
                if let Some(usage) = data.get("message").and_then(|m| m.get("usage")) {
                    self.input_tokens =
                        usage.get("input_tokens").and_then(|v| v.as_u64());
                }
            }
            // 内容块开始：如果是 tool_use 类型，初始化新的工具调用
            "content_block_start" => {
                if let Some(block) = data.get("content_block") {
                    let block_type =
                        block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if block_type == "tool_use" {
                        // 提取工具调用的 id 和 name
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        self.tool_calls.push(AccToolCall {
                            id,
                            name,
                            json_input: String::new(),
                        });
                    }
                }
            }
            // 内容块增量：累加文本或工具参数 JSON
            "content_block_delta" => {
                if let Some(delta) = data.get("delta") {
                    let delta_type =
                        delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match delta_type {
                        // 文本增量 → 追加到 content
                        "text_delta" => {
                            if let Some(text) =
                                delta.get("text").and_then(|t| t.as_str())
                            {
                                self.content.push_str(text);
                            }
                        }
                        // 工具参数 JSON 增量 → 追加到最后一个工具调用的 json_input
                        "input_json_delta" => {
                            if let Some(json) = delta
                                .get("partial_json")
                                .and_then(|j| j.as_str())
                            {
                                if let Some(tc) = self.tool_calls.last_mut() {
                                    tc.json_input.push_str(json);
                                }
                            }
                        }
                        // 忽略未知的 delta 类型（前向兼容）
                        _ => {}
                    }
                }
            }
            // 内容块结束：无需操作，块已完整
            "content_block_stop" => {}
            // 消息增量：提取停止原因和输出 token 用量
            "message_delta" => {
                if let Some(delta) = data.get("delta") {
                    if let Some(reason) =
                        delta.get("stop_reason").and_then(|r| r.as_str())
                    {
                        self.stop_reason = Some(reason.to_string());
                    }
                }
                if let Some(usage) = data.get("usage") {
                    self.output_tokens =
                        usage.get("output_tokens").and_then(|v| v.as_u64());
                }
            }
            // 消息结束：流完成，无需操作
            "message_stop" => {}
            // 流内错误
            "error" => {
                if let Some(err) = data.get("error") {
                    let msg = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("未知错误");
                    self.error_message = Some(msg.to_string());
                    tracing::error!("Anthropic 流式错误: {}", msg);
                }
            }
            // 忽略其他事件类型（前向兼容）
            _ => {}
        }
    }
}

// ============================================================================
// AnthropicProvider 实现
// ============================================================================

impl AnthropicProvider {
    /// 创建一个新的 Anthropic 提供商实例
    ///
    /// # 参数
    /// - `base_url`: API 基础 URL，None 或空字符串时默认为 "https://api.anthropic.com"
    /// - `credential`: API Key 或 OAuth setup token，None 时不发送认证头
    /// - `max_tokens`: 最大输出 token 数（Anthropic API 强制要求）
    /// - `timeout_secs`: HTTP 请求超时时间（秒）
    pub fn new(
        base_url: Option<&str>,
        credential: Option<&str>,
        max_tokens: u32,
        timeout_secs: u64,
    ) -> Self {
        // 构建可复用的 HTTP 客户端（含超时配置）
        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .unwrap_or_else(|_| Client::new());

        // 处理 base_url：去除尾部斜杠，空值或空字符串使用 Anthropic 默认地址
        let base_url = base_url
            .map(|u| u.trim_end_matches('/'))
            .filter(|u| !u.is_empty())
            .unwrap_or("https://api.anthropic.com")
            .to_string();

        tracing::info!(
            "Anthropic 提供商初始化: base_url={}, max_tokens={}, timeout={}s",
            base_url,
            max_tokens,
            timeout_secs,
        );

        Self {
            base_url,
            credential: credential
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(ToString::to_string),
            max_tokens,
            client,
        }
    }

    // ========================================================================
    // 认证（双路径）
    // ========================================================================

    /// 判断凭据是否为 OAuth setup token
    /// OAuth setup token 以 "sk-ant-oat01-" 开头
    fn is_setup_token(token: &str) -> bool {
        token.starts_with("sk-ant-oat01-")
    }

    /// 为请求添加认证头
    /// - OAuth setup token → Authorization: Bearer + anthropic-beta 头
    /// - 常规 API key → x-api-key 头
    fn apply_auth(
        request: reqwest::RequestBuilder,
        credential: &str,
    ) -> reqwest::RequestBuilder {
        if Self::is_setup_token(credential) {
            // OAuth setup token：使用 Bearer 认证 + beta 特性标记
            request
                .header("Authorization", format!("Bearer {credential}"))
                .header("anthropic-beta", "oauth-2025-04-20")
        } else {
            // 常规 API key：使用 x-api-key 头
            request.header("x-api-key", credential)
        }
    }

    // ========================================================================
    // Prompt 缓存策略
    // ========================================================================

    /// 判断系统提示是否应启用缓存（超过 ~1024 token / 3KB 文本）
    fn should_cache_system(text: &str) -> bool {
        text.len() > 3072
    }

    /// 判断对话是否应启用末尾消息缓存（超过 4 条非系统消息）
    fn should_cache_conversation(messages: &[ChatMessage]) -> bool {
        messages
            .iter()
            .filter(|m| m.role != "system" && m.role != "developer")
            .count()
            > 4
    }

    /// 为最后一条消息的最后一个内容块添加 cache_control
    /// 缓存从前缀开始匹配，标记末尾使整个对话前缀可缓存
    fn apply_cache_to_last_message(messages: &mut [NativeMessage]) {
        if let Some(last_msg) = messages.last_mut() {
            if let Some(last_content) = last_msg.content.last_mut() {
                match last_content {
                    // 文本块和工具结果块支持 cache_control
                    ContentBlockOut::Text { cache_control, .. }
                    | ContentBlockOut::ToolResult { cache_control, .. }
                    | ContentBlockOut::ToolUse { cache_control, .. } => {
                        *cache_control = Some(CacheControl::ephemeral());
                    }
                    // 图片块不支持 cache_control
                    ContentBlockOut::Image { .. } => {}
                }
            }
        }
    }

    // ========================================================================
    // 工具转换
    // ========================================================================

    /// 将 ToolSpec 列表转换为 Anthropic 原生工具定义
    /// 最后一个工具添加 cache_control: ephemeral 以批量缓存所有工具定义
    fn convert_native_tools(tools: Option<&[ToolSpec]>) -> Option<Vec<NativeToolSpec>> {
        let items = tools?;
        if items.is_empty() {
            return None;
        }
        let mut native_tools: Vec<NativeToolSpec> = items
            .iter()
            .map(|tool| NativeToolSpec {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.parameters.clone(),
                cache_control: None,
            })
            .collect();
        // 为最后一个工具添加 cache_control（缓存从头开始的所有工具定义）
        if let Some(last_tool) = native_tools.last_mut() {
            last_tool.cache_control = Some(CacheControl::ephemeral());
        }
        Some(native_tools)
    }

    // ========================================================================
    // 消息转换
    // ========================================================================

    /// 将 ScapegoatBot 的 ChatMessage 列表转换为 Anthropic 原生格式
    ///
    /// 核心转换规则：
    /// - system / developer → 提取到顶层 system 字段（不进入 messages）
    /// - assistant（有 tool_calls）→ content: [Text? + ToolUse blocks]
    /// - tool（有 tool_call_id）→ user 角色, content: [ToolResult block]
    /// - user → user 角色, content: [Text + Image blocks]
    ///
    /// 转换后自动合并连续同角色消息（Anthropic 要求严格 user/assistant 交替）
    fn convert_messages(
        messages: &[ChatMessage],
    ) -> (Option<SystemPrompt>, Vec<NativeMessage>) {
        let mut system_text: Option<String> = None;
        let mut raw_messages: Vec<NativeMessage> = Vec::new();

        for msg in messages {
            match msg.role.as_str() {
                // system / developer → 提取到顶层 system 字段
                // 如果有多个系统消息，拼接而非覆盖（保留所有系统指令）
                "system" | "developer" => {
                    match &mut system_text {
                        Some(existing) => {
                            existing.push_str("\n\n");
                            existing.push_str(&msg.content);
                        }
                        None => {
                            system_text = Some(msg.content.clone());
                        }
                    }
                }
                // assistant 消息（可能包含工具调用）
                "assistant" => {
                    let mut blocks = Vec::new();
                    // 文本内容（如果有）
                    if !msg.content.is_empty() {
                        blocks.push(ContentBlockOut::Text {
                            text: msg.content.clone(),
                            cache_control: None,
                        });
                    }
                    // 工具调用块：将 ToolCall.arguments（JSON 字符串）解析为 Value
                    for tc in &msg.tool_calls {
                        let input =
                            serde_json::from_str::<serde_json::Value>(&tc.arguments)
                                .unwrap_or_else(|_| serde_json::json!({}));
                        blocks.push(ContentBlockOut::ToolUse {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            input,
                            cache_control: None,
                        });
                    }
                    // 确保至少有一个内容块（Anthropic 不接受空 content 数组）
                    if blocks.is_empty() {
                        blocks.push(ContentBlockOut::Text {
                            text: String::new(),
                            cache_control: None,
                        });
                    }
                    raw_messages.push(NativeMessage {
                        role: "assistant".to_string(),
                        content: blocks,
                    });
                }
                // tool 消息 → 转为 user 角色的 ToolResult 块
                "tool" => {
                    let tool_use_id =
                        msg.tool_call_id.clone().unwrap_or_default();
                    // 根据是否包含图片选择纯文本或多模态内容
                    let content = if msg.images.is_empty() {
                        // 纯文本工具结果
                        ToolResultContent::Text(msg.content.clone())
                    } else {
                        // 多模态工具结果（文本 + 图片）
                        let mut result_blocks = Vec::new();
                        if !msg.content.is_empty() {
                            result_blocks.push(ToolResultBlock::Text {
                                text: msg.content.clone(),
                            });
                        }
                        for img in &msg.images {
                            result_blocks.push(ToolResultBlock::Image {
                                source: ImageSource {
                                    kind: "base64",
                                    media_type: img.mime.clone(),
                                    data: img.base64.clone(),
                                },
                            });
                        }
                        ToolResultContent::Blocks(result_blocks)
                    };
                    raw_messages.push(NativeMessage {
                        role: "user".to_string(),
                        content: vec![ContentBlockOut::ToolResult {
                            tool_use_id,
                            content,
                            cache_control: None,
                        }],
                    });
                }
                // user（及其他未知角色）→ user 角色
                _ => {
                    let mut blocks = Vec::new();
                    // 文本内容
                    if !msg.content.is_empty() {
                        blocks.push(ContentBlockOut::Text {
                            text: msg.content.clone(),
                            cache_control: None,
                        });
                    }
                    // 内联图片
                    for img in &msg.images {
                        blocks.push(ContentBlockOut::Image {
                            source: ImageSource {
                                kind: "base64",
                                media_type: img.mime.clone(),
                                data: img.base64.clone(),
                            },
                        });
                    }
                    // 确保至少有一个内容块
                    if blocks.is_empty() {
                        blocks.push(ContentBlockOut::Text {
                            text: String::new(),
                            cache_control: None,
                        });
                    }
                    raw_messages.push(NativeMessage {
                        role: "user".to_string(),
                        content: blocks,
                    });
                }
            }
        }

        // 系统提示：根据长度决定是否启用缓存
        let system_prompt = system_text.map(|text| {
            if Self::should_cache_system(&text) {
                // 长系统提示 → 使用 Blocks 格式 + cache_control
                SystemPrompt::Blocks(vec![SystemBlock {
                    block_type: "text".to_string(),
                    text,
                    cache_control: Some(CacheControl::ephemeral()),
                }])
            } else {
                // 短系统提示 → 直接传递字符串
                SystemPrompt::Plain(text)
            }
        });

        // 合并连续同角色消息（Anthropic 要求严格 user/assistant 交替）
        let merged = Self::merge_consecutive_roles(raw_messages);

        (system_prompt, merged)
    }

    /// 合并连续同角色的消息
    /// Anthropic API 要求消息列表严格 user/assistant 交替，
    /// 但多个 tool_result（均映射为 user）可能连续出现，必须合并为单条 user 消息
    fn merge_consecutive_roles(messages: Vec<NativeMessage>) -> Vec<NativeMessage> {
        let mut result: Vec<NativeMessage> = Vec::new();
        for msg in messages {
            if let Some(last) = result.last_mut() {
                if last.role == msg.role {
                    // 相邻同角色 → 将内容块合并到前一条消息
                    last.content.extend(msg.content);
                    continue;
                }
            }
            result.push(msg);
        }
        result
    }

    // ========================================================================
    // SSE 流式解析
    // ========================================================================

    /// 解析 Anthropic SSE 响应文本
    ///
    /// Anthropic SSE 格式为 `event: <type>\ndata: <json>` 双行结构，
    /// 与 OpenAI CC 的 `data: {json}` 单行格式不同，
    /// 但与 OpenAI Responses API 的格式相似
    fn parse_anthropic_sse(response_text: &str) -> AnthropicStreamAccumulator {
        let mut acc = AnthropicStreamAccumulator::new();
        // 追踪当前事件类型（event: 行和 data: 行是成对出现的）
        let mut current_event = String::new();

        for line in response_text.lines() {
            let line = line.trim();
            // 跳过空行
            if line.is_empty() {
                continue;
            }
            // 解析事件类型行
            if let Some(evt) = line.strip_prefix("event: ") {
                current_event = evt.trim().to_string();
            }
            // 解析数据行（必须紧跟在事件类型行之后）
            else if let Some(data) = line.strip_prefix("data: ") {
                let data = data.trim();
                if !current_event.is_empty() {
                    match serde_json::from_str::<serde_json::Value>(data) {
                        Ok(v) => acc.process_event(&current_event, &v),
                        Err(e) => {
                            tracing::warn!(
                                "Anthropic: 跳过无法解析的事件数据: {e}"
                            );
                        }
                    }
                    current_event.clear();
                }
            }
        }

        acc
    }

    // ========================================================================
    // 核心发送方法
    // ========================================================================

    /// 发送流式请求到 Anthropic Messages API 并解析响应
    ///
    /// 所有请求强制 stream: true，防止反向代理/CDN 的网关超时（HTTP 504）。
    /// 响应体是 SSE 事件流，由 parse_anthropic_sse 解析为结构化结果。
    async fn send_streaming(
        &self,
        system: Option<SystemPrompt>,
        messages: Vec<NativeMessage>,
        tools: Option<Vec<NativeToolSpec>>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        // 校验凭据
        let credential = self.credential.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Anthropic API 密钥未配置。请在 config.toml 的 [llm] 段设置 api_key。"
            )
        })?;

        let tool_count = tools.as_ref().map(|t| t.len()).unwrap_or(0);

        // 构建请求体
        let body = AnthropicRequest {
            model: model.to_string(),
            max_tokens: self.max_tokens,
            system,
            messages,
            temperature,
            tools,
            stream: true,
        };

        let url = format!("{}/v1/messages", self.base_url);
        tracing::info!(
            "Anthropic 请求: POST {} (model={}, tools={}, max_tokens={}, stream=true)",
            url,
            model,
            tool_count,
            self.max_tokens,
        );

        // 构建 HTTP 请求，添加必要的头部
        let mut req = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .json(&body);

        // 添加认证头（根据凭据类型选择 x-api-key 或 Bearer）
        req = Self::apply_auth(req, credential);

        // 发送请求
        let response = req.send().await?;
        let status = response.status();
        tracing::info!("Anthropic 响应: HTTP {}", status);

        // 读取完整响应体（SSE 事件流）
        let text = response.text().await?;

        // 非成功状态码 → 解析错误消息
        if !status.is_success() {
            let msg = serde_json::from_str::<ApiErrorResponse>(&text)
                .map(|e| e.error.message)
                .unwrap_or_else(|_| text.clone());
            anyhow::bail!("Anthropic API 错误: HTTP {status}: {msg}");
        }

        // 解析 SSE 事件流
        let acc = Self::parse_anthropic_sse(&text);

        // 检查流内错误
        if let Some(ref err) = acc.error_message {
            anyhow::bail!("Anthropic 流式错误: {err}");
        }

        // 构建 token 用量统计
        let usage =
            if acc.input_tokens.is_some() || acc.output_tokens.is_some() {
                tracing::info!(
                    "Anthropic 用量: input={}, output={}",
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

        // 提取文本内容
        let text_content = if !acc.content.is_empty() {
            Some(acc.content)
        } else {
            None
        };

        // 转换工具调用（累加器的 json_input 字符串即为 ProviderToolCall 的 arguments）
        let tool_calls: Vec<ProviderToolCall> = acc
            .tool_calls
            .into_iter()
            .filter(|tc| !tc.id.is_empty())
            .map(|tc| ProviderToolCall {
                id: tc.id,
                name: tc.name,
                arguments: tc.json_input,
            })
            .collect();

        // 标准化停止原因
        let raw_stop = acc.stop_reason;
        let stop_reason = raw_stop
            .as_deref()
            .map(NormalizedStopReason::from_anthropic_stop_reason);

        // 输出响应摘要日志
        if !tool_calls.is_empty() {
            let names: Vec<&str> =
                tool_calls.iter().map(|tc| tc.name.as_str()).collect();
            tracing::info!(
                "Anthropic 结果: {} 个工具调用 {:?}",
                tool_calls.len(),
                names
            );
        } else if let Some(ref t) = text_content {
            tracing::info!("Anthropic 结果: 文本 {} 字符", t.len());
        } else {
            tracing::info!("Anthropic 结果: 无内容");
        }

        Ok(ProviderChatResponse {
            text: text_content,
            tool_calls,
            usage,
            reasoning_content: None,
            stop_reason,
            raw_stop_reason: raw_stop,
        })
    }
}

// ============================================================================
// Provider Trait 实现
// ============================================================================

#[async_trait]
impl Provider for AnthropicProvider {
    /// 声明提供商能力：支持原生工具调用和视觉输入
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
        }
    }

    /// 将工具规范转换为 Anthropic 格式的 ToolsPayload
    /// 注意 Anthropic 使用 input_schema 而非 OpenAI 的 parameters
    fn convert_tools(&self, tools: &[ToolSpec]) -> ToolsPayload {
        let native: Vec<serde_json::Value> = tools
            .iter()
            .map(|spec| {
                serde_json::json!({
                    "name": spec.name,
                    "description": spec.description,
                    "input_schema": spec.parameters,
                })
            })
            .collect();
        ToolsPayload::Anthropic { tools: native }
    }

    /// 带可选系统提示的单轮对话
    /// 用于上下文压缩等内部用途（不携带工具定义）
    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        // 构建系统提示（根据长度决定是否启用缓存）
        let system = system_prompt.map(|s| {
            if Self::should_cache_system(s) {
                SystemPrompt::Blocks(vec![SystemBlock {
                    block_type: "text".to_string(),
                    text: s.to_string(),
                    cache_control: Some(CacheControl::ephemeral()),
                }])
            } else {
                SystemPrompt::Plain(s.to_string())
            }
        });

        // 构建单条 user 消息
        let messages = vec![NativeMessage {
            role: "user".to_string(),
            content: vec![ContentBlockOut::Text {
                text: message.to_string(),
                cache_control: None,
            }],
        }];

        // 发送流式请求（无工具）
        let resp = self
            .send_streaming(system, messages, None, model, temperature)
            .await?;

        Ok(resp.text.unwrap_or_default())
    }

    /// 多轮对话支持
    /// 将完整消息历史转换为 Anthropic 格式后发送
    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        // 转换消息：提取系统提示 + 转换对话消息 + 合并同角色消息
        let (system, native_messages) = Self::convert_messages(messages);

        // 发送流式请求（无工具）
        let resp = self
            .send_streaming(system, native_messages, None, model, temperature)
            .await?;

        Ok(resp.text.unwrap_or_default())
    }

    /// 结构化聊天 API（支持工具调用），用于 Agent 循环
    /// 这是 Agent 的主要调用入口，支持完整的消息历史 + 工具定义
    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        // 转换消息：提取系统提示 + 转换对话消息 + 合并同角色消息
        let (system, mut native_messages) =
            Self::convert_messages(request.messages);

        // 对话缓存：为长对话的最后一条消息添加 cache_control
        if Self::should_cache_conversation(request.messages) {
            Self::apply_cache_to_last_message(&mut native_messages);
        }

        // 转换工具定义（最后一个工具添加 cache_control 以批量缓存）
        let tools = Self::convert_native_tools(request.tools);

        // 发送流式请求
        self.send_streaming(system, native_messages, tools, model, temperature)
            .await
    }
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::traits::ImageData;

    // ---- 序列化验证 ----

    /// 测试：CacheControl 序列化为 {"type": "ephemeral"}
    #[test]
    fn cache_control_serialization() {
        let cc = CacheControl::ephemeral();
        let json = serde_json::to_value(&cc).unwrap();
        assert_eq!(json, serde_json::json!({"type": "ephemeral"}));
    }

    /// 测试：SystemPrompt::Plain 序列化为纯字符串
    #[test]
    fn system_prompt_plain_serialization() {
        let sp = SystemPrompt::Plain("你好".to_string());
        let json = serde_json::to_value(&sp).unwrap();
        assert_eq!(json, serde_json::json!("你好"));
    }

    /// 测试：SystemPrompt::Blocks 序列化为带 cache_control 的文本块数组
    #[test]
    fn system_prompt_blocks_serialization() {
        let sp = SystemPrompt::Blocks(vec![SystemBlock {
            block_type: "text".to_string(),
            text: "长系统提示".to_string(),
            cache_control: Some(CacheControl::ephemeral()),
        }]);
        let json = serde_json::to_value(&sp).unwrap();
        assert_eq!(
            json,
            serde_json::json!([{
                "type": "text",
                "text": "长系统提示",
                "cache_control": {"type": "ephemeral"}
            }])
        );
    }

    /// 测试：NativeToolSpec 序列化使用 input_schema（非 parameters）
    #[test]
    fn tool_spec_uses_input_schema() {
        let spec = NativeToolSpec {
            name: "Read".to_string(),
            description: "读取文件".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                }
            }),
            cache_control: None,
        };
        let json = serde_json::to_value(&spec).unwrap();
        // 必须是 input_schema 而非 parameters
        assert!(json.get("input_schema").is_some());
        assert!(json.get("parameters").is_none());
        assert_eq!(json["name"], "Read");
    }

    /// 测试：ToolResultContent::Text 序列化为纯字符串
    #[test]
    fn tool_result_content_text_serialization() {
        let content = ToolResultContent::Text("文件内容".to_string());
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json, serde_json::json!("文件内容"));
    }

    /// 测试：ToolResultContent::Blocks 序列化为块数组
    #[test]
    fn tool_result_content_blocks_serialization() {
        let content = ToolResultContent::Blocks(vec![
            ToolResultBlock::Text {
                text: "图片描述".to_string(),
            },
            ToolResultBlock::Image {
                source: ImageSource {
                    kind: "base64",
                    media_type: "image/png".to_string(),
                    data: "iVBOR...".to_string(),
                },
            },
        ]);
        let json = serde_json::to_value(&content).unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[1]["type"], "image");
        assert_eq!(arr[1]["source"]["media_type"], "image/png");
    }

    // ---- 消息转换 ----

    /// 测试：系统消息被提取到顶层，不进入 messages
    #[test]
    fn convert_messages_extracts_system() {
        let messages = vec![
            ChatMessage::system("你是一个助手"),
            ChatMessage::user("你好"),
        ];
        let (system, native) = AnthropicProvider::convert_messages(&messages);
        // 系统提示被提取
        assert!(system.is_some());
        // messages 中只有 user 消息（不含 system）
        assert_eq!(native.len(), 1);
        assert_eq!(native[0].role, "user");
    }

    /// 测试：带工具调用的 assistant 消息正确转换
    #[test]
    fn convert_messages_assistant_with_tool_calls() {
        let messages = vec![
            ChatMessage::user("帮我读文件"),
            ChatMessage::assistant_with_tool_calls(
                "好的，我来读取文件。".to_string(),
                vec![ProviderToolCall {
                    id: "call_123".to_string(),
                    name: "Read".to_string(),
                    arguments: r#"{"path":"test.txt"}"#.to_string(),
                }],
            ),
        ];
        let (_, native) = AnthropicProvider::convert_messages(&messages);
        assert_eq!(native.len(), 2);
        // assistant 消息应有 Text + ToolUse 两个内容块
        let assistant = &native[1];
        assert_eq!(assistant.role, "assistant");
        assert_eq!(assistant.content.len(), 2);
        // 验证 ToolUse 的 input 是 Value 而非字符串
        match &assistant.content[1] {
            ContentBlockOut::ToolUse { id, name, input, .. } => {
                assert_eq!(id, "call_123");
                assert_eq!(name, "Read");
                assert_eq!(input["path"], "test.txt");
            }
            _ => panic!("第二个内容块应为 ToolUse"),
        }
    }

    /// 测试：tool 消息转为 user 角色的 ToolResult
    #[test]
    fn convert_messages_tool_result() {
        let messages = vec![
            ChatMessage::user("读文件"),
            ChatMessage::assistant_with_tool_calls(
                String::new(),
                vec![ProviderToolCall {
                    id: "call_456".to_string(),
                    name: "Read".to_string(),
                    arguments: r#"{"path":"a.txt"}"#.to_string(),
                }],
            ),
            ChatMessage::tool_result("call_456", "文件内容"),
        ];
        let (_, native) = AnthropicProvider::convert_messages(&messages);
        // user + assistant + user(tool_result) = 3 条
        // 但第 1 条（user）和第 3 条（tool_result → user）不相邻（中间有 assistant）
        assert_eq!(native.len(), 3);
        // 第三条是 user 角色的 ToolResult
        let tool_msg = &native[2];
        assert_eq!(tool_msg.role, "user");
        match &tool_msg.content[0] {
            ContentBlockOut::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                assert_eq!(tool_use_id, "call_456");
                match content {
                    ToolResultContent::Text(t) => assert_eq!(t, "文件内容"),
                    _ => panic!("应为纯文本结果"),
                }
            }
            _ => panic!("应为 ToolResult 块"),
        }
    }

    /// 测试：带图片的 tool 消息转为多模态 ToolResult
    #[test]
    fn convert_messages_tool_result_with_images() {
        let messages = vec![ChatMessage::tool_result_with_images(
            "call_789",
            "图片描述",
            vec![ImageData {
                mime: "image/png".to_string(),
                base64: "iVBOR".to_string(),
            }],
        )];
        let (_, native) = AnthropicProvider::convert_messages(&messages);
        assert_eq!(native.len(), 1);
        match &native[0].content[0] {
            ContentBlockOut::ToolResult { content, .. } => match content {
                ToolResultContent::Blocks(blocks) => {
                    assert_eq!(blocks.len(), 2); // text + image
                }
                _ => panic!("应为 Blocks 类型"),
            },
            _ => panic!("应为 ToolResult 块"),
        }
    }

    // ---- 连续同角色合并 ----

    /// 测试：连续多个 user 消息被合并为一条
    #[test]
    fn merge_consecutive_same_role() {
        let messages = vec![
            NativeMessage {
                role: "user".to_string(),
                content: vec![ContentBlockOut::Text {
                    text: "消息1".to_string(),
                    cache_control: None,
                }],
            },
            NativeMessage {
                role: "user".to_string(),
                content: vec![ContentBlockOut::Text {
                    text: "消息2".to_string(),
                    cache_control: None,
                }],
            },
            NativeMessage {
                role: "assistant".to_string(),
                content: vec![ContentBlockOut::Text {
                    text: "回复".to_string(),
                    cache_control: None,
                }],
            },
        ];
        let merged = AnthropicProvider::merge_consecutive_roles(messages);
        // 两条 user 合并为一条，assistant 保留
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].role, "user");
        assert_eq!(merged[0].content.len(), 2); // 两个文本块合并
        assert_eq!(merged[1].role, "assistant");
    }

    /// 测试：交替角色不合并
    #[test]
    fn merge_alternating_roles_unchanged() {
        let messages = vec![
            NativeMessage {
                role: "user".to_string(),
                content: vec![ContentBlockOut::Text {
                    text: "问题".to_string(),
                    cache_control: None,
                }],
            },
            NativeMessage {
                role: "assistant".to_string(),
                content: vec![ContentBlockOut::Text {
                    text: "回答".to_string(),
                    cache_control: None,
                }],
            },
        ];
        let merged = AnthropicProvider::merge_consecutive_roles(messages);
        assert_eq!(merged.len(), 2);
    }

    /// 测试：空列表处理
    #[test]
    fn merge_empty_list() {
        let merged = AnthropicProvider::merge_consecutive_roles(Vec::new());
        assert!(merged.is_empty());
    }

    // ---- SSE 解析 ----

    /// 测试：纯文本响应的 SSE 解析
    #[test]
    fn parse_sse_text_response() {
        // 模拟 Anthropic SSE 事件流（纯文本响应）
        let sse = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"世界\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":50}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n";

        let acc = AnthropicProvider::parse_anthropic_sse(sse);
        assert_eq!(acc.content, "你好世界");
        assert!(acc.tool_calls.is_empty());
        assert_eq!(acc.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(acc.input_tokens, Some(100));
        assert_eq!(acc.output_tokens, Some(50));
        assert!(acc.error_message.is_none());
    }

    /// 测试：带工具调用的 SSE 解析
    #[test]
    fn parse_sse_tool_call_response() {
        // 模拟 Anthropic SSE 事件流（工具调用响应）
        let sse = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":200}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"我来读取文件。\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01A\",\"name\":\"Read\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\": \\\"test.txt\\\"}\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":1}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":80}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n";

        let acc = AnthropicProvider::parse_anthropic_sse(sse);
        assert_eq!(acc.content, "我来读取文件。");
        assert_eq!(acc.tool_calls.len(), 1);
        assert_eq!(acc.tool_calls[0].id, "toolu_01A");
        assert_eq!(acc.tool_calls[0].name, "Read");
        assert_eq!(acc.tool_calls[0].json_input, r#"{"path": "test.txt"}"#);
        assert_eq!(acc.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(acc.input_tokens, Some(200));
        assert_eq!(acc.output_tokens, Some(80));
    }

    /// 测试：SSE 错误事件解析
    #[test]
    fn parse_sse_error_event() {
        let sse = "\
event: error\n\
data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"服务过载\"}}\n";

        let acc = AnthropicProvider::parse_anthropic_sse(sse);
        assert_eq!(acc.error_message.as_deref(), Some("服务过载"));
    }

    // ---- 认证检测 ----

    /// 测试：OAuth setup token 检测
    #[test]
    fn is_setup_token_detection() {
        // OAuth setup token
        assert!(AnthropicProvider::is_setup_token(
            "sk-ant-oat01-abc123"
        ));
        // 常规 API key
        assert!(!AnthropicProvider::is_setup_token(
            "sk-ant-api03-abc123"
        ));
        // 其他格式
        assert!(!AnthropicProvider::is_setup_token("random-key"));
    }

    // ---- 缓存策略 ----

    /// 测试：短系统提示不启用缓存
    #[test]
    fn should_not_cache_short_system() {
        assert!(!AnthropicProvider::should_cache_system("短提示"));
    }

    /// 测试：长系统提示启用缓存
    #[test]
    fn should_cache_long_system() {
        let long_text = "x".repeat(4000);
        assert!(AnthropicProvider::should_cache_system(&long_text));
    }

    /// 测试：少于 5 条非系统消息不启用对话缓存
    #[test]
    fn should_not_cache_short_conversation() {
        let messages = vec![
            ChatMessage::system("系统提示"),
            ChatMessage::user("你好"),
            ChatMessage::assistant("你好！"),
        ];
        assert!(!AnthropicProvider::should_cache_conversation(&messages));
    }

    /// 测试：超过 4 条非系统消息启用对话缓存
    #[test]
    fn should_cache_long_conversation() {
        let messages = vec![
            ChatMessage::system("系统提示"),
            ChatMessage::user("消息1"),
            ChatMessage::assistant("回复1"),
            ChatMessage::user("消息2"),
            ChatMessage::assistant("回复2"),
            ChatMessage::user("消息3"),
        ];
        assert!(AnthropicProvider::should_cache_conversation(&messages));
    }

    // ---- 工具转换 ----

    /// 测试：工具转换为 Anthropic 格式（最后一个带 cache_control）
    #[test]
    fn convert_native_tools_with_cache() {
        let tools = vec![
            ToolSpec {
                name: "Read".to_string(),
                description: "读取文件".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            },
            ToolSpec {
                name: "Write".to_string(),
                description: "写入文件".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            },
        ];
        let native = AnthropicProvider::convert_native_tools(Some(&tools)).unwrap();
        assert_eq!(native.len(), 2);
        // 第一个工具无 cache_control
        assert!(native[0].cache_control.is_none());
        // 最后一个工具有 cache_control
        assert!(native[1].cache_control.is_some());
    }

    /// 测试：空工具列表返回 None
    #[test]
    fn convert_native_tools_empty() {
        let tools: Vec<ToolSpec> = Vec::new();
        assert!(AnthropicProvider::convert_native_tools(Some(&tools)).is_none());
    }

    /// 测试：None 输入返回 None
    #[test]
    fn convert_native_tools_none() {
        assert!(AnthropicProvider::convert_native_tools(None).is_none());
    }

    // ---- 工具参数转换 ----

    /// 测试：ToolCall.arguments（JSON 字符串）→ ToolUse.input（Value）转换
    #[test]
    fn tool_call_arguments_to_value() {
        let args_str = r#"{"path": "test.txt", "content": "hello"}"#;
        let value: serde_json::Value =
            serde_json::from_str(args_str).unwrap();
        assert_eq!(value["path"], "test.txt");
        assert_eq!(value["content"], "hello");
    }

    /// 测试：无效 JSON 参数降级为空对象
    #[test]
    fn tool_call_invalid_arguments_fallback() {
        let args_str = "not valid json";
        let value: serde_json::Value =
            serde_json::from_str(args_str)
                .unwrap_or_else(|_| serde_json::json!({}));
        assert!(value.is_object());
        assert!(value.as_object().unwrap().is_empty());
    }

    // ---- 完整请求体序列化验证 ----

    /// 测试：AnthropicRequest 完整序列化（验证字段名和结构正确性）
    #[test]
    fn full_request_serialization() {
        let request = AnthropicRequest {
            model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: 4096,
            system: Some(SystemPrompt::Plain("你是助手".to_string())),
            messages: vec![NativeMessage {
                role: "user".to_string(),
                content: vec![ContentBlockOut::Text {
                    text: "你好".to_string(),
                    cache_control: None,
                }],
            }],
            temperature: 0.7,
            tools: None,
            stream: true,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "claude-sonnet-4-20250514");
        assert_eq!(json["max_tokens"], 4096);
        assert_eq!(json["system"], "你是助手");
        assert_eq!(json["stream"], true);
        assert_eq!(json["temperature"], 0.7);
        assert!(json.get("tools").is_none()); // skip_serializing_if = None
    }
}
