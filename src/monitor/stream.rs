//! `stream` 源：外部 WebSocket / SSE 事件流。
//!
//! 只能写在配置文件里（主人决定连到哪里）；`Monitor` 工具与 `Agent` 绑定一律拒绝。
//! 每个 stream monitor 自己维护一条连接，收到的每条消息变成一个 `event.type == "stream"`
//! 的事件，按该 monitor 的 `when` 过滤后走正常的批处理与投递——投递即输入，照常过输入闸。
//!
//! - WebSocket：每个文本帧一个事件；二进制帧只记日志。单条消息上限 1 MiB。
//! - SSE：每个 `data` 事件一个事件；带 `event:` 名字时文本前缀 `[<name>] `。
//! - 事件文本最多 [`MAX_EVENT_CHARS`] 字符，超出截断并标注。
//! - 断线后指数退避重连（5 秒起，最长 5 分钟）。连接状态变化（断开、恢复）各发一条
//!   系统通知：静默不等于正常，但持续断线不会每次重试都通知一次。
//! - 监视器被删除后，连接在 [`IDLE_CHECK`] 内关闭。

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tracing::{debug, warn};

use super::{Monitor, MonitorEvent, MonitorSink, enqueue, matches};
use crate::config::StreamKind;
use crate::providers::sse::{SseParser, Utf8Stream};

/// 单个事件文本的字符上限：外部流不能一次塞满上下文。
const MAX_EVENT_CHARS: usize = 4_000;
/// WebSocket 单条消息 / 单帧的字节上限。
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
/// 连接或读取最多静默这么久就检查一次监视器是否已被删除。
const IDLE_CHECK: Duration = Duration::from_secs(30);
/// 建立连接的超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// 首次重连等待；每次失败翻倍。
const BACKOFF_START: Duration = Duration::from_secs(5);
/// 重连等待上限。
const BACKOFF_MAX: Duration = Duration::from_secs(300);

/// 一次连接的结局。
enum Ended {
    /// 监视器已删除：退出。
    Cancelled,
    /// 连接结束或失败；`healthy` 表示这次曾经连上过。
    Dropped { healthy: bool, reason: String },
}

