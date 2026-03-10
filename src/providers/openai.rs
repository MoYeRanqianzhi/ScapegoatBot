// src/providers/openai.rs
// OpenAI 兼容 API 提供商实现
// 参考 zeroclaw/src/providers/openai.rs，实现 OpenAI Chat Completions API 调用
// 支持所有 OpenAI API 兼容的服务（OpenAI、DeepSeek、Moonshot、本地 Ollama 等）

use super::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    NormalizedStopReason, Provider, ProviderCapabilities, TokenUsage, ToolCall as ProviderToolCall,
    ToolSpec, ToolsPayload,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

// ============================================================================
// 提供商结构体
// ============================================================================

/// OpenAI 兼容 API 提供商
/// 支持标准 OpenAI API 以及任何兼容 OpenAI Chat Completions 格式的服务
/// 例如：OpenAI、Azure OpenAI、DeepSeek、Moonshot、本地 Ollama、vLLM 等
pub struct OpenAiProvider {
    /// API 基础 URL（不含尾部斜杠），如 "https://api.openai.com/v1"
    base_url: String,
    /// API 认证凭据（Bearer token），设为 None 则不发送 Authorization 头
    credential: Option<String>,
    /// 可选的 max_tokens 覆盖值，设为 None 则不在请求中发送 max_tokens 字段
    max_tokens_override: Option<u32>,
    /// 可复用的 HTTP 客户端（内含连接池）
    client: Client,
}

// ============================================================================
// 请求结构体（发送给 API 的 JSON 载荷）
// ============================================================================

