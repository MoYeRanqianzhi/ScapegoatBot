//! OneBot 11 客户端对本地模拟服务端的集成测试（随 `cargo test` 运行，不需要 NapCat）。
//!
//! 不依赖 QQ 账号或 NapCat 安装：测试内启动一个本地 WebSocket 服务端，
//! 使用 NapCatLink 的真实客户端完成连接、`get_login_info`、发送消息和接收事件。
//! 这验证的是实际网络协议、NapCat 客户端封装与事件归一化，而不是模拟函数调用。

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use scapegoat_bot::qq::client::QqClient;
use scapegoat_bot::qq::event::{ChatId, QqEvent, normalize};

/// 启动一个最小 OneBot 11 服务端，返回 URL 与收到的 action 记录。
async fn mock_onebot() -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&calls);

    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut socket) = accept_async(stream).await else {
            return;
        };
        while let Some(Ok(message)) = socket.next().await {
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(request) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Some(action) = request.get("action").and_then(Value::as_str) else {
                continue;
            };
            recorded.lock().await.push(request.clone());
            let data = match action {
                "get_login_info" => json!({"user_id": 10000, "nickname": "MockBot"}),
                "get_version_info" => json!({"app_name": "NapCat", "app_version": "4.9.0"}),
                "send_group_msg" => json!({"message_id": 12345}),
                _ => json!({}),
            };
            let response = json!({
                "status": "ok",
                "retcode": 0,
                "data": data,
                "echo": request.get("echo").cloned().unwrap_or(Value::Null),
            });
            if socket
                .send(Message::Text(response.to_string().into()))
                .await
                .is_err()
            {
                return;
            }

            // 发送消息后推送一条真实的群消息事件。
            if action == "send_group_msg" {
                let event = json!({
                    "time": 1_700_000_000u64,
                    "self_id": 10000,
                    "post_type": "message",
                    "message_type": "group",
                    "sub_type": "normal",
                    "message_id": 555,
                    "group_id": 42,
                    "user_id": 10001,
                    "raw_message": "hello",
                    "message": [{"type": "text", "data": {"text": "hello"}}],
                    "sender": {"user_id": 10001, "nickname": "Alice"}
                });
                let _ = socket.send(Message::Text(event.to_string().into())).await;
            }
        }
    });

    (format!("ws://{address}"), calls)
}

#[tokio::test]
async fn napcat_client_connects_sends_and_receives() {
    let (url, calls) = mock_onebot().await;
    let client = QqClient::new(&url, None).unwrap();
    client.connect().await.unwrap();

    let info = client.login_info().await.unwrap();
    assert_eq!(info.user_id, 10000);
    assert_eq!(info.nickname, "MockBot");

    let mut subscription = client.subscribe();
    let message_id = client
        .send_text(ChatId::Group(42), "hello from sbot", None)
        .await
        .unwrap();
    assert_eq!(message_id, 12345);

    let sent = calls.lock().await.clone();
    let send = sent
        .iter()
        .find(|call| call["action"] == "send_group_msg")
        .expect("send_group_msg was not called");
    assert_eq!(send["params"]["group_id"], 42);
    assert!(
        send["params"]["message"]
            .to_string()
            .contains("hello from sbot")
    );

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), subscription.recv())
        .await
        .expect("no event received")
        .expect("event stream closed");
    let normalized = normalize(&event.name, &event.data);
    match normalized {
        QqEvent::Message(message) => {
            assert_eq!(message.chat, ChatId::Group(42));
            assert_eq!(message.message_id, 555);
            assert_eq!(message.sender.qq, Some(10001));
            assert_eq!(message.plain_text(), "hello");
        }
        other => panic!("expected a message event, got {other:?}"),
    }
}

/// 接受一个连接，应答它的每个 action，直到 `stop` 触发；然后按 `close` 的方式结束这条连接。
///
/// 返回这条连接上收到的 action 名。`close` 为 `true` 时发送状态码 1000 的关闭帧（服务端
/// 正常退出）：NapCatLink 只在这个状态码下不再自动重连，其他断开方式它会自己重试。
async fn serve_one_connection(
    listener: &TcpListener,
    stop: tokio::sync::oneshot::Receiver<()>,
    close: bool,
) -> Vec<String> {
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = accept_async(stream).await.unwrap();
    let mut actions = Vec::new();
    let mut stop = stop;
    loop {
        tokio::select! {
            _ = &mut stop => break,
            message = socket.next() => {
                let Some(Ok(Message::Text(text))) = message else {
                    // 客户端断开或发来非文本帧：这条连接到此为止。
                    return actions;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let Some(action) = request.get("action").and_then(Value::as_str) else {
                    continue;
                };
                actions.push(action.to_owned());
                let response = json!({
                    "status": "ok",
                    "retcode": 0,
                    "data": {"user_id": 10000, "nickname": "MockBot"},
                    "echo": request.get("echo").cloned().unwrap_or(Value::Null),
                });
                socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
        }
    }
    if close {
        let normal = tokio_tungstenite::tungstenite::protocol::CloseFrame {
            code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal,
            reason: "server shutting down".into(),
        };
        let _ = socket.close(Some(normal)).await;
    }
    actions
}

/// 服务端以 1000 正常关闭连接后（NapCat 退出再启动），NapCatLink 自己不再重连；
/// `keep_connected` 必须接手把连接恢复，之后的 action 调用照常成功。没有它，守护进程会
/// 一直失联到被重启（去掉 `keep_connected_every` 那一行，本测试在“没有重连”处失败）。
#[tokio::test]
async fn the_client_reconnects_after_the_server_closes_the_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (stop_first, stopped_first) = tokio::sync::oneshot::channel();
    let (stop_second, stopped_second) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let first = serve_one_connection(&listener, stopped_first, true).await;
        // 第二条连接只会在客户端重新连上来时出现。
        let second = serve_one_connection(&listener, stopped_second, false).await;
        (first, second)
    });

    let client = Arc::new(QqClient::new(&url, None).unwrap());
    client.connect().await.unwrap();
    assert_eq!(client.login_info().await.unwrap().user_id, 10000);
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let fast = std::time::Duration::from_millis(50);
    client.keep_connected_every(fast, fast, shutdown_rx);

    // 服务端关闭第一条连接；客户端要先真的掉线（状态不再是已连接）。
    stop_first.send(()).unwrap();
    let wait = |connected: bool| {
        let client = Arc::clone(&client);
        async move {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while client.is_connected() != connected {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .is_ok()
        }
    };
    assert!(wait(false).await, "the client never noticed the close");

    // 监督任务重新连上；之后的调用走第二条连接（服务端的记录在下面核对）。
    assert!(wait(true).await, "the client did not reconnect");
    assert_eq!(client.login_info().await.unwrap().nickname, "MockBot");

    // 停止后监督任务退出，不再占着连接重试。
    shutdown.send(true).unwrap();
    stop_second.send(()).unwrap();
    let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .expect("the mock server did not finish")
        .unwrap();
    assert!(first.contains(&"get_login_info".to_owned()), "{first:?}");
    assert!(second.contains(&"get_login_info".to_owned()), "{second:?}");
}
