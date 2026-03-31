// src/main.rs
// ScapegoatBot 主入口文件
// 职责：CLI 解析与路由 → 守护进程模式 / 控制指令转发
// 架构：main() 解析命令行 → run_daemon() 执行守护进程逻辑 → send_control() 发送控制指令

// ============================================================================
// 模块声明
// ============================================================================

mod agent;
mod agent_state;
mod cli;
mod control;
mod control_server;
mod compact;
mod config;
mod group_role;
mod mcp;
mod message_format;
mod prompt;
mod providers;
mod skills;
mod storage;
mod swarm;
mod tools;
mod whitelist;

// ============================================================================
// 导入
// ============================================================================

use config::AppConfig;
use message_format::{needs_wrapping, segments_to_message_xml};
use skills::SkillRegistry;
use storage::markdown::{MessageRecord, MessageState};
use storage::images::ImageStore;
use storage::message_index::MessageIndex;
use storage::StorageManager;
use tools::ToolContext;
use whitelist::Whitelist;

use chrono::{Local, TimeZone};
use clap::Parser;
use napcat_link::{GroupMessageEvent, MessageSegment, NapLink, PrivateMessageEvent};
use napcat_link::{FriendRequestEvent, GroupRequestEvent};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

// ============================================================================
// main() — CLI 解析与命令路由
// ============================================================================

/// 程序入口：解析命令行参数，根据子命令路由到对应处理逻辑
/// - 无子命令 / start → 启动守护进程（前台或后台）
/// - stop → 向运行中的守护进程发送关闭指令
/// - restart → 先关闭再重新启动
/// - status → 查询运行状态
/// - ai → 控制 AI Agent 开关
/// - chat → 向 AI Agent 发送自然语言指令
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 解析命令行参数（clap derive 模式）
    let cli = cli::Cli::parse();

    // 根据子命令类型分发到不同的处理路径
    match cli.command {
        // ---- 无子命令或 start 子命令：启动守护进程 ----
        None | Some(cli::Commands::Start { .. }) => {
            // 检查是否指定了 --daemon 标志（后台模式）
            let daemon = matches!(&cli.command, Some(cli::Commands::Start { daemon: true }));
            if daemon {
                // 后台模式：spawn 一个 detached 子进程
                spawn_daemon(&cli.config)?;
            } else {
                // 前台模式：在当前进程中运行守护进程
                run_daemon(&cli.config).await?;
            }
        }
        // ---- restart 子命令：先停止再启动 ----
        Some(cli::Commands::Restart { daemon }) => {
            // 先尝试关闭已运行的实例（忽略连接失败，可能没有运行中的实例）
            let _ = send_control(&cli.config, control::ControlRequest::Shutdown).await;
            // 等待旧进程退出（给守护进程一点时间清理资源）
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            // 根据 --daemon 标志决定启动模式
            if daemon {
                // 后台模式：spawn detached 子进程
                spawn_daemon(&cli.config)?;
            } else {
                // 前台模式：在当前进程中运行
                run_daemon(&cli.config).await?;
            }
        }
        // ---- stop 子命令：发送关闭指令到运行中的守护进程 ----
        Some(cli::Commands::Stop) => {
            // 连接到控制端口并发送 Shutdown 请求
            let resp = send_control(&cli.config, control::ControlRequest::Shutdown).await?;
            // 输出服务端返回的确认消息
            println!("{}", resp.message);
        }
        // ---- status 子命令：查询守护进程运行状态 ----
        Some(cli::Commands::Status) => {
            // 连接到控制端口并发送 Status 请求
            let resp = send_control(&cli.config, control::ControlRequest::Status).await?;
            // 输出状态信息（包含 AI Agent 是否激活等）
            println!("{}", resp.message);
        }
        // ---- ai 子命令：控制 AI Agent 开关 ----
        Some(cli::Commands::Ai { action }) => {
            // 将 CLI 层的 AiAction 映射为控制协议层的 AiControlAction
            let ctrl_action = match action {
                cli::AiAction::Start => control::AiControlAction::Start,
                cli::AiAction::Stop => control::AiControlAction::Stop,
                cli::AiAction::Restart => control::AiControlAction::Restart,
            };
            // 发送 AI 控制指令到守护进程
            let resp = send_control(
                &cli.config,
                control::ControlRequest::Ai { action: ctrl_action },
            )
            .await?;
            // 输出操作结果
            println!("{}", resp.message);
        }
        // ---- chat 子命令：发送自然语言指令到 AI Agent ----
        Some(cli::Commands::Chat {
            message,
            group,
            private,
        }) => {
            // 发送 Chat 指令，附带可选的群/私聊过滤条件
            let resp = send_control(
                &cli.config,
                control::ControlRequest::Chat {
                    message,
                    group_filter: group,
                    private_filter: private,
                },
            )
            .await?;
            // 输出 AI Agent 的确认消息
            println!("{}", resp.message);
        }
    }

    Ok(())
}

// ============================================================================
// send_control() — CLI 客户端：连接控制端口发送指令
// ============================================================================

