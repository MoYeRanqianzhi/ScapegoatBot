// src/providers/traits.rs
// 提供商 Trait 定义
// 参考 zeroclaw/src/providers/traits.rs，定义统一的 LLM 提供商接口
// 所有具体提供商（OpenAI、Anthropic 等）均需实现此 Trait

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt::Write;

// ============================================================================
// 角色常量
// ============================================================================

/// 系统消息角色，用于设置 LLM 的行为和人格
pub const ROLE_SYSTEM: &str = "system";
/// 用户消息角色，代表终端用户的输入
pub const ROLE_USER: &str = "user";
/// 助手消息角色，代表 LLM 的回复
pub const ROLE_ASSISTANT: &str = "assistant";
/// 工具消息角色，代表工具执行的返回结果
pub const ROLE_TOOL: &str = "tool";

// ============================================================================
// 聊天消息
// ============================================================================

/// 对话中的单条消息
/// 每条消息包含角色（system/user/assistant/tool）和文本内容
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// 消息角色：system、user、assistant 或 tool
    pub role: String,
    /// 消息文本内容
    pub content: String,
}

impl ChatMessage {
    /// 创建系统消息，用于注入系统提示（人格设定、Skill 指令等）
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ROLE_SYSTEM.into(),
            content: content.into(),
        }
    }

    /// 创建用户消息，代表终端用户发送的内容
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ROLE_USER.into(),
            content: content.into(),
        }
    }

    /// 创建助手消息，代表 LLM 返回的回复
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ROLE_ASSISTANT.into(),
            content: content.into(),
        }
    }

    /// 创建工具消息，代表工具执行后的返回值
    pub fn tool(content: impl Into<String>) -> Self {
        Self {
            role: ROLE_TOOL.into(),
            content: content.into(),
        }
    }
}

// ============================================================================
// 工具调用
// ============================================================================

/// LLM 请求执行的工具调用
/// 当 LLM 判断需要调用外部工具时，会返回一个或多个 ToolCall
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// 工具调用的唯一标识符，用于将结果关联回原始请求
    pub id: String,
    /// 要调用的工具名称
    pub name: String,
    /// 传递给工具的 JSON 格式参数字符串
    pub arguments: String,
}

// ============================================================================
// Token 用量统计
// ============================================================================

/// 单次 LLM API 响应的原始 Token 计数
/// 用于跟踪 API 调用的成本和上下文使用情况
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    /// 输入 Token 数量（包含系统提示、历史消息、用户输入等）
    pub input_tokens: Option<u64>,
    /// 输出 Token 数量（LLM 生成的回复内容）
    pub output_tokens: Option<u64>,
}

// ============================================================================
// 标准化停止原因
// ============================================================================

/// 提供商无关的停止原因枚举
/// 不同 LLM 提供商使用不同的停止原因字符串，此枚举将它们统一为标准化格式
/// 方便上层逻辑（如代理循环）进行统一处理
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum NormalizedStopReason {
    /// 正常结束：LLM 主动完成了回复
    EndTurn,
    /// 工具调用：LLM 请求执行一个或多个工具
    ToolCall,
    /// Token 上限：回复因达到 max_tokens 而被截断
    MaxTokens,
    /// 上下文窗口溢出：输入 + 输出超出了模型的上下文窗口大小
    ContextWindowExceeded,
    /// 安全拦截：内容被安全过滤器阻止
    SafetyBlocked,
    /// 已取消：请求被客户端取消
    Cancelled,
    /// 未知原因：无法识别的提供商特定停止原因
    Unknown(String),
}

impl NormalizedStopReason {
    /// 从 OpenAI 格式的 finish_reason 转换为标准化停止原因
    /// OpenAI 使用 "stop"、"tool_calls"、"length" 等字符串
    pub fn from_openai_finish_reason(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "stop" => Self::EndTurn,
            "tool_calls" | "function_call" => Self::ToolCall,
            "length" | "max_tokens" => Self::MaxTokens,
            "content_filter" => Self::SafetyBlocked,
            "cancelled" | "canceled" => Self::Cancelled,
            _ => Self::Unknown(raw.trim().to_string()),
        }
    }

    /// 从 Anthropic 格式的 stop_reason 转换为标准化停止原因
    /// Anthropic 使用 "end_turn"、"tool_use"、"max_tokens" 等字符串
    pub fn from_anthropic_stop_reason(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "end_turn" | "stop_sequence" => Self::EndTurn,
            "tool_use" => Self::ToolCall,
            "max_tokens" => Self::MaxTokens,
            "model_context_window_exceeded" => Self::ContextWindowExceeded,
            "safety" => Self::SafetyBlocked,
            "cancelled" | "canceled" => Self::Cancelled,
            _ => Self::Unknown(raw.trim().to_string()),
        }
    }
}

