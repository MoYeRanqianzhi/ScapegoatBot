// src/main.rs
// ScapegoatBot 主入口文件
// 职责：CLI 解析与路由 → 守护进程模式 / 控制指令转发
// 架构：main() 解析命令行 → run_daemon() 执行守护进程逻辑 → send_control() 发送控制指令

// ============================================================================
// 模块声明
// ============================================================================

mod agent_state;
mod cli;
mod control;
mod control_server;
mod config;
mod message_format;
mod prompt;
mod providers;
mod skills;
mod storage;
mod tools;
mod whitelist;

// ============================================================================
// 导入
// ============================================================================

use config::AppConfig;
use message_format::{needs_wrapping, segments_to_message_xml};
use skills::SkillRegistry;
use storage::markdown::{MessageRecord, MessageState};
use storage::{SessionType, StorageManager};
use tools::ToolContext;
use whitelist::Whitelist;

use chrono::{Local, TimeZone};
use clap::Parser;
use napcat_link::{GroupMessageEvent, NapLink, PrivateMessageEvent};
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

    // 创建工具执行上下文（API + 白名单 + Skill 注册表）
    let tool_ctx = ToolContext::new(client.api().clone(), whitelist, skill_registry);
    let tool_ctx = Arc::new(tool_ctx);

    // ===== 5. AI Agent 状态 + 控制通道 =====

    // 创建 AI Agent 状态（默认启动，active = true）
    let agent_state = Arc::new(agent_state::AgentState::new(true));
    // 创建关闭信号广播通道（容量 1，仅需发送一次关闭信号）
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);

    // 构建控制服务端上下文（持有 Agent 状态和关闭信号发送端）
    let ctrl_server = Arc::new(control_server::ControlServer {
        agent_state: agent_state.clone(),
        shutdown_tx: shutdown_tx.clone(),
    });
    // 在后台 tokio 任务中启动控制通道服务端
    // 监听 127.0.0.1:{port}，接收来自 CLI 的控制指令
    tokio::spawn(control_server::start_control_server(
        config.control.port,
        ctrl_server,
    ));

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

    // ===== 7. 消息事件监听任务 =====

    // 克隆 Arc 引用，传入 spawn 的异步任务
    let storage_clone = storage.clone();
    let tool_ctx_clone = tool_ctx.clone();
    let master_qq_clone = master_qq.clone();
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
                    // 首次收到消息时解析 self_id（用于 master=self 模式）
                    if master_is_self && master_qq_clone.load(Ordering::Relaxed) == 0 {
                        master_qq_clone.store(msg.self_id, Ordering::Relaxed);
                        tracing::info!("master=self 模式：已解析主人 QQ 为 {}", msg.self_id);
                    }
                    handle_group_message(
                        &storage_clone,
                        &msg,
                        master_qq_clone.load(Ordering::Relaxed),
                        master_is_self,
                        &tool_ctx_clone,
                    );
                }
            } else if event_name.starts_with("message.private") {
                // ----- 私聊消息处理 -----
                if let Ok(msg) =
                    serde_json::from_value::<PrivateMessageEvent>(event.data.clone())
                {
                    // 首次收到消息时解析 self_id（用于 master=self 模式）
                    if master_is_self && master_qq_clone.load(Ordering::Relaxed) == 0 {
                        master_qq_clone.store(msg.self_id, Ordering::Relaxed);
                        tracing::info!("master=self 模式：已解析主人 QQ 为 {}", msg.self_id);
                    }
                    handle_private_message(
                        &storage_clone,
                        &msg,
                        master_qq_clone.load(Ordering::Relaxed),
                        master_is_self,
                        &tool_ctx_clone,
                    );
                }
            } else if event_name.starts_with("request.friend") {
                // ----- 好友请求事件（记录到日志，不主动存储） -----
                if let Ok(req) =
                    serde_json::from_value::<FriendRequestEvent>(event.data.clone())
                {
                    tracing::info!(
                        "收到好友请求: QQ={}, 验证信息={}",
                        req.user_id,
                        req.comment
                    );
                }
            } else if event_name.starts_with("request.group") {
                // ----- 群请求事件（记录到日志） -----
                if let Ok(req) =
                    serde_json::from_value::<GroupRequestEvent>(event.data.clone())
                {
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

    // ===== 8. 连接 NapCat 服务器 =====

    // 发起 WebSocket 连接（阻塞直到握手成功）
    client.connect().await?;
    tracing::info!("已连接到 NapCat: {}", config.napcat.ws_url);

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

/// 处理群聊消息：白名单检查 → 存储 JSONL → 存储 MD → 主人消息复制
fn handle_group_message(
    storage: &StorageManager,
    msg: &GroupMessageEvent,
    master_qq: i64,
    master_is_self: bool,
    tool_ctx: &ToolContext,
) {
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

    // 根据消息内容判断存储状态和格式：
    // - Raw：纯文本消息，直接存储 raw_message 原文
    // - Wrapped：包含非文本段（@、表情、图片等）或存在格式干扰，用 <message> + CQ XML 包裹
    let (state, content) = if needs_wrapping(&msg.message, &msg.raw_message) {
        (MessageState::Wrapped, segments_to_message_xml(&msg.message))
    } else {
        (MessageState::Raw, msg.raw_message.clone())
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
        state,
        content,
    };

    if let Err(e) = storage.store_markdown(SessionType::Group, group_id, &record) {
        tracing::error!("存储群消息 MD 失败 (群={}): {}", group_id, e);
    }

    // ===== 主人消息复制 =====
    // 如果发送者是主人，将消息复制到 master_style 目录用于语言风格学习
    if master_qq != 0 && msg.user_id == master_qq {
        // master=self 模式：过滤 agent 发送的消息
        let is_agent_msg = master_is_self && tool_ctx.is_agent_sent(msg.message_id);
        if !is_agent_msg {
            if let Err(e) = storage.store_master_style(&record) {
                tracing::error!("存储主人风格消息失败 (群={}): {}", group_id, e);
            }
        }
    }
}

/// 处理私聊消息：白名单检查 → 存储 JSONL → 存储 MD → 主人消息复制
fn handle_private_message(
    storage: &StorageManager,
    msg: &PrivateMessageEvent,
    master_qq: i64,
    master_is_self: bool,
    tool_ctx: &ToolContext,
) {
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

    // 根据消息内容判断存储状态和格式（逻辑同群消息）
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
        state,
        content,
    };

    if let Err(e) = storage.store_markdown(SessionType::Private, user_id, &record) {
        tracing::error!("存储私聊消息 MD 失败 (QQ={}): {}", user_id, e);
    }

    // ===== 主人消息复制 =====
    // 如果发送者是主人，将消息复制到 master_style 目录用于语言风格学习
    if master_qq != 0 && msg.user_id == master_qq {
        // master=self 模式：过滤 agent 发送的消息
        let is_agent_msg = master_is_self && tool_ctx.is_agent_sent(msg.message_id);
        if !is_agent_msg {
            if let Err(e) = storage.store_master_style(&record) {
                tracing::error!("存储主人风格消息失败 (QQ={}): {}", user_id, e);
            }
        }
    }
}