/// CLI 客户端函数：加载配置 → 连接控制端口 → 发送请求 → 接收响应
/// 用于 stop / status / ai / chat 等非守护进程命令
///
/// # 参数
/// - `config_path`：配置文件路径（用于读取控制端口号）
/// - `req`：要发送的控制请求
///
/// # 返回
/// 成功时返回服务端的 ControlResponse，连接失败时返回友好的错误提示
async fn send_control(
    config_path: &str,
    req: control::ControlRequest,
) -> anyhow::Result<control::ControlResponse> {
    // 加载配置文件以获取控制端口号
    let config = config::AppConfig::load(std::path::Path::new(config_path))?;
    // 从配置中读取控制通道端口
    let port = config.control.port;
    // 连接到守护进程的控制端口（仅限 localhost）
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "无法连接到 sbot（端口 {port}），请确认 sbot 是否正在运行"
            )
        })?;
    // 通过长度前缀帧协议发送请求
    control::send_msg(&mut stream, &req).await?;
    // 接收并反序列化服务端响应
    let resp: control::ControlResponse = control::recv_msg(&mut stream).await?;
    Ok(resp)
}

// ============================================================================
// run_daemon() — 守护进程主函数
// ============================================================================

/// 守护进程主函数：执行完整的初始化流程并进入消息监听循环
/// 包含：日志初始化 → 配置加载 → 白名单 → 存储 → Skill → NapCat 连接 → 事件监听
/// 同时启动控制通道服务端，支持远程管理指令
///
/// # 参数
/// - `config_path`：配置文件路径
async fn run_daemon(config_path: &str) -> anyhow::Result<()> {
    // 初始化 tracing 日志（支持 RUST_LOG 环境变量，默认 info 级别）
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("ScapegoatBot 启动中...");

    // ===== 1. 加载配置 =====

    // 从指定路径加载 TOML 配置文件（含白名单子集校验）
    let config_path_buf = PathBuf::from(config_path);
    let config = AppConfig::load(&config_path_buf)?;
    tracing::info!("配置加载成功: {}", config_path_buf.display());

    // ===== 2. 初始化白名单检查器 =====

    // 根据配置构建白名单实例（收/发分离的四级名单）
    let whitelist = Whitelist::new(&config.whitelist);
    tracing::info!(
        "白名单已初始化 - 收群: {} 个, 收私聊: {} 个, 发群: {} 个, 发私聊: {} 个",
        config.whitelist.recv_groups.len(),
        config.whitelist.recv_privates.len(),
        config.whitelist.send_groups.len(),
        config.whitelist.send_privates.len(),
    );

    // ===== 3. 初始化消息存储管理器 =====

    // 创建存储管理器，负责 JSONL 和 Markdown 格式的消息持久化
    let storage = Arc::new(StorageManager::new(&config.storage));
    tracing::info!(
        "存储管理器已初始化 - 目录: {}, 每文件最大消息数: {}",
        config.messages_dir().display(),
        config.storage.max_messages_per_file
    );

    // ===== 3.5 加载 Skill 注册表 =====

    // 默认包含 workspace/skills 目录，加上配置中指定的额外目录
    let mut skill_dirs = config.skills.resolve_dirs();
    // 确保 workspace/skills 始终在扫描路径中
    let workspace_skills = PathBuf::from(&config.storage.workspace_dir).join("skills");
    if !skill_dirs.iter().any(|d| d == &workspace_skills) {
        skill_dirs.push(workspace_skills);
    }
    // 扫描所有 skill 目录，收集包含 SKILL.md 的子目录
    let skill_registry = SkillRegistry::scan(&skill_dirs)?;
    tracing::info!(
        "Skill 注册表已加载 - {} 个技能",
        skill_registry.len()
    );

    // ===== 4. 连接 NapCat =====

    // 构建 NapCat WebSocket 客户端
    let mut builder = NapLink::builder(&config.napcat.ws_url);
    // 如果配置了 Token 则设置认证信息
    if let Some(token) = &config.napcat.token {
        builder = builder.token(token);
    }
    // 启用自动重连（断线后自动恢复连接）
    builder = builder.reconnect_enabled(true);
    let client = builder.build()?;

    // 在连接前创建事件订阅（避免丢失早期事件）
    let mut sub = client.subscribe();

    // 创建图片注册表数据库（SQLite 持久化，存储在 workspace/data/ 下）
    let image_db_path = std::path::PathBuf::from(&config.storage.workspace_dir)
        .join("data")
        .join("images.db");
    let image_store = ImageStore::open(&image_db_path)?;

    // 创建消息索引数据库（SQLite 持久化，存储 message_id → 文件位置映射）
    let msg_db_path = std::path::PathBuf::from(&config.storage.workspace_dir)
        .join("data")
        .join("messages.db");
    let message_index = MessageIndex::open(&msg_db_path)?;

    // 消息存储根目录（供 GetMessage 工具读取 JSONL 原始内容）
    let messages_dir = std::path::PathBuf::from(&config.storage.workspace_dir).join("messages");

    // 创建工具执行上下文（API + 白名单 + Skill 注册表 + 图片注册表 + 消息索引）
    let tool_ctx = ToolContext::new(
        client.api().clone(),
        whitelist,
        skill_registry,
        image_store,
        message_index,
        messages_dir,
    );
    let tool_ctx = Arc::new(tool_ctx);

    // ===== 5. AI Agent 状态 + 控制通道 =====

    // 创建 AI Agent 状态（默认启动，active = true）
    let agent_state = Arc::new(agent_state::AgentState::new(true));
    // 创建关闭信号广播通道（容量 1，仅需发送一次关闭信号）
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);

    // 创建 CLI chat 指令通道（发送端给 ControlServer，接收端给 Agent）
    let (cmd_tx, cmd_rx) =
        tokio::sync::mpsc::unbounded_channel::<agent::PendingCommand>();

    // 构建控制服务端上下文（持有 Agent 状态、关闭信号发送端、CLI 指令转发通道）
    let ctrl_server = Arc::new(control_server::ControlServer {
        agent_state: agent_state.clone(),
        shutdown_tx: shutdown_tx.clone(),
        cmd_tx,
    });
    // 在后台 tokio 任务中启动控制通道服务端
    // 监听 127.0.0.1:{port}，接收来自 CLI 的控制指令
    tokio::spawn(control_server::start_control_server(
        config.control.port,
        ctrl_server,
    ));

    // ===== 5.5 群身份状态 =====

    // 创建群身份状态（线程安全共享：Agent 读取，事件监听写入，定时同步写入）
    let group_role_state = Arc::new(group_role::GroupRoleState::new());

    // ===== 6. 主人账号配置 =====

    // master_qq 的值在收到第一条消息时通过 self_id 解析（如果是 "self" 模式）
    // 非 "self" 模式则在此处直接解析
    let master_is_self = config.identity.is_master_self();
    // 用 AtomicI64 存储解析后的主人 QQ 号
    // 初始值 0 表示尚未解析（"self" 模式需要等到收到消息才能获取 self_id）
    let master_qq = Arc::new(AtomicI64::new(
        if master_is_self {
            0 // 等待第一条消息的 self_id 来填充
        } else {
            config.identity.resolve_master_qq(0).unwrap_or(0)
        },
    ));

    tracing::info!(
        "主人账号配置: {} ({})",
        config.identity.master,
        if master_is_self { "self 模式" } else { "指定 QQ 号" }
    );

    // ===== 6.5 Bot 自身 QQ 号 =====

    // bot_qq 始终从第一条收到消息的 self_id 解析，用于判断自发消息
    // 与 master_qq 独立：master 可能是不同的 QQ 号
    let bot_qq = Arc::new(AtomicI64::new(0));

    // ===== 6.6 Agent 消息通道与启动 =====

    // 创建无界通道，用于事件监听器向 Agent 发送消息事件
    let (agent_tx, agent_rx) = tokio::sync::mpsc::unbounded_channel::<agent::MessageEvent>();

    // 创建并启动 Agent 运行器
    if config.primary_model().is_configured() {
        match agent::AgentRunner::new(
            Arc::new(config.clone()),
            tool_ctx.clone(),
            agent_state.clone(),
            agent_rx,
            cmd_rx,
            group_role_state.clone(),
        )
        .await
        {
            Ok(runner) => {
                tokio::spawn(runner.run());
                tracing::info!("Agent 循环已启动（模型: {}）", config.primary_model().model);
            }
            Err(e) => {
                tracing::error!("Agent 初始化失败: {}（消息存储将继续，但不会自动回复）", e);
                // agent_rx 被丢弃，agent_tx.send() 后续会返回 Err 但无害
            }
        }
    } else {
        tracing::warn!("LLM 未配置（base_url 或 api_key 为空），Agent 不会启动。消息存储正常工作。");
        // agent_rx 和 cmd_rx 被丢弃，对应的 send() 后续返回 Err 但无害
        drop(agent_rx);
        drop(cmd_rx);
    }

    // ===== 7. 消息事件监听任务 =====

    // 克隆 Arc 引用，传入 spawn 的异步任务
    let storage_clone = storage.clone();
    let tool_ctx_clone = tool_ctx.clone();
    let master_qq_clone = master_qq.clone();
    let bot_qq_clone = bot_qq.clone();
    let agent_tx_clone = agent_tx.clone();
    let group_role_clone = group_role_state.clone();
    // 在后台 tokio 任务中运行消息事件监听循环
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
                    // 首次收到消息时解析 bot 自身 QQ 号和 master QQ
                    if bot_qq_clone.load(Ordering::Relaxed) == 0 {
                        bot_qq_clone.store(msg.self_id, Ordering::Relaxed);
                        tracing::info!("Bot QQ 已解析为 {}", msg.self_id);
                    }
                    if master_is_self && master_qq_clone.load(Ordering::Relaxed) == 0 {
                        master_qq_clone.store(msg.self_id, Ordering::Relaxed);
                        tracing::info!("master=self 模式：主人 QQ = {}", msg.self_id);
                    }
                    // 白名单过滤：不在收白名单中的群消息直接丢弃
                    if !tool_ctx_clone.whitelist.can_recv_group(msg.group_id) {
                        continue;
                    }
                    handle_group_message(
                        &storage_clone,
                        &msg,
                        master_qq_clone.load(Ordering::Relaxed),
                        bot_qq_clone.load(Ordering::Relaxed),
                        master_is_self,
                        &tool_ctx_clone,
                        &agent_tx_clone,
                    );
                }
            } else if event_name.starts_with("message.private") {
                // ----- 私聊消息处理 -----
                if let Ok(msg) =
                    serde_json::from_value::<PrivateMessageEvent>(event.data.clone())
                {
                    // 首次收到消息时解析 bot 自身 QQ 号和 master QQ
                    if bot_qq_clone.load(Ordering::Relaxed) == 0 {
                        bot_qq_clone.store(msg.self_id, Ordering::Relaxed);
                        tracing::info!("Bot QQ 已解析为 {}", msg.self_id);
                    }
                    if master_is_self && master_qq_clone.load(Ordering::Relaxed) == 0 {
                        master_qq_clone.store(msg.self_id, Ordering::Relaxed);
                        tracing::info!("master=self 模式：主人 QQ = {}", msg.self_id);
                    }
                    // 白名单过滤：不在收白名单中的私聊消息直接丢弃
                    if !tool_ctx_clone.whitelist.can_recv_private(msg.user_id) {
                        continue;
                    }
                    handle_private_message(
                        &storage_clone,
                        &msg,
                        master_qq_clone.load(Ordering::Relaxed),
                        bot_qq_clone.load(Ordering::Relaxed),
                        master_is_self,
                        &tool_ctx_clone,
                        &agent_tx_clone,
                    );
                }
            } else if event_name.starts_with("message_sent.group") {
                // ----- Bot 自身发送的群消息（message_sent 事件） -----
                // NapCat 在 Bot 账号发送消息后推送此事件，用于记录 Agent 自发消息
                if let Ok(msg) =
                    serde_json::from_value::<GroupMessageEvent>(event.data.clone())
                {
                    // 首次收到消息时也可解析 bot QQ（兜底）
                    if bot_qq_clone.load(Ordering::Relaxed) == 0 {
                        bot_qq_clone.store(msg.self_id, Ordering::Relaxed);
                        tracing::info!("Bot QQ 已解析为 {} (via message_sent)", msg.self_id);
                    }
                    handle_self_sent_group_message(
                        &storage_clone,
                        &msg,
                        master_qq_clone.load(Ordering::Relaxed),
                        master_is_self,
                        &tool_ctx_clone,
                    );
                }
            } else if event_name.starts_with("message_sent.private") {
                // ----- Bot 自身发送的私聊消息（message_sent 事件） -----
                if let Ok(msg) =
                    serde_json::from_value::<PrivateMessageEvent>(event.data.clone())
                {
                    // 首次收到消息时也可解析 bot QQ（兜底）
                    if bot_qq_clone.load(Ordering::Relaxed) == 0 {
                        bot_qq_clone.store(msg.self_id, Ordering::Relaxed);
                        tracing::info!("Bot QQ 已解析为 {} (via message_sent)", msg.self_id);
                    }
                    handle_self_sent_private_message(
                        &storage_clone,
                        &msg,
                        master_qq_clone.load(Ordering::Relaxed),
                        master_is_self,
                        &tool_ctx_clone,
                    );
                }
            } else if event_name.starts_with("request.friend") {
                // ----- 好友请求事件（存储到日志 + 转发给 Agent） -----
                if let Ok(req) =
                    serde_json::from_value::<FriendRequestEvent>(event.data.clone())
                {
                    tracing::info!(
                        "收到好友请求: QQ={}, 验证信息={}, flag={}",
                        req.user_id,
                        req.comment,
                        req.flag
                    );
                    // 格式化为 Agent 可读的事件文本（包含 flag 以便 Agent 直接处理）
                    let formatted = format!(
                        "[好友请求] QQ: {} | 验证信息: \"{}\" | flag: {}",
                        req.user_id, req.comment, req.flag
                    );
                    // 发送给 Agent，触发处理流程
                    let _ = agent_tx_clone.send(agent::MessageEvent {
                        formatted,
                        source: Some(agent::MessageSource::FriendRequest {
                            user_id: req.user_id,
                        }),
                    });
                }
            } else if event_name.starts_with("request.group") {
                // ----- 群请求事件（存储到日志 + 转发给 Agent） -----
                if let Ok(req) =
                    serde_json::from_value::<GroupRequestEvent>(event.data.clone())
                {
                    tracing::info!(
                        "收到群请求: 群={}, QQ={}, 类型={}, 验证信息={}, flag={}",
                        req.group_id,
                        req.user_id,
                        req.sub_type,
                        req.comment,
                        req.flag
                    );
                    // 描述请求类型（加群 / 群邀请）
                    let req_type_desc = if req.sub_type == "invite" {
                        "群邀请"
                    } else {
                        "加群请求"
                    };
                    // 格式化为 Agent 可读的事件文本
                    let formatted = format!(
                        "[群请求] 类型: {} | 群号: {} | QQ: {} | 验证信息: \"{}\" | flag: {}",
                        req_type_desc, req.group_id, req.user_id, req.comment, req.flag
                    );
                    // 发送给 Agent，触发处理流程
                    let _ = agent_tx_clone.send(agent::MessageEvent {
                        formatted,
                        source: Some(agent::MessageSource::GroupRequest {
                            group_id: req.group_id,
                            user_id: req.user_id,
                        }),
                    });
                }
            } else if event_name.starts_with("notice.group_admin") {
                // ----- 群管理员变更事件（即时更新群身份状态） -----
                if let Ok(notice) = serde_json::from_value::<
                    napcat_link::types::event::notice::GroupAdminNotice,
                >(event.data.clone())
                {
                    let bot = bot_qq_clone.load(Ordering::Relaxed);
                    // 仅关注 Bot 自身的管理员状态变更
                    if bot != 0 && notice.user_id == bot {
                        let role = if notice.sub_type == "set" {
                            group_role::GroupRole::Admin
                        } else {
                            group_role::GroupRole::Member
                        };
                        group_role_clone.set_role(notice.group_id, role);
                        tracing::info!(
                            "群 {} 管理员状态变更: {} → {:?}",
                            notice.group_id,
                            notice.sub_type,
                            role
                        );
                    }
                }
            } else if event_name.starts_with("notice.friend_add") {
                // ----- 好友添加成功通知（转发给 Agent） -----
                if let Ok(notice) = serde_json::from_value::<
                    napcat_link::types::event::notice::FriendAddNotice,
                >(event.data.clone())
                {
                    tracing::info!("好友添加成功: QQ={}", notice.user_id);
                    // 格式化为 Agent 可读的通知文本
                    let formatted = format!(
                        "[好友添加成功] QQ: {} 已成为好友",
                        notice.user_id
                    );
                    // 发送给 Agent，Agent 可据此主动打招呼
                    let _ = agent_tx_clone.send(agent::MessageEvent {
                        formatted,
                        source: Some(agent::MessageSource::FriendAdded {
                            user_id: notice.user_id,
                        }),
                    });
                }
            }
        }
    });

    // ===== 7.5 群身份定时全量同步任务 =====

    {
        // 克隆共享状态到定时同步任务
        let sync_state = group_role_state.clone();
        let sync_api = tool_ctx.api.clone();
        let sync_bot_qq = bot_qq.clone();
        let sync_recv_groups: Vec<i64> = config.whitelist.recv_groups.clone();

        tokio::spawn(async move {
            // ===== 等待 bot_qq 就绪 =====
            // bot_qq 在收到第一条消息时从 self_id 解析
            // 轮询等待，每 1 秒检查一次，最多等 300 秒
            let mut waited = 0u32;
            loop {
                let qq = sync_bot_qq.load(Ordering::Relaxed);
                if qq != 0 {
                    tracing::info!("群角色同步: bot_qq={} 已就绪", qq);
                    break;
                }
                if waited >= 300 {
                    tracing::warn!("群角色同步: 等待 bot_qq 超时（300s），放弃首次同步");
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                waited += 1;
            }

            let bot = sync_bot_qq.load(Ordering::Relaxed);

            // ===== 首次全量同步 =====
            tracing::info!("群角色同步: 执行首次全量同步...");
            let roles = group_role::sync_group_roles(&sync_api, bot, &sync_recv_groups).await;
            sync_state.replace_all(roles);

            // ===== 定时循环：每 12 小时 + 随机 15~1800 秒延迟 =====
            loop {
                let base = std::time::Duration::from_secs(12 * 3600);
                let jitter_secs = rand::random_range(15u64..=1800);
                let jitter = std::time::Duration::from_secs(jitter_secs);
                tracing::debug!("群角色同步: 下次同步在 {}h + {}s 后", 12, jitter_secs);
                tokio::time::sleep(base + jitter).await;

                let bot = sync_bot_qq.load(Ordering::Relaxed);
                if bot == 0 {
                    tracing::warn!("群角色同步: bot_qq 为 0，跳过本次同步");
                    continue;
                }
                tracing::info!("群角色同步: 执行定时全量同步...");
                let roles =
                    group_role::sync_group_roles(&sync_api, bot, &sync_recv_groups).await;
                sync_state.replace_all(roles);
            }
        });
    }

    // ===== 8. 连接 NapCat 服务器 =====

    // 带重试的初始连接：如果 NapCat 尚未就绪，每隔一段时间重试
    // 避免因启动顺序导致 sbot 直接退出
    {
        let max_initial_retries = 30; // 最多重试 30 次（约 5 分钟）
        let retry_interval = std::time::Duration::from_secs(10);
        let mut attempt = 0;

        loop {
            attempt += 1;
            match client.connect().await {
                Ok(()) => {
                    tracing::info!("已连接到 NapCat: {}", config.napcat.ws_url);
                    break;
                }
                Err(e) => {
                    if attempt >= max_initial_retries {
                        tracing::error!(
                            "连接 NapCat 失败，已重试 {} 次，放弃: {}",
                            attempt, e
                        );
                        anyhow::bail!("无法连接到 NapCat: {}", e);
                    }
                    tracing::warn!(
                        "连接 NapCat 失败 (第 {}/{} 次)，{}秒后重试: {}",
                        attempt, max_initial_retries, retry_interval.as_secs(), e
                    );
                    tokio::time::sleep(retry_interval).await;
                }
            }
        }
    }

    // ===== 9. 等待关闭信号 =====

    // 使用 tokio::select! 同时监听两个关闭源：
    // 1. Ctrl+C 终端信号（本地终止）
    // 2. 远程 shutdown 指令（通过控制通道发送）
    // 任何一个先触发都会导致守护进程开始关闭流程
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("收到 Ctrl+C 信号");
        }
        _ = shutdown_rx.recv() => {
            tracing::info!("收到远程关闭指令");
        }
    }

    // 断开 NapCat WebSocket 连接，释放资源
    tracing::info!("正在断开连接...");
    client.disconnect();

    Ok(())
}

