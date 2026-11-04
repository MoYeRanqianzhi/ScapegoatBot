//! 用真实 WebSocket 协议模拟 NapCat/OneBot 11 的 API 响应与事件推送。
//!
//! 这些测试不依赖真实 QQ 或 NapCat：本地服务器按 OneBot 11 的
//! `{action, params, echo}` / `{status, retcode, data, echo}` 形状应答，
//! 同时覆盖 `QqClient` 与 `API` 工具的请求参数。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use napcat_link::MessageSegment;
use scapegoat_bot::qq::QqClient;
use scapegoat_bot::storage::Database;
use scapegoat_bot::storage::crypto::MasterKey;
use scapegoat_bot::tools::ToolContext;
use scapegoat_bot::tools::api::ApiTool;
use scapegoat_bot::tools::{Tool, api::NapcatGateway};
use scapegoat_bot::vfs::quota::Limits;
use scapegoat_bot::vfs::sqlite::VfsStore;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_async, connect_async};

type Records = Arc<Mutex<Vec<(String, Value)>>>;

/// 启动一个 OneBot 11/NapCat 形状的本地 WebSocket 服务，返回连接 URL。
async fn spawn_mock_napcat(records: Records, push_event: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let records = Arc::clone(&records);
            tokio::spawn(async move {
                let Ok(mut socket) = accept_async(stream).await else {
                    return;
                };
                if push_event {
                    let event = json!({
                        "post_type": "message",
                        "message_type": "group",
                        "sub_type": "normal",
                        "time": 1_760_000_000,
                        "self_id": 10000,
                        "message_id": 5,
                        "group_id": 123,
                        "user_id": 20000,
                        "raw_message": "hi",
                        "message": [{"type": "text", "data": {"text": "hi"}}],
                        "sender": {"user_id": 20000, "nickname": "Alice"}
                    });
                    let _ = socket.send(Message::Text(event.to_string().into())).await;
                }
                while let Some(Ok(message)) = socket.next().await {
                    let Message::Text(text) = message else {
                        continue;
                    };
                    let Ok(request) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    let action = request
                        .get("action")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let params = request.get("params").cloned().unwrap_or(Value::Null);
                    records
                        .lock()
                        .unwrap()
                        .push((action.clone(), params.clone()));

                    let data = match action.as_str() {
                        "get_login_info" => json!({"user_id": 10000, "nickname": "MockBot"}),
                        "get_version_info" => {
                            json!({"app_name": "NapCat", "app_version": "4.8.0"})
                        }
                        "get_group_list" => json!([{"group_id": 123, "group_name": "Mock"}]),
                        "get_group_msg_history" => json!({
                            "messages": [{
                                "message_id": 1,
                                "message_type": "group",
                                "group_id": params.get("group_id").cloned().unwrap_or(json!(123)),
                                "user_id": 20000,
                                "raw_message": "hello",
                                "message": [{"type": "text", "data": {"text": "hello"}}],
                                "sender": {"user_id": 20000, "nickname": "Alice"}
                            }]
                        }),
                        "get_group_info" => {
                            let response = json!({
                                "status": "failed",
                                "retcode": 100,
                                "message": "mock failure",
                                "data": null,
                                "echo": request.get("echo").cloned().unwrap_or(Value::Null),
                            });
                            let _ = socket
                                .send(Message::Text(response.to_string().into()))
                                .await;
                            continue;
                        }
                        _ if action.starts_with("send_") => json!({"message_id": 321}),
                        _ => json!({"ok": true}),
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
                }
            });
        }
    });
    format!("ws://{addr}")
}

async fn connect(url: &str) -> Arc<QqClient> {
    let client = Arc::new(QqClient::new(url, None).unwrap());
    tokio::time::timeout(Duration::from_secs(5), client.connect())
        .await
        .expect("connect timed out")
        .expect("connect failed");
    client
}

fn tool_context() -> (ToolContext, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open_in_memory().unwrap());
    let key = Arc::new(MasterKey::from_bytes([31u8; 32]));
    let vfs =
        Arc::new(VfsStore::open(db, key, dir.path().join("data"), Limits::default()).unwrap());
    (ToolContext::new(vfs, None), dir)
}

#[tokio::test]
async fn login_and_version_round_trip_over_onebot_ws() {
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let url = spawn_mock_napcat(Arc::clone(&records), false).await;
    let client = connect(&url).await;

    let login = client.login_info().await.unwrap();
    assert_eq!(login.user_id, 10000);
    assert_eq!(login.nickname, "MockBot");

    let version = client.version_info().await.unwrap();
    assert_eq!(version["app_name"], "NapCat");

    let actions: Vec<String> = records
        .lock()
        .unwrap()
        .iter()
        .map(|(action, _)| action.clone())
        .collect();
    assert!(actions.contains(&"get_login_info".to_owned()));
    assert!(actions.contains(&"get_version_info".to_owned()));
}

