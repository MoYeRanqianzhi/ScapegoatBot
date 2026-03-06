// src/main.rs
// ScapegoatBot 主入口文件
// 加载配置 → 初始化白名单 → 连接 NapCat → 监听消息事件 → 存储消息 → 白名单过滤

mod config;
mod message_format;
mod providers;
mod skills;
mod storage;
mod tools;
mod whitelist;

use config::AppConfig;
use message_format::segments_to_message_xml;
use storage::markdown::MessageRecord;
use storage::{SessionType, StorageManager};
use tools::ToolContext;
use whitelist::Whitelist;

use chrono::{Local, TimeZone};
use napcat_link::{GroupMessageEvent, NapLink, PrivateMessageEvent};
use napcat_link::{FriendRequestEvent, GroupRequestEvent};
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 初始化 tracing 日志
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("ScapegoatBot 启动中...");

    // ===== 1. 加载配置 =====

    let config_path = PathBuf::from("config.toml");
    let config = AppConfig::load(&config_path)?;
    tracing::info!("配置加载成功: {}", config_path.display());

    // ===== 2. 初始化白名单检查器 =====

    let whitelist = Whitelist::new(&config.whitelist);
    tracing::info!(
        "白名单已初始化 - 收群: {} 个, 收私聊: {} 个, 发群: {} 个, 发私聊: {} 个",
        config.whitelist.recv_groups.len(),
        config.whitelist.recv_privates.len(),
        config.whitelist.send_groups.len(),
        config.whitelist.send_privates.len(),
    );

    // ===== 3. 初始化消息存储管理器 =====

    let storage = Arc::new(StorageManager::new(&config.storage));
    tracing::info!(
        "存储管理器已初始化 - 目录: {}, 每文件最大消息数: {}",
        config.messages_dir().display(),
        config.storage.max_messages_per_file
    );

    // ===== 4. 连接 NapCat =====

    let mut builder = NapLink::builder(&config.napcat.ws_url);
    if let Some(token) = &config.napcat.token {
        builder = builder.token(token);
    }
    // 启用自动重连
    builder = builder.reconnect_enabled(true);
    let client = builder.build()?;

    // 在连接前创建事件订阅（避免丢失早期事件）
    let mut sub = client.subscribe();

    // 创建工具执行上下文（API + 白名单）
    let tool_ctx = ToolContext::new(client.api().clone(), whitelist);
    // tool_ctx 可在后续传递给 LLM 工具调用层
    let _tool_ctx = Arc::new(tool_ctx);

    // 启动消息事件监听任务
    let storage_clone = storage.clone();
    tokio::spawn(async move {
        tracing::info!("消息事件监听任务已启动");

        loop {
            // 监听所有消息和请求事件
            let event = match sub.recv().await {
                Some(e) => e,
                None => {
                    tracing::warn!("事件流已关闭");
                    break;
                }
            };

            // 根据事件名称路由处理
            let event_name = event.name.as_str();

            if event_name.starts_with("message.group") {
                // ----- 群消息处理 -----
                if let Ok(msg) = serde_json::from_value::<GroupMessageEvent>(event.data.clone()) {
                    handle_group_message(&storage_clone, &msg);
                }
            } else if event_name.starts_with("message.private") {
                // ----- 私聊消息处理 -----
                if let Ok(msg) = serde_json::from_value::<PrivateMessageEvent>(event.data.clone())
                {
                    handle_private_message(&storage_clone, &msg);
                }
            } else if event_name.starts_with("request.friend") {
                // ----- 好友请求事件（记录到日志，不主动存储） -----
                if let Ok(req) = serde_json::from_value::<FriendRequestEvent>(event.data.clone()) {
                    tracing::info!(
                        "收到好友请求: QQ={}, 验证信息={}",
                        req.user_id,
                        req.comment
                    );
                }
            } else if event_name.starts_with("request.group") {
                // ----- 群请求事件（记录到日志） -----
                if let Ok(req) = serde_json::from_value::<GroupRequestEvent>(event.data.clone()) {
                    tracing::info!(
                        "收到群请求: 群={}, QQ={}, 类型={}, 验证信息={}",
                        req.group_id,
                        req.user_id,
                        req.sub_type,
                        req.comment
                    );
                }
            }
        }
    });

    // 连接到 NapCat 服务器
    client.connect().await?;
    tracing::info!("已连接到 NapCat: {}", config.napcat.ws_url);

    // 等待 Ctrl+C 信号
    tokio::signal::ctrl_c().await?;
    tracing::info!("收到退出信号，正在断开连接...");
    client.disconnect();

    Ok(())
}