// ============================================================================
// spawn_daemon() — 后台模式启动
// ============================================================================

/// 以后台模式启动守护进程：spawn 一个 detached 子进程运行 `sbot start`（不带 -d）
/// 子进程的标准输入/输出/错误全部重定向到 null，与终端完全分离
///
/// # 参数
/// - `config_path`：配置文件路径，传递给子进程
///
/// # 平台特性
/// - Windows：使用 CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS 标志
fn spawn_daemon(config_path: &str) -> anyhow::Result<()> {
    // 获取当前可执行文件的绝对路径（即 sbot 自身）
    let exe = std::env::current_exe()?;
    // 构建子进程命令
    let mut cmd = std::process::Command::new(exe);
    // 传递 start 命令和配置文件路径（不带 -d，子进程以前台模式运行）
    cmd.args(["start", "-c", config_path]);

    // Windows 平台：设置进程创建标志实现 detach
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP (0x00000200) | DETACHED_PROCESS (0x00000008)
        // 组合这两个标志使子进程完全脱离当前控制台
        cmd.creation_flags(0x00000200 | 0x00000008);
    }

    // 重定向标准流到 null（后台进程不需要终端 I/O）
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // 启动子进程
    let child = cmd.spawn()?;
    // 输出子进程 PID，便于用户手动管理
    println!("sbot 已在后台启动 (PID: {})", child.id());
    Ok(())
}

