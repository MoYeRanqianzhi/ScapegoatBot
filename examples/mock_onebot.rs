//! 本地 OneBot 11 mock 服务端，用于实机验证 NapCat 客户端与 QQ 管线。
//!
//! 运行 `cargo run --example mock_onebot -- 39001`：
//! - 接受一个 WebSocket 连接；
//! - 回应 `get_login_info` / `get_version_info` / 发送类 action；
//! - 连接建立后推送一条群消息事件；
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

    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = accept_async(stream).await.unwrap();
    println!("CONNECTED");

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

        // 在客户端完成登录/版本查询、订阅建立后推送一条群消息。
        if action == "get_version_info" && !sent_event {
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