// ============================================================================
// 聊天响应
// ============================================================================

/// LLM 的结构化响应
/// 可能包含文本内容、工具调用请求，或两者兼有
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// 响应的文本内容（如果 LLM 只返回了工具调用，则可能为 None）
    pub text: Option<String>,
    /// LLM 请求执行的工具调用列表
    pub tool_calls: Vec<ToolCall>,
    /// 提供商返回的 Token 用量统计（如果可用）
    pub usage: Option<TokenUsage>,
    /// 来自"思考型"模型的原始推理内容（如 DeepSeek-R1、Kimi K2.5）
    /// 保留为不透明的透传字段，某些提供商要求在后续请求中回传此内容
    pub reasoning_content: Option<String>,
    /// 标准化的停止原因（如果提供商返回了）
    pub stop_reason: Option<NormalizedStopReason>,
    /// 提供商原始的停止原因字符串，用于诊断和调试
    pub raw_stop_reason: Option<String>,
}

impl ChatResponse {
    /// 检查 LLM 是否请求了至少一个工具调用
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// 便捷方法：返回文本内容或空字符串
    /// 当不需要区分 None 和空字符串时使用
    pub fn text_or_empty(&self) -> &str {
        self.text.as_deref().unwrap_or("")
    }
}

// ============================================================================
// 聊天请求
// ============================================================================

/// 提供商聊天调用的请求载荷
/// 包含消息历史和可选的工具定义
#[derive(Debug, Clone, Copy)]
pub struct ChatRequest<'a> {
    /// 对话消息列表（包含系统提示、历史消息和当前用户输入）
    pub messages: &'a [ChatMessage],
    /// 可选的工具规范列表（当前版本暂不使用工具调用功能）
    pub tools: Option<&'a [ToolSpec]>,
}

// ============================================================================
// 工具规范
// ============================================================================

/// 工具的元数据规范，用于向 LLM 描述可用工具
/// 遵循 OpenAI function calling 的 JSON Schema 格式
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// 工具名称，供 LLM 在工具调用中引用
    pub name: String,
    /// 工具的自然语言描述，帮助 LLM 理解何时以及如何使用该工具
    pub description: String,
    /// 工具参数的 JSON Schema，定义工具接受的输入格式
    pub parameters: serde_json::Value,
}

// ============================================================================
// 工具结果消息
// ============================================================================

/// 工具执行结果，用于将工具的输出回传给 LLM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultMessage {
    /// 对应的工具调用 ID，用于将结果关联回原始的 ToolCall
    pub tool_call_id: String,
    /// 工具执行产生的文本输出内容
    pub content: String,
}

// ============================================================================
// 提供商能力声明
// ============================================================================

/// 提供商能力声明
/// 描述一个 LLM 提供商支持哪些功能特性
/// 用于在运行时智能选择工具调用模式和请求格式
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// 是否支持原生工具调用（通过 API 原语）
    /// 为 true 时，提供商可以将工具定义转换为 API 原生格式
    /// 为 false 时，工具必须通过系统提示以文本形式注入
    pub native_tool_calling: bool,
    /// 是否支持视觉 / 图像输入
    pub vision: bool,
}

// ============================================================================
// 工具载荷格式
// ============================================================================

/// 提供商特定的工具载荷格式
/// 不同的 LLM 提供商对工具定义有不同的 API 格式要求
/// 此枚举封装了这些差异，实现从统一的 ToolSpec 到各提供商原生格式的转换
#[derive(Debug, Clone)]
pub enum ToolsPayload {
    /// OpenAI Chat Completions API 格式（tools 数组 + function 对象）
    OpenAI { tools: Vec<serde_json::Value> },
    /// Anthropic Messages API 格式（tools 数组 + input_schema 对象）
    Anthropic { tools: Vec<serde_json::Value> },
    /// 提示引导回退模式（将工具定义作为文本注入系统提示）
    /// 适用于不支持原生工具调用的提供商
    PromptGuided { instructions: String },
}

// ============================================================================
// 提供商 Trait
// ============================================================================

/// LLM 提供商的统一接口
/// 所有具体提供商（OpenAI、Anthropic、Ollama 等）均需实现此 Trait
/// 通过 Trait Object 实现运行时多态，支持提供商的热切换
#[async_trait]
pub trait Provider: Send + Sync {
    /// 查询提供商的能力声明
    /// 默认实现返回最小能力集（不支持原生工具调用）
    /// 具体提供商应重写此方法以声明实际能力
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    /// 将工具规范转换为提供商原生格式
    /// 默认实现返回 PromptGuided 载荷（将工具文档以文本形式注入系统提示）
    /// 支持原生工具调用的提供商应重写此方法
    fn convert_tools(&self, tools: &[ToolSpec]) -> ToolsPayload {
        ToolsPayload::PromptGuided {
            instructions: build_tool_instructions_text(tools),
        }
    }