#[tokio::test]
async fn send_paths_use_napcat_actions_and_params() {
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let url = spawn_mock_napcat(Arc::clone(&records), false).await;
    let client = connect(&url).await;

    let group_id = client
        .send_message(
            scapegoat_bot::qq::event::ChatId::Group(123),
            vec![MessageSegment::Text {
                text: "hello".to_owned(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(group_id, 321);

    let private_id = client
        .send_message(
            scapegoat_bot::qq::event::ChatId::Private(20000),
            vec![MessageSegment::Text {
                text: "hi".to_owned(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(private_id, 321);

    let records = records.lock().unwrap();
    let group = records
        .iter()
        .find(|(action, _)| action == "send_group_msg")
        .expect("send_group_msg");
    assert_eq!(group.1["group_id"], 123);
    assert_eq!(group.1["message"][0]["type"], "text");
    assert_eq!(group.1["message"][0]["data"]["text"], "hello");

    let private = records
        .iter()
        .find(|(action, _)| action == "send_private_msg")
        .expect("send_private_msg");
    assert_eq!(private.1["user_id"], 20000);
}

#[tokio::test]
async fn api_tool_matches_napcat_request_and_error_shapes() {
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let url = spawn_mock_napcat(Arc::clone(&records), false).await;
    let client = connect(&url).await;
    let tool = ApiTool::new(
        Some(Arc::clone(&client) as Arc<dyn NapcatGateway>),
        None,
        10000,
        10000,
        None,
        vec!["*".to_owned()],
        None,
    );
    let (mut context, _dir) = tool_context();
    context.evidence = scapegoat_bot::agent::TurnEvidence::owner("control");

    let sent = Tool::execute(
        &tool,
        &context,
        json!({
            "action": "napcat.send_group_msg",
            "params": {"group_id": 123, "message": "hello"}
        }),
    )
    .await;
    assert!(!sent.is_error, "{}", sent.content);
    assert!(sent.content.contains("321"));

    let history = Tool::execute(
        &tool,
        &context,
        json!({
            "action": "napcat.get_group_msg_history",
            "params": {"group_id": 123, "count": 5}
        }),
    )
    .await;
    assert!(!history.is_error, "{}", history.content);
    assert!(history.content.contains("hello"));

    // NapCat 的失败响应：status=failed + retcode!=0 应转成工具错误。
    let failure = Tool::execute(
        &tool,
        &context,
        json!({
            "action": "napcat.get_group_info",
            "params": {"group_id": 123}
        }),
    )
    .await;
    assert!(failure.is_error);
    assert!(failure.content.contains("mock failure"));

    let records = records.lock().unwrap();
    let send = records
        .iter()
        .find(|(action, _)| action == "send_group_msg")
        .expect("send_group_msg");
    assert_eq!(send.1["group_id"], 123);
    assert_eq!(send.1["message"][0]["data"]["text"], "hello");
}

#[tokio::test]
async fn websocket_event_push_reaches_the_subscription() {
    let records = Arc::new(Mutex::new(Vec::new()));
    let url = spawn_mock_napcat(Arc::clone(&records), true).await;
    let unconnected = QqClient::new(&url, None).unwrap();
    let mut events = unconnected.subscribe();
    let client = Arc::new(unconnected);
    tokio::time::timeout(Duration::from_secs(5), client.connect())
        .await
        .expect("connect timed out")
        .expect("connect failed");
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv_filter("message"))
        .await
        .expect("no message event pushed")
        .expect("event stream closed");
    assert_eq!(event.data["group_id"], 123);
    assert_eq!(event.data["sender"]["nickname"], "Alice");
}

#[tokio::test]
async fn direct_websocket_handshake_is_accepted() {
    // 用裸 tungstenite 连接，确认 mock 服务与 NapCat 的握手/请求形状一致。
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let url = spawn_mock_napcat(Arc::clone(&records), false).await;
    let (mut socket, _) = connect_async(url).await.unwrap();
    socket
        .send(Message::Text(
            json!({"action": "get_login_info", "params": {}, "echo": "e1"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let reply = socket.next().await.unwrap().unwrap();
    let text = reply.into_text().unwrap();
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["echo"], "e1");
    assert_eq!(value["retcode"], 0);
    assert_eq!(value["data"]["user_id"], 10000);
}

#[tokio::test]
async fn approve_mode_nonce_round_trip_over_napcat() {
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let url = spawn_mock_napcat(Arc::clone(&records), false).await;
    let client = connect(&url).await;
    let approvals = scapegoat_bot::security::action_gate::ApprovalQueue::new();
    let gate = Arc::new(scapegoat_bot::security::action_gate::ActionGate::new(
        &scapegoat_bot::config::GateConfig {
            mode: scapegoat_bot::config::GateMode::Approve,
            max_per_minute: 60,
            llm_review: false,
        },
        Arc::clone(&approvals),
    ));
    let tool = ApiTool::new(
        Some(Arc::clone(&client) as Arc<dyn NapcatGateway>),
        None,
        10000,
        10000,
        None,
        vec!["*".to_owned()],
        None,
    )
    .with_action_gate(Arc::clone(&gate));
    let (mut context, _dir) = tool_context();
    context.evidence = scapegoat_bot::agent::TurnEvidence::owner("control");

    let params = json!({"action": "napcat.send_group_msg",
                        "params": {"group_id": 123, "message": "hello"}});
    let pending = Tool::execute(&tool, &context, params.clone()).await;
    assert!(pending.is_error);
    assert!(pending.content.contains("approval_required"));

    let id = approvals.pending()[0]["id"].as_str().unwrap().to_owned();
    let nonce = approvals.nonce_for(&id).unwrap();
    {
        let records = records.lock().unwrap();
        let notice = records
            .iter()
            .find(|(action, _)| action == "send_private_msg")
            .expect("send_private_msg notification");
        assert_eq!(notice.1["user_id"], 10000);
        assert!(notice.1["message"].as_str().unwrap().contains(&nonce));
    }

    assert!(approvals.approve_by_nonce(&nonce).is_some());
    let allowed = Tool::execute(&tool, &context, params).await;
    assert!(!allowed.is_error, "{}", allowed.content);
    assert!(
        records
            .lock()
            .unwrap()
            .iter()
            .any(|(action, _)| action == "send_group_msg")
    );
}
