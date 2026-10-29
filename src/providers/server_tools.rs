//! 服务端工具：声明与结果归一化。
//!
//! 设计依据：设计 §5.3 / §10。
//!
//! - Anthropic 支持 `web_search_20260209` / `web_fetch_20260209`（当前模型）；
//! - OpenAI Responses 支持内置 `web_search`；
//! - Chat Completions 没有服务端工具，必须走用户配置的 HTTP 后端（Phase 4）。

use serde_json::{Value, json};

use super::ServerToolKind;

/// Anthropic 的 web_search 工具版本。
pub const ANTHROPIC_WEB_SEARCH_VERSION: &str = "web_search_20260209";
/// Anthropic 的 web_fetch 工具版本。
pub const ANTHROPIC_WEB_FETCH_VERSION: &str = "web_fetch_20260209";

/// Anthropic 服务端工具声明。
pub fn anthropic_decl(kind: ServerToolKind, max_uses: u32) -> Value {
    match kind {
        ServerToolKind::WebSearch => json!({
            "type": ANTHROPIC_WEB_SEARCH_VERSION,
            "name": "web_search",
            "max_uses": max_uses,
        }),
        ServerToolKind::WebFetch => json!({
            "type": ANTHROPIC_WEB_FETCH_VERSION,
            "name": "web_fetch",
            "max_uses": max_uses,
        }),
    }
}

/// OpenAI Responses 的服务端工具声明。
pub fn openai_decl(kind: ServerToolKind) -> Value {
    match kind {
        ServerToolKind::WebSearch => json!({ "type": "web_search" }),
        // Responses 目前没有独立的 web_fetch 内置工具，统一用 web_search 代偿。
        ServerToolKind::WebFetch => json!({ "type": "web_search" }),
    }
}

/// 把 Anthropic 的服务端工具结果块转成内部表示。
///
/// `block` 是 `content_block_start` 里的完整 content_block；保留原文，
/// 以便历史回传时逐字还原。
pub fn normalize_anthropic_result(block: &Value) -> Option<(String, Value)> {
    let tool_use_id = block.get("tool_use_id")?.as_str()?.to_owned();
    Some((tool_use_id, block.clone()))
}

/// 把历史中的服务端工具块降级为文本（协议回退时使用，设计 §3.2）。
pub fn degrade_to_text(name: &str, content: &Value) -> String {
    let rendered = serde_json::to_string(content).unwrap_or_else(|_| "<unrenderable>".to_owned());
    format!("[server tool {name} result] {rendered}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_declarations_use_current_versions() {
        let search = anthropic_decl(ServerToolKind::WebSearch, 5);
        assert_eq!(search["type"], ANTHROPIC_WEB_SEARCH_VERSION);
        assert_eq!(search["max_uses"], 5);
        let fetch = anthropic_decl(ServerToolKind::WebFetch, 2);
        assert_eq!(fetch["type"], ANTHROPIC_WEB_FETCH_VERSION);
    }

    #[test]
    fn openai_declarations_are_builtin() {
        assert_eq!(openai_decl(ServerToolKind::WebSearch)["type"], "web_search");
    }

    #[test]
    fn results_are_normalized_and_degraded() {
        let block = json!({
            "type": "web_search_tool_result",
            "tool_use_id": "srvtoolu_1",
            "content": [{"type": "web_search_result", "url": "https://example.com"}]
        });
        let (id, content) = normalize_anthropic_result(&block).unwrap();
        assert_eq!(id, "srvtoolu_1");
        assert_eq!(content["type"], "web_search_tool_result");
        assert!(degrade_to_text("web_search", &content).contains("example.com"));
        assert!(normalize_anthropic_result(&json!({})).is_none());
    }
}