/// 启动一条 stream 连接任务。
pub(super) fn spawn_stream(
    monitor: Arc<Monitor>,
    sink: Arc<dyn MonitorSink>,
    url: String,
    kind: StreamKind,
) {
    tokio::spawn(async move {
        // 上一次通知时连接是否正常；初始当作正常，第一次连不上也会通知。
        let mut reported_up = true;
        let mut backoff = BACKOFF_START;
        loop {
            let ended = match kind {
                StreamKind::Websocket => {
                    run_websocket(&monitor, &sink, &url, &mut reported_up).await
                }
                StreamKind::Sse => run_sse(&monitor, &sink, &url, &mut reported_up).await,
            };
            let (healthy, reason) = match ended {
                Ended::Cancelled => break,
                Ended::Dropped { healthy, reason } => (healthy, reason),
            };
            if healthy {
                backoff = BACKOFF_START;
            }
            warn!(monitor = %monitor.name, %reason, "stream disconnected");
            if reported_up {
                reported_up = false;
                sink.deliver(monitor.system_delivery(
                    "stream",
                    format!(
                        "system: monitor {:?} lost its stream ({reason}); reconnecting with backoff",
                        monitor.name
                    ),
                ))
                .await;
            }
            // 分段等待，期间监视器被删除就立即退出。
            let mut waited = Duration::ZERO;
            while waited < backoff {
                if monitor.cancelled.load(Ordering::SeqCst) {
                    return;
                }
                let step = (backoff - waited).min(IDLE_CHECK);
                tokio::time::sleep(step).await;
                waited += step;
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
        debug!(monitor = %monitor.name, "stream monitor stopped");
    });
}

/// 连上之后调用：之前报告过断线就补一条“已恢复”。
async fn mark_connected(monitor: &Monitor, sink: &Arc<dyn MonitorSink>, reported_up: &mut bool) {
    if !*reported_up {
        *reported_up = true;
        sink.deliver(monitor.system_delivery(
            "stream",
            format!(
                "system: monitor {:?} reconnected to its stream",
                monitor.name
            ),
        ))
        .await;
    }
}

/// 一条外部消息 → 按 `when` 过滤后入队。
async fn handle(monitor: &Arc<Monitor>, sink: &Arc<dyn MonitorSink>, text: String) {
    let event = MonitorEvent {
        kind: "stream".to_owned(),
        text: truncate(text),
        ..MonitorEvent::default()
    };
    match matches(monitor.when.as_deref(), &event) {
        Ok(true) => enqueue(monitor, sink, &event).await,
        Ok(false) => {}
        // `when` 在添加时已校验；这里不应发生，只记日志。
        Err(error) => warn!(%error, monitor = %monitor.name, "stream predicate failed"),
    }
}

/// 截到 [`MAX_EVENT_CHARS`] 字符并标注。
fn truncate(text: String) -> String {
    match text.char_indices().nth(MAX_EVENT_CHARS) {
        Some((cut, _)) => format!("{}… [truncated]", &text[..cut]),
        None => text,
    }
}

/// WebSocket：连接并读到断开或监视器被删除。
async fn run_websocket(
    monitor: &Arc<Monitor>,
    sink: &Arc<dyn MonitorSink>,
    url: &str,
    reported_up: &mut bool,
) -> Ended {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let connect = tokio_tungstenite::connect_async_with_config(url, Some(config), false);
    let mut socket = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok((socket, _))) => socket,
        Ok(Err(error)) => {
            return Ended::Dropped {
                healthy: false,
                reason: format!("cannot connect: {error}"),
            };
        }
        Err(_) => {
            return Ended::Dropped {
                healthy: false,
                reason: "connect timed out".to_owned(),
            };
        }
    };
    mark_connected(monitor, sink, reported_up).await;
    loop {
        if monitor.cancelled.load(Ordering::SeqCst) {
            let _ = socket.close(None).await;
            return Ended::Cancelled;
        }
        // 读取带超时，只是为了定期检查删除；超时本身不算断线。
        let frame = match tokio::time::timeout(IDLE_CHECK, socket.next()).await {
            Err(_) => continue,
            Ok(frame) => frame,
        };
        match frame {
            Some(Ok(Message::Text(text))) => handle(monitor, sink, text.as_str().to_owned()).await,
            Some(Ok(Message::Binary(bytes))) => {
                debug!(monitor = %monitor.name, len = bytes.len(), "ignoring a binary stream frame");
            }
            // Ping 由协议层自动回复；Pong 与原始帧不需要处理。
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
            Some(Ok(Message::Close(frame))) => {
                return Ended::Dropped {
                    healthy: true,
                    reason: match frame {
                        Some(frame) => format!("closed by the server ({})", frame.code),
                        None => "closed by the server".to_owned(),
                    },
                };
            }
            Some(Err(error)) => {
                return Ended::Dropped {
                    healthy: true,
                    reason: error.to_string(),
                };
            }
            None => {
                return Ended::Dropped {
                    healthy: true,
                    reason: "connection ended".to_owned(),
                };
            }
        }
    }
}