// ============================================================================
// 消息处理函数
// ============================================================================

/// 处理群聊消息：统一存储 → 主人消息副本 → Agent 自发消息副本 → 通知 Agent
fn handle_group_message(
    storage: &StorageManager,
    msg: &GroupMessageEvent,
    master_qq: i64,
    bot_qq: i64,
    master_is_self: bool,
    tool_ctx: &ToolContext,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<agent::MessageEvent>,
) {
    let group_id = msg.group_id;

    // ===== 统一存储：JSONL + MD =====
    if let Ok(raw_json) = serde_json::to_value(msg) {
        match storage.store_raw_jsonl(&raw_json) {
            Ok(location) => {
                // 写入消息索引：message_id → (file_path, line_number) + 元数据
                tool_ctx.message_index.index_message(
                    msg.message_id,
                    &location.file_path,
                    location.line_number,
                    msg.time,
                    msg.user_id,
                    Some(msg.group_id),
                    "group",
                );
            }
            Err(e) => {
                tracing::error!("存储消息 JSONL 失败 (群={}): {}", group_id, e);
            }
        }
    }

    // 注册消息中的图片 URL（供 ReadImage 工具安全查找）
    let image_urls: Vec<String> = msg
        .message
        .iter()
        .filter_map(|seg| {
            if let MessageSegment::Image { url, .. } = seg {
                url.clone()
            } else {
                None
            }
        })
        .collect();
    if !image_urls.is_empty() {
        tool_ctx.register_message_images(msg.message_id, &image_urls);
    }

    // 构建 MD 记录
    let time_str = Local
        .timestamp_opt(msg.time, 0)
        .single()
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "??:??:??".to_string());

    let (state, content) = if needs_wrapping(&msg.message, &msg.raw_message) {
        (MessageState::Wrapped, segments_to_message_xml(&msg.message))
    } else {
        (MessageState::Raw, msg.raw_message.clone())
    };

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
        remark: None,
        identity: format!("GroupMember ({group_id})"),
        state,
        content,
    };

    if let Err(e) = storage.store_markdown(&record) {
        tracing::error!("存储消息 MD 失败 (群={}): {}", group_id, e);
    }

    // ===== 主人消息副本 =====
    // 确认为主人发送：master=self 模式下排除 Agent 发送的消息
    if master_qq != 0 && msg.user_id == master_qq {
        let is_agent_msg = master_is_self && tool_ctx.is_agent_sent(msg.message_id);
        if !is_agent_msg {
            if let Err(e) = storage.store_master(&record) {
                tracing::error!("存储主人风格消息失败 (群={}): {}", group_id, e);
            }
        }
    }

    // ===== 通知 Agent =====
    // 将格式化的消息记录发送给 Agent 处理
    // Agent 自己发送的消息不需要通知（避免自我循环）
    // 注意：Agent 自发消息的存储（messages/self/）已移交给 message_sent.* 事件处理器
    let is_self_sent = bot_qq != 0 && msg.user_id == bot_qq;
    if !is_self_sent {
        let formatted = format!(
            "[群消息] 群:{} | 时间:{} | MsgID:{} | QQ:{} | 昵称:{}{} | 身份:{}\n{}",
            group_id,
            record.time,
            record.message_id,
            record.qq,
            record.nick,
            record
                .group_nick
                .as_ref()
                .map(|n| format!("({})", n))
                .unwrap_or_default(),
            record.identity,
            record.content,
        );
        let _ = agent_tx.send(agent::MessageEvent {
            formatted,
            // 携带群消息来源，Agent 首次 LLM 请求成功后自动标记该群已读
            source: Some(agent::MessageSource::Group { group_id }),
        });
    }
}

