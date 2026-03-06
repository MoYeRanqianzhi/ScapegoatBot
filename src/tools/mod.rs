// src/tools/mod.rs
// 工具模块
// 将 NapCatLink API 映射为 LLM 可调用的内置工具
// 在工具执行层进行白名单检查

pub mod message;
pub mod social;
pub mod query;

use crate::whitelist::Whitelist;
use napcat_link::OneBotApi;
use std::sync::Arc;

// ============================================================================
// 工具执行上下文
// ============================================================================

/// 工具执行上下文
/// 包含所有工具执行所需的共享资源
/// 通过 Arc 在多个异步任务间共享
#[derive(Clone)]
pub struct ToolContext {
    /// NapCatLink API 聚合器
    pub api: OneBotApi,
    /// 白名单检查器
    pub whitelist: Arc<Whitelist>,
}

impl ToolContext {
    /// 创建工具执行上下文
    pub fn new(api: OneBotApi, whitelist: Whitelist) -> Self {
        Self {
            api,
            whitelist: Arc::new(whitelist),
        }
    }
}

// ============================================================================
// 工具执行结果
// ============================================================================

/// 工具执行结果
/// 包含成功时的文本输出或失败时的错误消息
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// 是否成功
    pub success: bool,
    /// 输出内容（成功时为结果描述，失败时为错误消息）
    pub output: String,
}

impl ToolResult {
    /// 创建成功结果
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            success: true,
            output: output.into(),
        }
    }

    /// 创建失败结果
    pub fn err(output: impl Into<String>) -> Self {
        Self {
            success: false,
            output: output.into(),
        }
    }
}