    /// 简单的单轮对话（单条用户消息，无显式系统提示）
    /// 这是非代理场景下的首选 API
    async fn simple_chat(
        &self,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        // 默认委托给 chat_with_system，不带系统提示
        self.chat_with_system(None, message, model, temperature)
            .await
    }

    /// 带可选系统提示的单轮对话
    /// 这是最基础的聊天方法，所有提供商必须实现
    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String>;

    /// 多轮对话支持
    /// 默认实现从消息列表中提取系统消息和最后一条用户消息，然后委托给 chat_with_system
    /// 支持完整历史的提供商可以重写此方法
    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        // 从消息列表中找到系统消息
        let system = messages
            .iter()
            .find(|m| m.role == ROLE_SYSTEM)
            .map(|m| m.content.as_str());
        // 从消息列表中找到最后一条用户消息
        let last_user = messages
            .iter()
            .rfind(|m| m.role == ROLE_USER)
            .map(|m| m.content.as_str())
            .unwrap_or("");
        // 委托给 chat_with_system 处理
        self.chat_with_system(system, last_user, model, temperature)
            .await
    }

    /// 结构化聊天 API，用于代理循环调用
    /// 支持工具调用的完整交互流程
    /// 当提供商不支持原生工具调用时，自动回退到提示引导模式
    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        // 如果请求包含工具定义，但提供商不支持原生工具调用
        // 则将工具说明注入到系统提示中作为文本回退
        if let Some(tools) = request.tools {
            if !tools.is_empty() && !self.supports_native_tools() {
                // 将工具定义转换为文本格式的说明
                let tool_instructions = match self.convert_tools(tools) {
                    ToolsPayload::PromptGuided { instructions } => instructions,
                    payload => {
                        anyhow::bail!(
                            "提供商在 supports_native_tools() 为 false 时返回了非 PromptGuided 载荷 ({payload:?})"
                        )
                    }
                };

                // 复制消息列表，将工具说明注入系统消息
                let mut modified_messages = request.messages.to_vec();
                if let Some(system_message) =
                    modified_messages.iter_mut().find(|m| m.role == ROLE_SYSTEM)
                {
                    // 系统消息已存在，追加工具说明
                    if !system_message.content.is_empty() {
                        system_message.content.push_str("\n\n");
                    }
                    system_message.content.push_str(&tool_instructions);
                } else {
                    // 没有系统消息，在消息列表开头插入一条
                    modified_messages.insert(0, ChatMessage::system(tool_instructions));
                }

                // 使用修改后的消息列表调用 chat_with_history
                let text = self
                    .chat_with_history(&modified_messages, model, temperature)
                    .await?;
                return Ok(ChatResponse {
                    text: Some(text),
                    tool_calls: Vec::new(),
                    usage: None,
                    reasoning_content: None,
                    stop_reason: None,
                    raw_stop_reason: None,
                });
            }
        }

        // 正常流程：直接调用 chat_with_history
        let text = self
            .chat_with_history(request.messages, model, temperature)
            .await?;
        Ok(ChatResponse {
            text: Some(text),
            tool_calls: Vec::new(),
            usage: None,
            reasoning_content: None,
            stop_reason: None,
            raw_stop_reason: None,
        })
    }

    /// 检查提供商是否支持原生工具调用
    fn supports_native_tools(&self) -> bool {
        self.capabilities().native_tool_calling
    }

    /// 检查提供商是否支持多模态视觉输入
    fn supports_vision(&self) -> bool {
        self.capabilities().vision
    }
}

// ============================================================================
// 工具说明文本构建
// ============================================================================

/// 将工具规范列表转换为人类可读的文本说明
/// 用于不支持原生工具调用的提供商的回退模式
/// 将工具的名称、描述和参数 Schema 格式化为系统提示中的指令文本
pub fn build_tool_instructions_text(tools: &[ToolSpec]) -> String {
    let mut out = String::from("## Available Tools\n\nYou have access to the following tools. ");
    out.push_str(
        "To use a tool, respond with a JSON object wrapped in <tool_call> tags:\n\
         <tool_call>{\"name\": \"tool_name\", \"arguments\": {...}}</tool_call>\n\n",
    );

    // 逐个列出工具的名称、描述和参数定义
    for tool in tools {
        let _ = writeln!(out, "### {}\n", tool.name);
        let _ = writeln!(out, "{}\n", tool.description);
        let _ = writeln!(
            out,
            "Parameters:\n```json\n{}\n```\n",
            serde_json::to_string_pretty(&tool.parameters).unwrap_or_default()
        );
    }

    out
}
