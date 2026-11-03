//! Server-Sent Events 解析。
//!
//! 三种协议都用 SSE，但语义不同；这里只负责把字节流切成“事件 + data”，
//! 协议解释留给各自的 provider。
//!
//! 细节：
//! - 支持 LF 与 CRLF；
//! - `data:` 多行按 SSE 规范用 `\n` 连接；
//! - 空行结束一个事件；
//! - 流结束时残留的半个事件会被丢弃（宁可丢也不误处理）；
//! - 换行在缓冲区层面归一，避免 CRLF 跨 chunk 时拆开。

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
        self.buffer.push_str(chunk);
        // 统一换行，后续只处理 `\n`。必须在缓冲区上做：`\r` 与 `\n`
        // 可能分别落在两个网络 chunk 里。
        if self.buffer.contains('\r') {
            self.buffer = self.buffer.replace("\r\n", "\n");
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

/// 增量 UTF-8 解码器。
///
/// 网络 chunk 的边界与字符边界无关；对每个 chunk 单独 `from_utf8_lossy`
/// 会把跨 chunk 的多字节字符（中文、emoji）静默替换成 U+FFFD。
/// 这里保留不完整的尾字节，等下一个 chunk 拼接后再解码。
#[derive(Debug, Default)]
pub struct Utf8Stream {
    carry: Vec<u8>,
}

impl Utf8Stream {
    pub fn new() -> Self {
        Self::default()
    }

    /// 输入一段字节，返回已能完整解码的文本。
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.carry.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.carry) {
                Ok(text) => {
                    out.push_str(text);
                    self.carry.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    out.push_str(
                        std::str::from_utf8(&self.carry[..valid])
                            .expect("valid_up_to always splits on a char boundary"),
                    );
                    match error.error_len() {
                        // 真正非法的字节序列：替换并继续扫描。
                        Some(len) => {
                            out.push('\u{FFFD}');
                            self.carry.drain(..valid + len);
                        }
                        // 末尾是不完整的字符：留到下一次 push。
                        None => {
                            self.carry.drain(..valid);
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    /// 流结束：把残留字节按 lossy 方式送出（不完整说明服务端截断了）。
    pub fn finish(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.carry)).into_owned()
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
    fn crlf_split_across_chunks_is_normalized() {
        let mut parser = SseParser::new();
        assert!(parser.push("data: hello\r").is_empty());
        let events = parser.push("\n\r\ndata: next\r\n\r\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "hello");
        assert_eq!(events[1].data, "next");
    }

    #[test]
    fn utf8_stream_keeps_split_characters() {
        let text = "你好，世界";
        let bytes = text.as_bytes();
        let split = 4; // 切在“好”的中间
        let mut stream = Utf8Stream::new();
        let first = stream.push(&bytes[..split]);
        let second = stream.push(&bytes[split..]);
        assert_eq!(format!("{first}{second}"), text);
        assert!(stream.finish().is_empty());
    }

    #[test]
    fn utf8_stream_replaces_genuinely_invalid_bytes() {
        let mut stream = Utf8Stream::new();
        let output = stream.push(&[0xff, b'a']);
        assert!(output.contains('a'));
        assert!(output.contains('\u{FFFD}'));
    }

    #[test]
    fn utf8_stream_flushes_incomplete_tail() {
        let mut stream = Utf8Stream::new();
        assert!(stream.push(&"你".as_bytes()[..1]).is_empty());
        assert_eq!(stream.finish(), "\u{FFFD}");
    }

    #[test]
    fn ignores_comments_and_empty_blocks() {
        let mut parser = SseParser::new();
        let events = parser.push(": keepalive\n\n\ndata: x\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "x");
    }
}