/// 处理私聊消息：统一存储 → 主人消息副本 → Agent 自发消息副本 → 通知 Agent
fn handle_private_message(
    storage: &StorageManager,
    msg: &PrivateMessageEvent,
    master_qq: i64,
    bot_qq: i64,
    master_is_self: bool,
    tool_ctx: &ToolContext,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<agent::MessageEvent>,
) {
    let user_id = msg.user_id;

    // ===== 统一存储：JSONL + MD =====
    if let Ok(raw_json) = serde_json::to_value(msg) {
        match storage.store_raw_jsonl(&raw_json) {
            Ok(location) => {
                // 写入消息索引：message_id → (file_path, line_number) + 元数据
                // 私聊消息 group_id 为 None
                tool_ctx.message_index.index_message(
                    msg.message_id,
                    &location.file_path,
                    location.line_number,
                    msg.time,
                    msg.user_id,
                    None,
                    "private",
                );
            }
            Err(e) => {
                tracing::error!("存储消息 JSONL 失败 (QQ={}): {}", user_id, e);
            }
        }
    }

    // 注册消息中的图片 URL（供 ReadImage 工具安全查找）
    let image_urls: Vec<String> = msg
        .message
        .iter()
        .filter_map(|seg| {
            if let MessageSegment::Image { url, .. } = seg {
                url.clone()
            } else {
                None
            }
        })
        .collect();
    if !image_urls.is_empty() {
        tool_ctx.register_message_images(msg.message_id, &image_urls);
    }

    let time_str = Local
        .timestamp_opt(msg.time, 0)
        .single()
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "??:??:??".to_string());

    let (state, content) = if needs_wrapping(&msg.message, &msg.raw_message) {
        (MessageState::Wrapped, segments_to_message_xml(&msg.message))
    } else {
        (MessageState::Raw, msg.raw_message.clone())
    };

    let nick = msg
        .sender
        .nickname
        .clone()
        .unwrap_or_else(|| msg.user_id.to_string());

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
        state,
        content,
    };

    if let Err(e) = storage.store_markdown(&record) {
        tracing::error!("存储消息 MD 失败 (QQ={}): {}", user_id, e);
    }

    // ===== 主人消息副本 =====
    if master_qq != 0 && msg.user_id == master_qq {
        let is_agent_msg = master_is_self && tool_ctx.is_agent_sent(msg.message_id);
        if !is_agent_msg {
            if let Err(e) = storage.store_master(&record) {
                tracing::error!("存储主人风格消息失败 (QQ={}): {}", user_id, e);
            }
        }
    }

    // ===== 通知 Agent =====
    // 注意：Agent 自发消息的存储（messages/self/）已移交给 message_sent.* 事件处理器
    let is_self_sent = bot_qq != 0 && msg.user_id == bot_qq;
    if !is_self_sent {
        let formatted = format!(
            "[私聊消息] 时间:{} | MsgID:{} | QQ:{} | 昵称:{} | 身份:{}\n{}",
            record.time,
            record.message_id,
            record.qq,
            record.nick,
            record.identity,
            record.content,
        );
        let _ = agent_tx.send(agent::MessageEvent {
            formatted,
            // 携带私聊来源，Agent 首次 LLM 请求成功后自动标记该私聊已读
            source: Some(agent::MessageSource::Private { user_id: msg.user_id }),
        });
    }
}

