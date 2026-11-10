//! 本地 OneBot 11 mock 服务端，用于实机验证 NapCat 客户端与 QQ 管线。
//!
//! 运行 `cargo run --example mock_onebot -- 39001`：
//! - 一次服务一个 WebSocket 连接，断开后接着等下一个（守护进程重连时用得上）；
//! - 回应 `get_login_info` / `get_version_info` / 发送类 action；
//! - 每个连接第一次 `get_login_info` 之后 3 秒推送一条群消息事件（守护进程登录后要先建好
//!   agent 才订阅事件，推早了收不到）；
//! - 每个收到的 action 打印为一行 JSON（stdout），便于测试脚本断言。

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(39001);
    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    println!("READY {}", listener.local_addr().unwrap());

    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let Ok(socket) = accept_async(stream).await else {
            continue;
        };
        println!("CONNECTED");
        serve(socket).await;
        println!("DISCONNECTED");
    }
}

/// 服务一个连接，直到对端断开。
async fn serve(mut socket: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) {
    let mut sent_event = false;

    while let Some(Ok(message)) = socket.next().await {
        let Message::Text(text) = message else {
            continue;
        };
        let Ok(request) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        println!("ACTION {}", request);
        let action = request.get("action").and_then(Value::as_str).unwrap_or("");
        let data = match action {
            "get_login_info" => json!({"user_id": 10000, "nickname": "MockBot"}),
            "get_version_info" => json!({"app_name": "NapCat", "app_version": "4.9.0"}),
            "send_group_msg" | "send_private_msg" => json!({"message_id": 777}),
            _ => json!({}),
        };
        let response = json!({
            "status": "ok",
            "retcode": 0,
            "data": data,
            "echo": request.get("echo").cloned().unwrap_or(Value::Null),
        });
        let _ = socket
            .send(Message::Text(response.to_string().into()))
            .await;

        // 客户端登录之后、订阅建立之后推送一条群消息。
        if action == "get_login_info" && !sent_event {
            sent_event = true;
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let event = json!({
                "time": 1_700_000_000u64,
                "self_id": 10000,
                "post_type": "message",
                "message_type": "group",
                "sub_type": "normal",
                "message_id": 901,
                "group_id": 42,
                "user_id": 10001,
                "raw_message": "你好，在吗？",
                "message": [{"type": "text", "data": {"text": "你好，在吗？"}}],
                "sender": {"user_id": 10001, "nickname": "Alice"}
            });
            let _ = socket.send(Message::Text(event.to_string().into())).await;
        }
    }
}
