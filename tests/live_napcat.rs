//! 真实 OneBot 11 WebSocket 集成测试。
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