// ============================================================================
// Bot 自身发送的消息处理函数（message_sent 事件）
// ============================================================================

/// 处理 Bot 自身发送的群消息（post_type = "message_sent", message_type = "group"）
///
/// 与 handle_group_message 的关键区别：
/// - 通过 message_index 去重，避免与 echo 事件重复存储 JSONL/MD
/// - 负责 messages/self/ 的写入（核心修复：此职责从 message.* 处理器移交至此）
/// - 不通知 Agent（避免自我循环）
fn handle_self_sent_group_message(
    storage: &StorageManager,
    msg: &GroupMessageEvent,
    master_qq: i64,
    master_is_self: bool,
    tool_ctx: &ToolContext,
) {
    let group_id = msg.group_id;

    // 去重检查：如果此消息已被 echo 的 message.* 事件处理过，跳过基础存储
    // 使用 message_index.lookup() 作为"已处理"标志
    let already_stored = tool_ctx.message_index.lookup(msg.message_id).is_some();

    if !already_stored {
        // ===== 基础存储：JSONL + 消息索引 =====
        if let Ok(raw_json) = serde_json::to_value(msg) {
            match storage.store_raw_jsonl(&raw_json) {
                Ok(location) => {
                    // 写入消息索引
                    tool_ctx.message_index.index_message(
                        msg.message_id,
                        &location.file_path,
                        location.line_number,
                        msg.time,
                        msg.user_id,
                        Some(msg.group_id),
                        "group",
                    );
                }
                Err(e) => {
                    tracing::error!("存储自发群消息 JSONL 失败 (群={}): {}", group_id, e);
                }
            }
        }

        // 注册消息中的图片 URL（供 ReadImage 工具安全查找）
        let image_urls: Vec<String> = msg
            .message
            .iter()
            .filter_map(|seg| {
                if let MessageSegment::Image { url, .. } = seg {
                    url.clone()
                } else {
                    None
                }
            })
            .collect();
        if !image_urls.is_empty() {
            tool_ctx.register_message_images(msg.message_id, &image_urls);
        }
    }

    // 构建 MD 记录（store_self / store_master / store_markdown 均需要）
    let time_str = Local
        .timestamp_opt(msg.time, 0)
        .single()
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "??:??:??".to_string());

    let (state, content) = if needs_wrapping(&msg.message, &msg.raw_message) {
        (MessageState::Wrapped, segments_to_message_xml(&msg.message))
    } else {
        (MessageState::Raw, msg.raw_message.clone())
    };

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
        remark: None,
        identity: format!("GroupMember ({group_id})"),
        state,
        content,
    };

    // 统一 MD 存储（去重：仅在 echo 未处理时写入）
    if !already_stored {
        if let Err(e) = storage.store_markdown(&record) {
            tracing::error!("存储自发群消息 MD 失败 (群={}): {}", group_id, e);
        }
    }

    // ===== Agent 自发消息副本（核心修复） =====
    // 由 record_agent_sent() 追踪的消息 ID 确认为 Agent 发送
    if tool_ctx.is_agent_sent(msg.message_id) {
        if let Err(e) = storage.store_self(&record) {
            tracing::error!("存储 Agent 自发群消息到 self/ 失败 (群={}): {}", group_id, e);
        }
    }

    // ===== 主人消息副本（master=self 模式） =====
    // master=self 时，Bot 账号也是主人账号，非 Agent 发送的消息属于主人手动发送
    if master_is_self && master_qq != 0 && msg.user_id == master_qq
        && !tool_ctx.is_agent_sent(msg.message_id)
    {
        if let Err(e) = storage.store_master(&record) {
            tracing::error!("存储主人自发群消息到 master/ 失败 (群={}): {}", group_id, e);
        }
    }

    // 不通知 Agent：Bot 自身消息不需要触发 Agent 处理循环
}

