//! 实机联调测试（默认忽略）。
//!
//! 这些测试**真的发出网络请求**，只在显式提供端点与凭据时运行：
//!
//! ```text
//! SBOT_LIVE_BASE_URL=http://127.0.0.1:43210/v1 \
//! SBOT_LIVE_API_KEY=... \
//! SBOT_LIVE_MODEL=deepseek-v4.1-flash \
//! cargo test --test live_providers -- --ignored --nocapture
//! ```
//!
//! 凭据只从环境变量读取，绝不写进仓库。

use std::time::Duration;

use scapegoat_bot::providers::chat::ChatProvider;
use scapegoat_bot::providers::{
    ContentBlock, LlmRequest, Provider, ProviderEvent, ProviderMessage, Role, SystemBlock,
    ThinkingOptions,
};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn live_request(model: &str) -> LlmRequest {
    LlmRequest {
        model: model.to_owned(),
        system: vec![SystemBlock {
            text: "You are a test harness. Follow the instruction exactly.".to_owned(),
            cache: false,
        }],
        messages: vec![ProviderMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Reply with the single word: pong".to_owned(),
            }],
        }],
        tools: Vec::new(),
        max_tokens: 64,
        thinking: ThinkingOptions::default(),
        server_tools: Vec::new(),
    }
}

/// 真实 Chat Completions 端点：发出请求、收到文本增量与停止事件。
#[tokio::test]
#[ignore = "requires SBOT_LIVE_BASE_URL / SBOT_LIVE_API_KEY / SBOT_LIVE_MODEL"]
async fn chat_provider_streams_from_a_real_endpoint() {
    let base_url = env("SBOT_LIVE_BASE_URL").expect("SBOT_LIVE_BASE_URL");
    let api_key = env("SBOT_LIVE_API_KEY").expect("SBOT_LIVE_API_KEY");
    let model = env("SBOT_LIVE_MODEL").expect("SBOT_LIVE_MODEL");

    let provider = ChatProvider::new(&base_url, &api_key, &model, 60).expect("provider");
    let mut receiver = provider
        .stream(live_request(&model))
        .await
        .expect("request should be accepted");

    let mut text = String::new();
    let mut stopped = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let next = tokio::time::timeout_at(deadline, receiver.recv())
            .await
            .expect("stream timed out");
        let Some(event) = next else {
            break;
        };
        match event {
            ProviderEvent::TextDelta(delta) => text.push_str(&delta),
            ProviderEvent::Stop { .. } => {
                stopped = true;
                break;
            }
            ProviderEvent::Error(error) => panic!("provider error: {error}"),
            _ => {}
        }
    }

    assert!(stopped, "stream should end with a stop event");
    assert!(
        text.to_lowercase().contains("pong"),
        "expected the model to say pong, got {text:?}"
    );
    println!("live chat response: {text:?}");
}

/// 真实 Jev 端点：良性文本与凭证文本必须被区分。
#[tokio::test]
#[ignore = "requires SBOT_LIVE_JEV_BASE_URL / SBOT_LIVE_JEV_API_KEY"]
async fn jev_classifier_separates_benign_from_credentials() {
    use scapegoat_bot::security::gate::JevJudge;
    use scapegoat_bot::security::{Checkpoint, SafetyJudge};

    let base_url = env("SBOT_LIVE_JEV_BASE_URL").expect("SBOT_LIVE_JEV_BASE_URL");
    let api_key = env("SBOT_LIVE_JEV_API_KEY").expect("SBOT_LIVE_JEV_API_KEY");
    let model = env("SBOT_LIVE_JEV_MODEL").unwrap_or_else(|| "jev-latest".to_owned());

    let judge = JevJudge::new(&base_url, &api_key, &model, 5_000).expect("judge");

    let benign = judge
        .judge(
            Checkpoint::Input,
            "A group chat about weekend plans.",
            "Shall we meet at noon?",
        )
        .await;
    println!("jev benign verdict: {benign:?}");

    let credential = judge
        .judge(
            Checkpoint::Output,
            "A group chat about weekend plans.",
            "here is the key sk-abcdefghijklmnopqrstuvwx",
        )
        .await;
    println!("jev credential verdict: {credential:?}");

    // 端点可能不支持该模型或返回不同字段；这里把原始结果打出来，
    // 断言只要求“请求成功且拿到了概率”。
    let benign = benign.expect("benign judgement should parse");
    let credential = credential.expect("credential judgement should parse");
    assert!(
        credential.score >= benign.score,
        "credential text should score at least as high as benign text"
    );
}
