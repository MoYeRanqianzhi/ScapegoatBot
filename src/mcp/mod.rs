// src/mcp/mod.rs
// MCP (Model Context Protocol) 模块
// 实现 JSON-RPC 2.0 通信协议，用于连接外部 MCP 服务器
// 外部工具通过 MCP 标准协议暴露给 Agent，与内置工具统一调用
//
// 架构：
// - mod.rs：JSON-RPC 2.0 类型定义 + 全局请求 ID 计数器
// - transport.rs：Stdio 传输层（子进程 stdin/stdout 通信）
// - manager.rs：MCP 连接管理器（多服务器连接 + 工具路由）

// ============================================================================
// 子模块声明
// ============================================================================

/// Stdio 传输层：管理子进程的 stdin/stdout JSON-RPC 通信
pub mod transport;
/// 连接管理器：管理多个 MCP 服务器连接和工具路由
pub mod manager;

// 重导出核心类型，方便外部模块直接使用 crate::mcp::McpConnectionManager
pub use manager::McpConnectionManager;

// ============================================================================
// 导入
// ============================================================================

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

// ============================================================================
// 全局请求 ID 计数器
// ============================================================================

/// 全局原子计数器，为每个 JSON-RPC 请求生成唯一递增 ID
/// 使用 AtomicU64 确保线程安全，Relaxed 排序足够（仅需唯一性，不需跨线程可见性保证）
static REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// 获取下一个请求 ID
/// 每次调用返回一个唯一的递增整数，用于 JSON-RPC 请求的 id 字段
pub fn next_request_id() -> u64 {
    // fetch_add 原子自增，返回自增前的值
    REQUEST_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ============================================================================
// JSON-RPC 2.0 类型定义
// ============================================================================

/// JSON-RPC 2.0 请求体
/// 序列化后发送给 MCP 服务器
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcRequest {
    /// JSON-RPC 协议版本，固定为 "2.0"
    pub jsonrpc: String,
    /// 请求唯一标识符，用于关联响应
    pub id: u64,
    /// 要调用的方法名称（如 "initialize", "tools/list", "tools/call"）
    pub method: String,
    /// 方法参数（可选），以 JSON 值传递
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// JSON-RPC 2.0 响应体
/// 从 MCP 服务器接收并反序列化
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcResponse {
    /// JSON-RPC 协议版本，固定为 "2.0"
    pub jsonrpc: String,
    /// 请求标识符（通知响应时可能为 None）
    pub id: Option<u64>,
    /// 成功时的返回值
    pub result: Option<serde_json::Value>,
    /// 失败时的错误信息
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC 2.0 错误对象
/// 包含错误码、消息和可选的附加数据
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcError {
    /// 错误码（负数为标准错误，正数为应用自定义错误）
    pub code: i64,
    /// 人类可读的错误描述
    pub message: String,
    /// 附加错误数据（可选）
    pub data: Option<serde_json::Value>,
}

/// 实现 Display trait，方便错误消息格式化输出
impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 输出格式：[错误码] 错误消息
        write!(f, "[{}] {}", self.code, self.message)
    }
}

// ============================================================================
// MCP 工具定义类型
// ============================================================================

/// MCP 工具定义
/// 由 tools/list 方法返回，描述 MCP 服务器提供的一个工具
#[derive(Debug, Clone, Deserialize)]
pub struct McpToolDef {
    /// 工具名称（在 MCP 服务器内唯一）
    pub name: String,
    /// 工具的自然语言描述（可选）
    pub description: Option<String>,
    /// 工具输入参数的 JSON Schema
    /// 定义工具接受的参数格式，直接透传给 LLM
    #[serde(rename = "inputSchema", default)]
    pub input_schema: serde_json::Value,
}

// ============================================================================
// MCP 工具调用结果类型
// ============================================================================

/// MCP tools/call 的返回结果
/// 包含内容列表和可选的错误标记
#[derive(Debug, Clone, Deserialize)]
pub struct CallToolResult {
    /// 工具返回的内容列表（可以包含多个文本/图片等内容块）
    pub content: Vec<McpContent>,
    /// 是否为错误结果（true 表示工具执行失败）
    #[serde(rename = "isError")]
    pub is_error: Option<bool>,
}

/// MCP 内容块
/// tools/call 返回的单个内容项，当前仅支持文本类型
#[derive(Debug, Clone, Deserialize)]
pub struct McpContent {
    /// 内容类型（"text", "image" 等，当前仅处理 "text"）
    #[serde(rename = "type")]
    pub content_type: String,
    /// 文本内容（仅当 type == "text" 时有值）
    pub text: Option<String>,
}

// ============================================================================
// JSON-RPC 通知类型（无 id 字段）
// ============================================================================

/// JSON-RPC 2.0 通知体（无 id 字段）
/// 通知不期望收到响应，MCP 协议的 "initialized" 即为通知
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcNotification {
    /// JSON-RPC 协议版本，固定为 "2.0"
    pub jsonrpc: String,
    /// 通知的方法名称
    pub method: String,
    /// 通知参数（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}