/// SSE：GET 一个 `text/event-stream`，读到断开或监视器被删除。
async fn run_sse(
    monitor: &Arc<Monitor>,
    sink: &Arc<dyn MonitorSink>,
    url: &str,
    reported_up: &mut bool,
) -> Ended {
    // 不设总超时：事件流本来就是长连接；只限制建立连接的时间。
    let client = match reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return Ended::Dropped {
                healthy: false,
                reason: format!("cannot build an HTTP client: {error}"),
            };
        }
    };
    let request = client
        .get(url)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .send();
    let mut response = match tokio::time::timeout(CONNECT_TIMEOUT, request).await {
        Ok(Ok(response)) if response.status().is_success() => response,
        Ok(Ok(response)) => {
            return Ended::Dropped {
                healthy: false,
                reason: format!("HTTP {}", response.status()),
            };
        }
        Ok(Err(error)) => {
            return Ended::Dropped {
                healthy: false,
                reason: format!("cannot connect: {error}"),
            };
        }
        Err(_) => {
            return Ended::Dropped {
                healthy: false,
                reason: "connect timed out".to_owned(),
            };
        }
    };
    mark_connected(monitor, sink, reported_up).await;
    let mut utf8 = Utf8Stream::new();
    let mut parser = SseParser::new();
    loop {
        if monitor.cancelled.load(Ordering::SeqCst) {
            return Ended::Cancelled;
        }
        let chunk = match tokio::time::timeout(IDLE_CHECK, response.chunk()).await {
            Err(_) => continue,
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => {
                return Ended::Dropped {
                    healthy: true,
                    reason: "the server ended the stream".to_owned(),
                };
            }
            Ok(Err(error)) => {
                return Ended::Dropped {
                    healthy: true,
                    reason: error.to_string(),
                };
            }
        };
        for event in parser.push(&utf8.push(&chunk)) {
            let text = match event.event.as_deref() {
                Some(name) if name != "message" => format!("[{name}] {}", event.data),
                _ => event.data,
            };
            handle(monitor, sink, text).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::{MonitorDelivery, MonitorManager};
    use futures_util::SinkExt;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recording(Mutex<Vec<MonitorDelivery>>);

    #[async_trait::async_trait]
    impl MonitorSink for Recording {
        async fn deliver(&self, delivery: MonitorDelivery) {
            self.0.lock().unwrap().push(delivery);
        }
    }

    /// 等到收到 `count` 条投递（最多 5 秒）。
    async fn wait_for(sink: &Recording, count: usize) -> Vec<MonitorDelivery> {
        for _ in 0..500 {
            if sink.0.lock().unwrap().len() >= count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        sink.0.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn websocket_frames_become_filtered_events_and_drops_are_reported() {
        // 本地 WebSocket 服务：发两帧（只有一帧满足 when）后关闭连接。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket.send(Message::text("build #1 passed")).await.unwrap();
            socket
                .send(Message::text("deploy failed on prod"))
                .await
                .unwrap();
            let _ = socket.close(None).await;
        });

        let sink = Arc::new(Recording::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        let config = crate::config::MonitorConfig {
            name: "ci".to_owned(),
            source: crate::config::MonitorSource {
                stream: Some(crate::config::StreamSource {
                    url: format!("ws://{addr}"),
                    kind: StreamKind::Websocket,
                }),
                ..Default::default()
            },
            when: Some("event.type == 'stream' and event.text contains 'failed'".to_owned()),
            ..Default::default()
        };
        crate::monitor::load_config_monitors(&manager, &[config], None)
            .await
            .unwrap();

        // 一条匹配的事件 + 服务端关闭后的一条断线通知。事件经过批处理窗口，
        // 两者谁先到不确定。
        let deliveries = wait_for(&sink, 2).await;
        assert_eq!(deliveries.len(), 2, "{deliveries:?}");
        let event = deliveries.iter().find(|delivery| !delivery.system).unwrap();
        assert_eq!(event.text, "[monitor ci | stream]\ndeploy failed on prod");
        let notice = deliveries.iter().find(|delivery| delivery.system).unwrap();
        assert!(notice.text.contains("lost its stream"), "{deliveries:?}");

        // 删除后连接任务退出，不再重连。
        assert!(manager.remove("ci").await);
    }

    #[test]
    fn long_events_are_truncated_on_a_char_boundary() {
        let text = "汉".repeat(MAX_EVENT_CHARS + 10);
        let cut = truncate(text);
        assert!(cut.ends_with("… [truncated]"));
        assert_eq!(
            cut.chars().filter(|ch| *ch == '汉').count(),
            MAX_EVENT_CHARS
        );
        assert_eq!(truncate("short".to_owned()), "short");
    }
}