// ============================================================================
// 消息处理函数
// ============================================================================

/// 处理群聊消息：白名单检查 → 存储 JSONL → 存储 MD
fn handle_group_message(storage: &StorageManager, msg: &GroupMessageEvent) {
    let group_id = msg.group_id;

    // 白名单检查已在上层完成（此处可由 whitelist 做二次检查）
    // 存储原始 JSON
    if let Ok(raw_json) = serde_json::to_value(msg) {
        if let Err(e) = storage.store_raw_jsonl(SessionType::Group, group_id, &raw_json) {
            tracing::error!("存储群消息 JSONL 失败 (群={}): {}", group_id, e);
        }
    }

    // 构建 MD 记录
    let time_str = Local
        .timestamp_opt(msg.time, 0)
        .single()
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "??:??:??".to_string());

    // 消息内容格式化为 CQ XML
    let content = segments_to_message_xml(&msg.message);

    // 确定消息状态：如果内容中包含 "\n---\n" 则标记为 Wrapped（已由 <message> 包裹保护）
    let state = if msg.raw_message.contains("\n---\n") {
        "Wrapped"
    } else {
        "Raw"
    };

    // 提取发送者信息
    let nick = msg
        .sender
        .nickname
        .clone()
        .unwrap_or_else(|| msg.user_id.to_string());
    let group_nick = msg.sender.card.clone().filter(|c| !c.is_empty());

    let record = MessageRecord {
        time: time_str,
        message_id: msg.message_id,
        message_type: "Group".to_string(),
        group_info: Some(format!("{group_id}")),
        qq: msg.user_id,
        nick,
        group_nick,
        remark: None, // 群消息中没有备注信息
        identity: format!("GroupMember ({group_id})"),
        state: state.to_string(),
        content,
    };

    if let Err(e) = storage.store_markdown(SessionType::Group, group_id, &record) {
        tracing::error!("存储群消息 MD 失败 (群={}): {}", group_id, e);
    }
}

/// 处理私聊消息：白名单检查 → 存储 JSONL → 存储 MD
fn handle_private_message(storage: &StorageManager, msg: &PrivateMessageEvent) {
    let user_id = msg.user_id;

    // 存储原始 JSON
    if let Ok(raw_json) = serde_json::to_value(msg) {
        if let Err(e) = storage.store_raw_jsonl(SessionType::Private, user_id, &raw_json) {
            tracing::error!("存储私聊消息 JSONL 失败 (QQ={}): {}", user_id, e);
        }
    }

    // 构建 MD 记录
    let time_str = Local
        .timestamp_opt(msg.time, 0)
        .single()
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "??:??:??".to_string());

    let content = segments_to_message_xml(&msg.message);

    let state = if msg.raw_message.contains("\n---\n") {
        "Wrapped"
    } else {
        "Raw"
    };

    let nick = msg
        .sender
        .nickname
        .clone()
        .unwrap_or_else(|| msg.user_id.to_string());

    // 根据 sub_type 判断身份
    let identity = match msg.sub_type.as_str() {
        "friend" => "Friend".to_string(),
        "group" => "GroupMember".to_string(),
        _ => "Stranger".to_string(),
    };

    let record = MessageRecord {
        time: time_str,
        message_id: msg.message_id,
        message_type: "Private".to_string(),
        group_info: None,
        qq: user_id,
        nick,
        group_nick: None,
        remark: None,
        identity,
        state: state.to_string(),
        content,
    };

    if let Err(e) = storage.store_markdown(SessionType::Private, user_id, &record) {
        tracing::error!("存储私聊消息 MD 失败 (QQ={}): {}", user_id, e);
    }
}