/// 基础聊天补全请求体（不含工具调用）
#[derive(Debug, Serialize)]
struct SimpleChatRequest {
    /// 模型名称，如 "gpt-4o-mini"、"deepseek-chat"
    model: String,
    /// 消息列表
    messages: Vec<ApiMessage>,
    /// 采样温度，控制回复的随机性（0.0 = 确定性，2.0 = 最随机）
    temperature: f64,
    /// 最大输出 Token 数限制（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

/// 带原生工具调用支持的聊天补全请求体
#[derive(Debug, Serialize)]
struct NativeChatRequest {
    /// 模型名称
    model: String,
    /// 消息列表（支持包含 tool_calls 和 tool_call_id 的消息）
    messages: Vec<NativeApiMessage>,
    /// 采样温度
    temperature: f64,
    /// 最大输出 Token 数限制（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    /// 可用工具定义列表（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<NativeToolSpec>>,
    /// 工具选择策略（可选），如 "auto"、"none"、"required"
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
}

/// 发送给 API 的基础消息格式
#[derive(Debug, Serialize)]
struct ApiMessage {
    /// 消息角色
    role: String,
    /// 消息文本内容
    content: String,
}

/// 发送给 API 的原生消息格式（支持工具调用字段）
#[derive(Debug, Serialize)]
struct NativeApiMessage {
    /// 消息角色
    role: String,
    /// 消息文本内容（tool 角色的消息可能不含 content）
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    /// 工具调用 ID（仅 role=tool 的消息需要）
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    /// 助手消息中的工具调用请求列表
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<NativeToolCallPayload>>,
    /// 思考型模型的推理内容，需要透传以保持 API 兼容性
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
}

// ============================================================================
// 工具调用相关结构体
// ============================================================================

/// OpenAI 原生工具规范
/// 遵循 OpenAI function calling 的格式：type="function" + function 对象
#[derive(Debug, Serialize, Deserialize)]
struct NativeToolSpec {
    /// 工具类型，目前只支持 "function"
    #[serde(rename = "type")]
    kind: String,
    /// 函数规范（名称、描述、参数 Schema）
    function: NativeToolFunctionSpec,
}

/// 工具函数的具体规范
#[derive(Debug, Serialize, Deserialize)]
struct NativeToolFunctionSpec {
    /// 函数名称
    name: String,
    /// 函数描述
    description: String,
    /// 参数 JSON Schema
    parameters: serde_json::Value,
}

/// 发送给 API 的工具调用载荷
#[derive(Debug, Serialize, Deserialize)]
struct NativeToolCallPayload {
    /// 工具调用 ID（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    /// 工具类型（可选）
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    /// 函数调用详情
    function: NativeFunctionCall,
}

/// 函数调用的具体内容（名称 + 参数 JSON 字符串）
#[derive(Debug, Serialize, Deserialize)]
struct NativeFunctionCall {
    /// 函数名称
    name: String,
    /// 参数 JSON 字符串
    arguments: String,
}

// ============================================================================
// 响应结构体（从 API 返回的 JSON 解析）
// ============================================================================

/// 基础聊天补全响应
#[derive(Debug, Deserialize)]
struct SimpleChatResponse {
    /// 候选回复列表（通常只有一个元素）
    choices: Vec<SimpleChoice>,
}

/// 基础响应中的单个候选回复
#[derive(Debug, Deserialize)]
struct SimpleChoice {
    /// 回复消息内容
    message: SimpleResponseMessage,
    /// 停止原因
    #[serde(default)]
    finish_reason: Option<String>,
}

/// 基础响应中的消息内容
#[derive(Debug, Deserialize)]
struct SimpleResponseMessage {
    /// 文本内容
    #[serde(default)]
    content: Option<String>,
    /// 思考型模型的推理内容
    #[serde(default)]
    reasoning_content: Option<String>,
}

impl SimpleResponseMessage {
    /// 获取有效内容：优先返回 content，如果为空则返回 reasoning_content
    fn effective_content(&self) -> String {
        match &self.content {
            Some(c) if !c.is_empty() => c.clone(),
            _ => self.reasoning_content.clone().unwrap_or_default(),
        }
    }
}

/// 带工具调用的聊天补全响应
#[derive(Debug, Deserialize)]
struct NativeChatResponse {
    /// 候选回复列表
    choices: Vec<NativeChoice>,
    /// Token 用量统计（可选）
    #[serde(default)]
    usage: Option<ApiUsageInfo>,
}

/// Token 用量信息
#[derive(Debug, Deserialize)]
struct ApiUsageInfo {
    /// 输入（Prompt）Token 数
    #[serde(default)]
    prompt_tokens: Option<u64>,
    /// 输出（Completion）Token 数
    #[serde(default)]
    completion_tokens: Option<u64>,
}

/// 带工具调用的候选回复
#[derive(Debug, Deserialize)]
struct NativeChoice {
    /// 回复消息内容
    message: NativeResponseMessage,
    /// 停止原因
    #[serde(default)]
    finish_reason: Option<String>,
}

/// 带工具调用的响应消息内容
#[derive(Debug, Deserialize)]
struct NativeResponseMessage {
    /// 文本内容
    #[serde(default)]
    content: Option<String>,
    /// 思考型模型的推理内容
    #[serde(default)]
    reasoning_content: Option<String>,
    /// 工具调用请求列表
    #[serde(default)]
    tool_calls: Option<Vec<NativeToolCallPayload>>,
}

impl NativeResponseMessage {
    /// 获取有效内容：优先返回 content，如果为空则返回 reasoning_content
    fn effective_content(&self) -> Option<String> {
        match &self.content {
            Some(c) if !c.is_empty() => Some(c.clone()),
            _ => self.reasoning_content.clone(),
        }
    }
}

// ============================================================================
// API 错误响应结构体
// ============================================================================

/// OpenAI API 错误响应的外层包装
#[derive(Debug, Deserialize)]
struct ApiErrorResponse {
    /// 错误详情
    error: ApiErrorDetail,
}

/// OpenAI API 错误详情
#[derive(Debug, Deserialize)]
struct ApiErrorDetail {
    /// 错误消息文本
    message: String,
    /// 错误类型（如 "invalid_request_error"）
    #[serde(default)]
    #[serde(rename = "type")]
    error_type: Option<String>,
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
    pub fn new(
        base_url: Option<&str>,
        credential: Option<&str>,
        max_tokens_override: Option<u32>,
    ) -> Self {
        Self {
            base_url: base_url
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            credential: credential.map(ToString::to_string),
            max_tokens_override: max_tokens_override.filter(|v| *v > 0),
            client: Client::new(),
        }
    }

