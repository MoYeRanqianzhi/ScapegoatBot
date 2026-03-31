// src/providers/mod.rs
// 模型提供商模块
// 定义 LLM 提供商的公共接口、具体实现和工厂函数

pub mod anthropic;
pub mod openai;
pub mod traits;

use self::anthropic::AnthropicProvider;
use self::openai::OpenAiProvider;
use self::traits::Provider;

// ============================================================================
// Provider 工厂函数
// ============================================================================

/// 根据配置参数创建 LLM 提供商实例
///
/// 统一的 Provider 创建入口，供 AgentRunner（主代理）和 SubAgentRunner（子代理）复用。
/// 根据 provider_type 选择具体实现：
/// - "anthropic" → AnthropicProvider（Claude 系列模型）
/// - 其他（含 "openai"）→ OpenAiProvider（OpenAI 兼容 API）
///
/// # 参数
/// - `provider_type`: 提供商类型字符串（"openai" / "anthropic"）
/// - `base_url`: API 基础 URL
/// - `api_key`: API 密钥（空字符串视为无凭据）
/// - `max_tokens`: 最大输出 token 数（Anthropic 必填，OpenAI 可选）
/// - `timeout_secs`: HTTP 请求超时时间（秒）
/// - `wire_api`: API 协议类型（仅 OpenAI: "chat_completions" / "responses"）
pub fn create_provider(
    provider_type: &str,
    base_url: &str,
    api_key: &str,
    max_tokens: Option<u32>,
    timeout_secs: u64,
    wire_api: &str,
) -> Box<dyn Provider> {
    // 凭据处理：空字符串视为 None
    let credential: Option<&str> = if api_key.is_empty() {
        None
    } else {
        Some(api_key)
    };

    match provider_type.to_ascii_lowercase().as_str() {
        // Anthropic Messages API（Claude 系列模型）
        "anthropic" => Box::new(AnthropicProvider::new(
            Some(base_url),
            credential,
            // Anthropic 必须有 max_tokens，默认 16384
            max_tokens.unwrap_or(16384),
            timeout_secs,
        )),
        // OpenAI 兼容 API（默认，含 ChatCompletions / Responses 双协议）
        _ => Box::new(OpenAiProvider::new(
            Some(base_url),
            credential,
            max_tokens,
            timeout_secs,
            wire_api,
        )),
    }
}
