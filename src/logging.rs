//! 日志输出：在写出之前遮蔽凭据。
//!
//! 依赖库会把凭据带进日志——napcat-link 在 INFO 级别打印带 `access_token` 查询参数的
//! WebSocket 地址，连接失败的错误信息里也会带上它。日志会被复制进 issue、发给别人看，
//! 所以在最后一步统一遮蔽，而不是指望每个库、每条日志各自小心。
//!
//! 实现：每个日志事件拿到一个新的 [`Redacting`] 写入器，事件的全部字节先进缓冲，
//! 写入器被丢弃时遮蔽后一次写出（tracing-subscriber 的 fmt 层每个事件取一次写入器）。

use std::io::Write;
use std::sync::LazyLock;

use regex::Regex;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// 要遮蔽的模式：`access_token=<值>`（URL 查询参数）与 `Bearer <值>`（鉴权头）。
static SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(access_token=)[^&\s"'#]+|(bearer\s+)[A-Za-z0-9._~+/=-]+"#)
        .expect("the secret pattern is valid")
});

/// 遮蔽一段日志文本里的凭据。
pub fn redact(text: &str) -> std::borrow::Cow<'_, str> {
    SECRET.replace_all(text, |captures: &regex::Captures<'_>| {
        // 两个分支各有一个前缀捕获组；保留前缀，只换掉值。
        let prefix = captures
            .get(1)
            .or_else(|| captures.get(2))
            .map_or("", |prefix| prefix.as_str());
        format!("{prefix}<redacted>")
    })
}

/// 初始化日志：默认 `info`，`RUST_LOG` 覆盖；输出到 stderr，写出前遮蔽凭据。
///
/// stdout 留给 `start` 时给人看的提示，日志不混进去。
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(RedactingStderr)
        .with_target(false)
        .init();
}

/// 为每个日志事件产出一个写到 stderr 的 [`Redacting`] 写入器。
struct RedactingStderr;

impl<'a> MakeWriter<'a> for RedactingStderr {
    type Writer = Redacting<std::io::Stderr>;

    fn make_writer(&'a self) -> Self::Writer {
        Redacting::new(std::io::stderr())
    }
}

/// 缓冲一个日志事件，丢弃时遮蔽并写出。
pub struct Redacting<W: Write> {
    inner: W,
    buffer: Vec<u8>,
}

impl<W: Write> Redacting<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
        }
    }

    /// 遮蔽缓冲内容并写到底层输出，清空缓冲。
    fn emit(&mut self) -> std::io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&self.buffer);
        let result = self.inner.write_all(redact(&text).as_bytes());
        self.buffer.clear();
        result.and_then(|()| self.inner.flush())
    }
}

impl<W: Write> Write for Redacting<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// 显式 flush 时就写出：凭据模式不会跨越一次 flush（fmt 层只在事件末尾 flush）。
    fn flush(&mut self) -> std::io::Result<()> {
        self.emit()
    }
}

impl<W: Write> Drop for Redacting<W> {
    fn drop(&mut self) {
        // 写日志失败无处可报；丢弃错误，与 stderr 写失败时 tracing 的行为一致。
        let _ = self.emit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_in_urls_and_headers_are_redacted() {
        let line = "connecting to ws://127.0.0.1:3001?foo=1&access_token=s3cr3t-T0ken&x=2 \
                    with Authorization: Bearer abc.DEF-123";
        let redacted = redact(line);
        assert!(!redacted.contains("s3cr3t-T0ken"), "{redacted}");
        assert!(!redacted.contains("abc.DEF-123"), "{redacted}");
        assert!(
            redacted.contains("access_token=<redacted>&x=2"),
            "{redacted}"
        );
        assert!(redacted.contains("Bearer <redacted>"), "{redacted}");
        // 没有凭据的文本原样返回，不分配。
        assert!(matches!(
            redact("plain line"),
            std::borrow::Cow::Borrowed("plain line")
        ));
    }

    #[test]
    fn the_writer_redacts_a_whole_event_split_across_writes() {
        let mut out = Vec::new();
        {
            let mut writer = Redacting::new(&mut out);
            // fmt 层可能分多次写；凭据跨越两次 write 也要被遮蔽。
            writer.write_all(b"url ws://h?access_tok").unwrap();
            writer.write_all(b"en=abcdef\n").unwrap();
        }
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, "url ws://h?access_token=<redacted>\n");
    }
}