    /// 将统一的 ToolSpec 列表转换为 OpenAI 原生工具格式
    fn convert_tool_specs(tools: Option<&[ToolSpec]>) -> Option<Vec<NativeToolSpec>> {
        tools.map(|items| {
            items
                .iter()
                .map(|spec| NativeToolSpec {
                    kind: "function".to_string(),
                    function: NativeToolFunctionSpec {
                        name: spec.name.clone(),
                        description: spec.description.clone(),
                        parameters: spec.parameters.clone(),
                    },
                })
                .collect()
        })
    }

    /// 构建 HTTP 请求的 Authorization 头
    fn auth_header(&self) -> Option<String> {
        self.credential
            .as_ref()
            .map(|key| format!("Bearer {key}"))
    }

    /// 发送基础聊天补全请求（不带工具调用）
    /// 适用于简单的单轮或多轮文本对话
    async fn send_simple_request(
        &self,
        messages: Vec<ApiMessage>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        // 构建请求体
        let body = SimpleChatRequest {
            model: model.to_string(),
            messages,
            temperature,
            max_tokens: self.max_tokens_override,
        };

        // 构建 HTTP POST 请求
        let url = format!("{}/chat/completions", self.base_url);
        let mut request = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body);

        // 添加认证头（如果有）
        if let Some(auth) = self.auth_header() {
            request = request.header("Authorization", auth);
        }

        // 发送请求并处理响应
        let response = request.send().await?;
        let status = response.status();
        let response_text = response.text().await?;

        // 检查 HTTP 状态码，非 2xx 时解析错误响应
        if !status.is_success() {
            let error_msg = serde_json::from_str::<ApiErrorResponse>(&response_text)
                .map(|e| e.error.message)
                .unwrap_or_else(|_| format!("HTTP {status}: {response_text}"));
            anyhow::bail!("OpenAI API 错误: {error_msg}");
        }

        // 解析成功响应
        let parsed: SimpleChatResponse = serde_json::from_str(&response_text)
            .map_err(|e| anyhow::anyhow!("解析 OpenAI 响应失败: {e}\n原始响应: {response_text}"))?;

        // 提取第一个候选回复的有效内容
        let content = parsed
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message.effective_content())
            .unwrap_or_default();

        Ok(content)
    }

    /// 发送带工具调用支持的聊天补全请求
    /// 返回完整的结构化响应（包含文本、工具调用、Token 统计等）
    async fn send_native_request(
        &self,
        messages: Vec<NativeApiMessage>,
        tools: Option<Vec<NativeToolSpec>>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        // 构建请求体
        let body = NativeChatRequest {
            model: model.to_string(),
            messages,
            temperature,
            max_tokens: self.max_tokens_override,
            tools,
            tool_choice: None, // 使用 API 默认策略
        };

        // 构建 HTTP POST 请求
        let url = format!("{}/chat/completions", self.base_url);
        let mut request = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body);

        // 添加认证头（如果有）
        if let Some(auth) = self.auth_header() {
            request = request.header("Authorization", auth);
        }

        // 发送请求并处理响应
        let response = request.send().await?;
        let status = response.status();
        let response_text = response.text().await?;

        // 检查 HTTP 状态码
        if !status.is_success() {
            let error_msg = serde_json::from_str::<ApiErrorResponse>(&response_text)
                .map(|e| e.error.message)
                .unwrap_or_else(|_| format!("HTTP {status}: {response_text}"));
            anyhow::bail!("OpenAI API 错误: {error_msg}");
        }

        // 解析成功响应
        let parsed: NativeChatResponse = serde_json::from_str(&response_text)
            .map_err(|e| anyhow::anyhow!("解析 OpenAI 响应失败: {e}\n原始响应: {response_text}"))?;

        // 提取 Token 用量统计
        let usage = parsed.usage.map(|u| TokenUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        });

        // 提取第一个候选回复
        let choice = parsed.choices.into_iter().next();

        // 从候选回复中提取各字段
        let (text, tool_calls, reasoning_content, stop_reason, raw_stop_reason) =
            if let Some(choice) = choice {
                // 提取文本内容
                let text = choice.message.effective_content();

                // 提取推理内容（思考型模型）
                let reasoning = choice.message.reasoning_content.clone();

                // 将 API 返回的工具调用转换为统一格式
                let calls = choice
                    .message
                    .tool_calls
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|tc| {
                        // 每个工具调用必须有 ID
                        let id = tc.id?;
                        Some(ProviderToolCall {
                            id,
                            name: tc.function.name,
                            arguments: tc.function.arguments,
                        })
                    })
                    .collect::<Vec<_>>();

                // 解析停止原因
                let raw = choice.finish_reason.clone();
                let normalized = raw
                    .as_deref()
                    .map(NormalizedStopReason::from_openai_finish_reason);

                (text, calls, reasoning, normalized, raw)
            } else {
                (None, Vec::new(), None, None, None)
            };

        Ok(ProviderChatResponse {
            text,
            tool_calls,
            usage,
            reasoning_content,
            stop_reason,
            raw_stop_reason,
        })
    }
}

