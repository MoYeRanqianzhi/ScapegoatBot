//! Server-Sent Events 解析。
//!
//! 三种协议都用 SSE，但语义不同；这里只负责把字节流切成“事件 + data”，
//! 协议解释留给各自的 provider。
//!
//! 细节：
//! - 支持 LF 与 CRLF；
//! - `data:` 多行按 SSE 规范用 `\n` 连接；
//! - 空行结束一个事件；
//! - 流结束时残留的半个事件会被丢弃（宁可丢也不误处理）。

/// 一个 SSE 事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// `event:` 字段；OpenAI 系协议通常不用。
    pub event: Option<String>,
    /// `data:` 字段拼接结果。
    pub data: String,
}

/// 增量解析器。
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: String,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 输入一段文本，返回已经完整的事件。
    pub fn push(&mut self, chunk: &str) -> Vec<SseEvent> {
        // 统一换行，后续只处理 `\n`。
        if chunk.contains("\r\n") {
            self.buffer.push_str(&chunk.replace("\r\n", "\n"));
        } else {
            self.buffer.push_str(chunk);
        }

        let mut events = Vec::new();
        while let Some(position) = self.buffer.find("\n\n") {
            let block: String = self.buffer.drain(..position + 2).collect();
            if let Some(event) = parse_block(block.trim_end_matches('\n')) {
                events.push(event);
            }
        }
        events
    }

    /// 流结束：如果缓冲区里还有完整的 `data` 行（没有以空行结尾），也返回它。
    ///
    /// 有些实现会在最后一个事件后直接断开连接；丢掉它会漏掉 `message_stop`。
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let remaining = std::mem::take(&mut self.buffer);
        let trimmed = remaining.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        parse_block(trimmed).into_iter().collect()
    }
}

/// 解析一个事件块。
fn parse_block(block: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data_lines: Vec<&str> = Vec::new();

    for line in block.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            // 规范：冒号后的一个前导空格要去掉。
            data_lines.push(value.strip_prefix(' ').unwrap_or(value));
        }
        // 其它字段（id / retry / 注释）对模型无关，忽略。
    }

    if data_lines.is_empty() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data_lines.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_events() {
        let mut parser = SseParser::new();
        let events =
            parser.push("event: message_start\ndata: {\"a\":1}\n\nevent: ping\ndata: {}\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(events[1].event.as_deref(), Some("ping"));
    }

    #[test]
    fn handles_crlf_and_split_chunks() {
        let mut parser = SseParser::new();
        assert!(parser.push("data: hel").is_empty());
        let events = parser.push("lo\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn joins_multiline_data() {
        let mut parser = SseParser::new();
        let events = parser.push("data: line1\ndata: line2\n\n");
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn finish_returns_trailing_event_without_blank_line() {
        let mut parser = SseParser::new();
        assert!(parser.push("data: tail").is_empty());
        let events = parser.finish();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "tail");
    }

    #[test]
    fn ignores_comments_and_empty_blocks() {
        let mut parser = SseParser::new();
        let events = parser.push(": keepalive\n\n\ndata: x\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "x");
    }
}