/// 处理 Bot 自身发送的私聊消息（post_type = "message_sent", message_type = "private"）
///
/// 与 handle_self_sent_group_message 对称，处理私聊版本。
/// 关键区别：无 group_id、无 group_nick、identity 根据 sub_type 判断
fn handle_self_sent_private_message(
    storage: &StorageManager,
    msg: &PrivateMessageEvent,
    master_qq: i64,
    master_is_self: bool,
    tool_ctx: &ToolContext,
) {
    let user_id = msg.user_id;

    // 去重检查：如果此消息已被 echo 的 message.* 事件处理过，跳过基础存储
    let already_stored = tool_ctx.message_index.lookup(msg.message_id).is_some();

    if !already_stored {
        // ===== 基础存储：JSONL + 消息索引 =====
        if let Ok(raw_json) = serde_json::to_value(msg) {
            match storage.store_raw_jsonl(&raw_json) {
                Ok(location) => {
                    // 写入消息索引（私聊消息 group_id 为 None）
                    tool_ctx.message_index.index_message(
                        msg.message_id,
                        &location.file_path,
                        location.line_number,
                        msg.time,
                        msg.user_id,
                        None,
                        "private",
                    );
                }
                Err(e) => {
                    tracing::error!("存储自发私聊消息 JSONL 失败 (QQ={}): {}", user_id, e);
                }
            }
        }

        // 注册消息中的图片 URL
        let image_urls: Vec<String> = msg
            .message
            .iter()
            .filter_map(|seg| {
                if let MessageSegment::Image { url, .. } = seg {
                    url.clone()
                } else {
                    None
                }
            })
            .collect();
        if !image_urls.is_empty() {
            tool_ctx.register_message_images(msg.message_id, &image_urls);
        }
    }

    // 构建 MD 记录
    let time_str = Local
        .timestamp_opt(msg.time, 0)
        .single()
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "??:??:??".to_string());

    let (state, content) = if needs_wrapping(&msg.message, &msg.raw_message) {
        (MessageState::Wrapped, segments_to_message_xml(&msg.message))
    } else {
        (MessageState::Raw, msg.raw_message.clone())
    };

    let nick = msg
        .sender
        .nickname
        .clone()
        .unwrap_or_else(|| msg.user_id.to_string());

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
        state,
        content,
    };

    // 统一 MD 存储（去重）
    if !already_stored {
        if let Err(e) = storage.store_markdown(&record) {
            tracing::error!("存储自发私聊消息 MD 失败 (QQ={}): {}", user_id, e);
        }
    }

    // ===== Agent 自发消息副本（核心修复） =====
    if tool_ctx.is_agent_sent(msg.message_id) {
        if let Err(e) = storage.store_self(&record) {
            tracing::error!("存储 Agent 自发私聊消息到 self/ 失败 (QQ={}): {}", user_id, e);
        }
    }

    // ===== 主人消息副本（master=self 模式） =====
    if master_is_self && master_qq != 0 && msg.user_id == master_qq
        && !tool_ctx.is_agent_sent(msg.message_id)
    {
        if let Err(e) = storage.store_master(&record) {
            tracing::error!("存储主人自发私聊消息到 master/ 失败 (QQ={}): {}", user_id, e);
        }
    }

    // 不通知 Agent
}