// ============================================================================
// Provider Trait 实现
// ============================================================================

#[async_trait]
impl Provider for OpenAiProvider {
    /// 声明 OpenAI 提供商的能力
    /// OpenAI 原生支持 function calling 和视觉输入
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
        }
    }

    /// 将工具规范转换为 OpenAI 原生格式
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
    /// 将消息转换为 API 格式并发送请求
    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        // 构建消息列表
        let mut messages = Vec::new();

        // 如果有系统提示，添加系统消息
        if let Some(system) = system_prompt {
            messages.push(ApiMessage {
                role: "system".to_string(),
                content: system.to_string(),
            });
        }

        // 添加用户消息
        messages.push(ApiMessage {
            role: "user".to_string(),
            content: message.to_string(),
        });

        // 发送基础请求
        self.send_simple_request(messages, model, temperature).await
    }

    /// 多轮对话支持
    /// 将完整的消息历史转换为 API 格式并发送请求
    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        // 将统一消息格式转换为 API 消息格式
        let api_messages: Vec<ApiMessage> = messages
            .iter()
            .map(|m| ApiMessage {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();

        // 发送基础请求
        self.send_simple_request(api_messages, model, temperature)
            .await
    }

    /// 结构化聊天 API（支持工具调用）
    /// 当请求包含工具定义时，使用原生工具调用格式发送请求
    /// 否则回退到基础的 chat_with_history
    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        // 检查是否需要使用原生工具调用
        let has_tools = request
            .tools
            .is_some_and(|t| !t.is_empty());

        if has_tools {
            // 使用原生工具调用格式
            // 将统一消息格式转换为原生 API 消息格式（包含 tool_calls 和 tool_call_id）
            let native_messages: Vec<NativeApiMessage> = request
                .messages
                .iter()
                .map(|m| {
                    // 将 ChatMessage 中的 tool_calls 转换为 API 原生格式
                    let tool_calls = if m.tool_calls.is_empty() {
                        None
                    } else {
                        Some(
                            m.tool_calls
                                .iter()
                                .map(|tc| NativeToolCallPayload {
                                    id: Some(tc.id.clone()),
                                    kind: Some("function".to_string()),
                                    function: NativeFunctionCall {
                                        name: tc.name.clone(),
                                        arguments: tc.arguments.clone(),
                                    },
                                })
                                .collect(),
                        )
                    };
                    NativeApiMessage {
                        role: m.role.clone(),
                        // tool 角色的消息始终发送 content（即使为空），其他角色空内容发 None
                        content: if m.content.is_empty() && m.role != "tool" {
                            None
                        } else {
                            Some(m.content.clone())
                        },
                        tool_call_id: m.tool_call_id.clone(),
                        tool_calls,
                        reasoning_content: None,
                    }
                })
                .collect();

            // 转换工具定义
            let native_tools = Self::convert_tool_specs(request.tools);

            // 发送带工具调用的请求
            self.send_native_request(native_messages, native_tools, model, temperature)
                .await
        } else {
            // 无工具调用，使用基础的 chat_with_history 并包装为 ChatResponse
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

/// 创建一个使用默认配置的 OpenAI 提供商
/// 从环境变量 OPENAI_API_KEY 和 OPENAI_BASE_URL 读取配置
pub fn from_env() -> OpenAiProvider {
    let api_key = std::env::var("OPENAI_API_KEY").ok();
    let base_url = std::env::var("OPENAI_BASE_URL").ok();
    OpenAiProvider::new(base_url.as_deref(), api_key.as_deref(), None)
}
