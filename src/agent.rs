// src/agent.rs
// Agent 核心循环模块
// 接收消息事件 → 批量去抖 → 构建上下文 → 调用 LLM → 执行工具调用 → 发送消息
//
// 架构：
// - 事件监听器通过 mpsc 通道将消息事件发送给 Agent
// - Agent 收到第一条消息后启动去抖窗口，批量收集消息
// - 批量消息格式化为一条 user 消息，追加到对话历史
// - 进入 agentic 循环：压缩 → 构建请求 → 调用 LLM → 分发工具调用 → 重复直到 end_turn
// - 工具调用结果追加到对话历史，作为 tool 消息回传给 LLM

use crate::agent_state::AgentState;
use crate::compact::{self, CompactionState};
use crate::config::AppConfig;
use crate::prompt::{self, AgentsMd, PromptConfig};
use crate::providers::traits::{ChatMessage, ChatRequest, Provider, ToolSpec};
use crate::storage::session::SessionManager;
use crate::swarm::dispatch::{ToolDispatcher, ToolResult};
use crate::swarm::SwarmManager;
use crate::tools::ToolContext;
use crate::tools::workspace::{WorkspaceToolSession, WorkspaceTools};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;

// ============================================================================
// 消息事件
// ============================================================================

/// 消息来源类型
/// 用于 Agent 处理完消息后自动调用对应的已读标记 API
#[derive(Debug, Clone)]
pub enum MessageSource {
    /// 群消息来源
    Group {
        /// 群号
        group_id: i64,
    },
    /// 私聊消息来源
    Private {
        /// 发送者 QQ 号
        user_id: i64,
    },
    /// 好友请求事件来源
    FriendRequest {
        /// 请求者 QQ 号
        user_id: i64,
    },
    /// 群请求事件来源（加群 / 群邀请）
    GroupRequest {
        /// 群号
        group_id: i64,
        /// 请求者 QQ 号
        user_id: i64,
    },
    /// 好友添加成功通知来源
    FriendAdded {
        /// 新好友 QQ 号
        user_id: i64,
    },
}

/// 传递给 Agent 的消息事件通知
/// 由事件监听器在存储消息后发送
#[derive(Debug, Clone)]
pub struct MessageEvent {
    /// 格式化后的消息文本（与存储到 MD 文件的内容一致）
    pub formatted: String,
    /// 消息来源（群/私聊），Agent 处理完后据此调用 mark_as_read
    /// CLI 指令转化的 MessageEvent 此字段为 None
    pub source: Option<MessageSource>,
}

// ============================================================================
// CLI 主人指令
// ============================================================================

/// 来自 CLI `sbot chat` 的待处理主人指令
/// 通过独立通道传递，与 QQ 消息通道分离
/// Agent 在空闲时直接触发新轮次，在 agentic 循环中于工具调用之间注入
#[derive(Debug, Clone)]
pub struct PendingCommand {
    /// 自然语言指令内容
    pub message: String,
    /// 本轮限定群白名单（空 = 不限定，使用全局）
    pub group_filter: Vec<i64>,
    /// 本轮限定私聊白名单（空 = 不限定，使用全局）
    pub private_filter: Vec<i64>,
}

// ============================================================================
// Agent 运行器
// ============================================================================

/// Agent 运行器
/// 持有 LLM 提供商、对话历史、工具上下文等全部运行时状态
/// 通过 `run()` 进入主循环，处理消息事件直到通道关闭
pub struct AgentRunner {
    // ----- LLM 相关 -----
    /// LLM 提供商（动态分发：OpenAI 或 Anthropic）
    provider: Box<dyn Provider>,
    /// 使用的模型名称
    model: String,
    /// 系统指令角色名称（"system" 或 "developer"）
    system_role_name: String,
    /// 推理努力程度（可选）
    reasoning_effort: Option<String>,
    /// 最大工具调用步数
    max_steps: u32,

    // ----- 配置 -----
    /// 全局应用配置
    config: Arc<AppConfig>,

    // ----- 上下文 -----
    /// 工具执行上下文（API + 白名单 + Skill）
    tool_ctx: Arc<ToolContext>,
    /// Agent 运行状态（活跃/暂停）
    agent_state: Arc<AgentState>,
    /// 群身份状态（管理员/群主），用于动态更新系统 prompt
    group_role_state: Arc<crate::group_role::GroupRoleState>,

    // ----- 工具分发器 -----
    /// 工具分发器（持有 workspace_tools、workspace_session、mcp_manager 等）
    /// 从 AgentRunner 提取，使工具分发逻辑可在多个 Agent 之间复用
    dispatcher: ToolDispatcher,

    // ----- 会话 -----
    /// 会话管理器（JSONL 持久化 + 内存缓存 + SQLite 索引）
    /// 替代之前的 compaction_state + conversation 字段
    session: SessionManager,
    /// 系统提示词（启动时构建，每轮刷新时间）
    system_prompt: String,

    // ----- 通道 -----
    /// 接收 QQ 消息事件的通道
    rx: mpsc::UnboundedReceiver<MessageEvent>,
    /// 接收 CLI chat 指令的通道（独立于 QQ 消息通道）
    /// 空闲时 select 两个通道；agentic 循环中用 try_recv 轮询注入
    cmd_rx: mpsc::UnboundedReceiver<PendingCommand>,

    // ----- 工具定义 -----
    /// LLM 可调用的工具规范列表
    tool_specs: Vec<ToolSpec>,

    // ----- 蜂群子代理 -----
    /// 蜂群管理器（协调子代理的生命周期）
    /// 通过 Arc 在主代理和子代理之间共享
    swarm: Arc<SwarmManager>,
    /// 接收子代理完成通知的通道
    swarm_rx: mpsc::UnboundedReceiver<crate::swarm::types::SwarmNotification>,
}

impl AgentRunner {
    /// 创建 Agent 运行器
    ///
    /// # 参数
    /// - `config`: 全局应用配置
    /// - `tool_ctx`: 工具执行上下文
    /// - `agent_state`: Agent 运行状态
    /// - `rx`: QQ 消息事件接收通道
    /// - `cmd_rx`: CLI chat 指令接收通道
    /// - `group_role_state`: 群身份状态（管理员/群主信息）
    pub async fn new(
        config: Arc<AppConfig>,
        tool_ctx: Arc<ToolContext>,
        agent_state: Arc<AgentState>,
        rx: mpsc::UnboundedReceiver<MessageEvent>,
        cmd_rx: mpsc::UnboundedReceiver<PendingCommand>,
        group_role_state: Arc<crate::group_role::GroupRoleState>,
    ) -> anyhow::Result<Self> {
        // 获取主代理模型配置
        let pm = config.primary_model();

        // 使用统一的 Provider 工厂创建 LLM 提供商
        let provider = crate::providers::create_provider(
            &pm.provider,
            &pm.base_url,
            &pm.api_key,
            pm.max_tokens,
            pm.timeout_secs,
            &pm.wire_api,
        );

        // 同步加载 AGENTS.md
        let agents_md_path =
            std::path::PathBuf::from(&config.storage.workspace_dir).join("AGENTS.md");
        let agents_md = match std::fs::read_to_string(&agents_md_path) {
            Ok(content) => AgentsMd {
                path: agents_md_path,
                content,
                found: true,
            },
            Err(_) => AgentsMd {
                path: agents_md_path,
                content: String::new(),
                found: false,
            },
        };

        // 创建工作区文件工具（cap-std 沙箱）
        let workspace_dir = Path::new(&config.storage.workspace_dir);
        let workspace_tools = WorkspaceTools::new(workspace_dir)?;

        // 初始化会话管理器（JSONL 持久化 + SQLite 索引）
        let session = SessionManager::open(workspace_dir)?;

        // 获取 Skill 列表用于系统提示
        let skill_items = tool_ctx.skills.list();

        // 构建备选模型列表段落（用于系统提示中展示 SpawnAgent 可选模型）
        let models_section = prompt::build_models_section(&config);

        // 构建系统提示（含前一个会话引用）
        let prompt_config = PromptConfig {
            workspace_dir,
            model_name: &pm.model,
            agents_md: &agents_md,
            extra_system_prompt: None,
            master_display: config.identity.master_display(),
            skills: &skill_items,
            send_groups: &config.whitelist.send_groups,
            send_privates: &config.whitelist.send_privates,
            recv_groups: &config.whitelist.recv_groups,
            recv_privates: &config.whitelist.recv_privates,
            prev_session_path: session.prev_session_path(),
            ai_boot: true,
            unprocessed_ranges: vec![],
            group_roles_section: "",  // 初始化为空，启动后由定时同步填充
            models_section: &models_section,
        };
        let system_prompt = prompt::build_system_prompt(&prompt_config);

        // 构建工具规范
        let mut tool_specs = build_tool_specs();

        // 条件注册 Fetch 工具（需要在配置中显式启用）
        if config.fetch.enabled {
            tool_specs.push(ToolSpec {
                name: "Fetch".to_string(),
                description: "获取网页内容并转为 Markdown。支持 HTML 自动转换。\
                              仅允许 http/https。有 SSRF 防护和域名过滤。"
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "要获取的网页 URL（必须以 http:// 或 https:// 开头）"
                        }
                    },
                    "required": ["url"]
                }),
            });
        }

        // ===== MCP 服务器连接 =====
        // 如果配置了 MCP 服务器，尝试连接并获取外部工具列表
        // 连接失败是非致命的，Agent 可以在无 MCP 的情况下正常工作
        let mcp_manager = if config.mcp.is_empty() {
            // 未配置任何 MCP 服务器，跳过
            None
        } else {
            match crate::mcp::McpConnectionManager::connect_all(&config.mcp).await {
                Ok(mgr) => {
                    // 从已连接的 MCP 服务器收集工具规范
                    let mcp_tools: Vec<ToolSpec> = mgr.list_all_tool_specs();
                    // 统计已启用的服务器数量
                    let server_count = config.mcp.iter()
                        .filter(|(_, c)| c.enabled)
                        .count();
                    tracing::info!(
                        "MCP 已连接 — {} 个服务器, {} 个工具",
                        server_count,
                        mcp_tools.len()
                    );
                    // 将 MCP 工具追加到内置工具列表中
                    tool_specs.extend(mcp_tools);
                    Some(mgr)
                }
                Err(e) => {
                    // 连接失败，仅记录警告，不影响 Agent 启动
                    tracing::warn!("MCP 连接失败，将在无 MCP 工具的情况下继续: {e}");
                    None
                }
            }
        };

        // 将 MCP 管理器包装为 Arc<tokio::sync::Mutex<>> 以支持跨 Agent 共享
        let mcp_manager_shared = mcp_manager
            .map(|mgr| Arc::new(tokio::sync::Mutex::new(mgr)));

        // ===== 蜂群子代理系统初始化 =====
        // 追加蜂群工具规范（SpawnAgent / MessageAgent / CheckAgent / WaitAgent）
        // 必须在创建 SwarmManager 之前完成，因为 tool_specs Arc 会传给 SwarmManager
        tool_specs.extend(crate::swarm::tools::build_swarm_tool_specs(&config));

        // 创建蜂群通知通道（子代理完成 → 主代理）
        let (notify_tx, swarm_rx) = mpsc::unbounded_channel();

        // 创建蜂群管理器
        let swarm = Arc::new(SwarmManager::new(
            config.clone(),
            tool_ctx.clone(),
            PathBuf::from(&config.storage.workspace_dir),
            mcp_manager_shared.clone(),
            Arc::new(tool_specs.clone()),
            notify_tx,
        ));

        // 创建工具分发器（持有 workspace_tools、workspace_session、mcp_manager 等）
        let dispatcher = ToolDispatcher {
            tool_ctx: tool_ctx.clone(),
            workspace_tools,
            workspace_session: WorkspaceToolSession::default(),
            config: config.clone(),
            mcp_manager: mcp_manager_shared,
        };

        Ok(Self {
            provider,
            model: pm.model.clone(),
            system_role_name: pm.effective_system_role_name().to_string(),
            reasoning_effort: pm.effective_reasoning_effort().map(str::to_string),
            max_steps: pm.max_steps,
            config,
            tool_ctx,
            agent_state,
            group_role_state,
            dispatcher,
            session,
            system_prompt,
            rx,
            cmd_rx,
            tool_specs,
            swarm,
            swarm_rx,
        })
    }

    /// Agent 主循环
    /// 持续接收消息事件 / CLI 指令 → 批量去抖 → 处理轮次
    /// 同时监听两个通道：QQ 消息通道 (rx) 和 CLI chat 指令通道 (cmd_rx)
    /// 直到消息通道关闭
    pub async fn run(mut self) {
        tracing::info!("Agent 循环已启动");

        // 局部枚举：标识唤醒来源
        enum WakeEvent {
            /// 收到 QQ 消息事件
            QqMessage(MessageEvent),
            /// 收到 CLI chat 主人指令
            ChatCommand(PendingCommand),
            /// 收到子代理完成通知
            SwarmNotification(crate::swarm::types::SwarmNotification),
            /// QQ 消息通道已关闭，应退出循环
            Closed,
        }

        // 标记 cmd_rx 通道是否已关闭
        // 关闭的 unbounded channel 的 recv() 立即返回 None，如果不加保护会导致
        // select! 不断选中该分支从而 CPU 100% 空转（busy-spin）
        let mut cmd_rx_closed = false;
        // 标记 swarm_rx 通道是否已关闭（SwarmManager 被 drop 时关闭）
        let mut swarm_rx_closed = false;

        loop {
            // 同时等待 QQ 消息事件、CLI chat 指令或子代理通知（三者都能唤醒 Agent）
            // 关闭的通道设标志后不再 poll，避免空转
            let wake = if cmd_rx_closed && swarm_rx_closed {
                // cmd 和 swarm 通道都已关闭，仅监听 rx
                match self.rx.recv().await {
                    Some(m) => WakeEvent::QqMessage(m),
                    None => WakeEvent::Closed,
                }
            } else if cmd_rx_closed {
                // cmd 通道关闭，监听 rx + swarm
                tokio::select! {
                    msg = self.rx.recv() => match msg {
                        Some(m) => WakeEvent::QqMessage(m),
                        None => WakeEvent::Closed,
                    },
                    notif = self.swarm_rx.recv() => match notif {
                        Some(n) => WakeEvent::SwarmNotification(n),
                        None => { swarm_rx_closed = true; continue; }
                    },
                }
            } else if swarm_rx_closed {
                // swarm 通道关闭，监听 rx + cmd
                tokio::select! {
                    msg = self.rx.recv() => match msg {
                        Some(m) => WakeEvent::QqMessage(m),
                        None => WakeEvent::Closed,
                    },
                    cmd = self.cmd_rx.recv() => match cmd {
                        Some(c) => WakeEvent::ChatCommand(c),
                        None => {
                            tracing::debug!("CLI 指令通道已关闭");
                            cmd_rx_closed = true;
                            continue;
                        }
                    },
                }
            } else {
                // 三个通道都开放
                tokio::select! {
                    msg = self.rx.recv() => match msg {
                        Some(m) => WakeEvent::QqMessage(m),
                        None => WakeEvent::Closed,
                    },
                    cmd = self.cmd_rx.recv() => match cmd {
                        Some(c) => WakeEvent::ChatCommand(c),
                        None => {
                            // cmd 通道关闭（ControlServer 停止），
                            // 设置标志后继续，后续迭代不再 poll cmd_rx
                            tracing::debug!("CLI 指令通道已关闭");
                            cmd_rx_closed = true;
                            continue;
                        }
                    },
                    notif = self.swarm_rx.recv() => match notif {
                        Some(n) => WakeEvent::SwarmNotification(n),
                        None => {
                            // swarm 通知通道关闭（SwarmManager 被 drop）
                            tracing::debug!("蜂群通知通道已关闭");
                            swarm_rx_closed = true;
                            continue;
                        }
                    },
                }
            };

            match wake {
                // ===== QQ 消息唤醒：正常去抖 + 批量处理 =====
                WakeEvent::QqMessage(first) => {
                    // 去抖：等待一小段时间，收集更多 QQ 消息
                    // 同时也收集去抖期间到达的 CLI 指令（如果通道仍开放）
                    let mut batch = vec![first];
                    let mut pending_cmds: Vec<PendingCommand> = Vec::new();
                    let debounce = tokio::time::Duration::from_secs(3);
                    let deadline = tokio::time::Instant::now() + debounce;

                    // 去抖循环：同样需要保护 cmd_rx 关闭后的空转问题
                    // 使用 cmd_rx_closed 标志条件性地 select cmd_rx 分支
                    loop {
                        if cmd_rx_closed {
                            // cmd 通道已关闭，仅收集 QQ 消息 + 等超时
                            tokio::select! {
                                msg = self.rx.recv() => {
                                    match msg {
                                        Some(m) => batch.push(m),
                                        None => break,
                                    }
                                }
                                _ = tokio::time::sleep_until(deadline) => break,
                            }
                        } else {
                            tokio::select! {
                                // 收集更多 QQ 消息
                                msg = self.rx.recv() => {
                                    match msg {
                                        Some(m) => batch.push(m),
                                        None => break,
                                    }
                                }
                                // 去抖期间到达的 CLI 指令暂存
                                cmd = self.cmd_rx.recv() => {
                                    match cmd {
                                        Some(c) => pending_cmds.push(c),
                                        // 通道关闭：设置标志，下次迭代切换到 rx-only 模式
                                        None => cmd_rx_closed = true,
                                    }
                                }
                                // 去抖超时
                                _ = tokio::time::sleep_until(deadline) => break,
                            }
                        }
                    }

                    // 检查 Agent 是否活跃
                    if !self.agent_state.is_active() {
                        tracing::debug!("Agent 已暂停，跳过 {} 条消息", batch.len());
                        continue;
                    }

                    // 检查 LLM 是否已配置
                    if !self.config.primary_model().is_configured() {
                        tracing::warn!("LLM 未配置（base_url 或 api_key 为空），跳过处理");
                        continue;
                    }

                    // 输出消息摘要日志
                    tracing::info!("Agent 开始处理 {} 条消息", batch.len());
                    for (i, msg) in batch.iter().enumerate() {
                        let preview: String = msg.formatted.chars().take(200).collect();
                        tracing::info!("  消息[{}]: {}", i + 1, preview);
                    }

                    // 处理 QQ 消息批次
                    if let Err(e) = self.process_turn(batch).await {
                        tracing::error!("Agent 处理轮次失败: {}", e);
                    }

                    // 处理去抖期间到达的 CLI 指令（逐条作为独立轮次）
                    for cmd in pending_cmds {
                        tracing::info!("处理去抖期间到达的主人指令: {}", cmd.message);
                        if let Err(e) = self.process_command_turn(cmd).await {
                            tracing::error!("主人指令处理失败: {}", e);
                        }
                    }
                }

                // ===== CLI 指令唤醒：不去抖，直接处理 =====
                WakeEvent::ChatCommand(cmd) => {
                    // 检查 Agent 是否活跃
                    if !self.agent_state.is_active() {
                        tracing::warn!("Agent 已暂停，跳过主人指令: {:?}", cmd.message);
                        continue;
                    }
                    // 检查 LLM 是否已配置
                    if !self.config.primary_model().is_configured() {
                        tracing::warn!("LLM 未配置，跳过主人指令");
                        continue;
                    }
                    // 主人指令直接处理（不去抖，不等待 QQ 消息）
                    tracing::info!("收到主人指令: {}", cmd.message);
                    if let Err(e) = self.process_command_turn(cmd).await {
                        tracing::error!("主人指令处理失败: {}", e);
                    }
                }

                // ===== 子代理完成通知唤醒 =====
                WakeEvent::SwarmNotification(notif) => {
                    // 检查 Agent 是否活跃，不活跃时丢弃通知
                    if !self.agent_state.is_active() || !self.config.primary_model().is_configured() {
                        continue;
                    }
                    // 格式化通知为用户消息
                    let status_text = if notif.success { "完成" } else { "失败" };
                    let formatted = format!(
                        "[子代理{}] {}（{}）:\n{}",
                        status_text, notif.agent_name, notif.model_display, notif.result
                    );
                    tracing::info!("子代理通知: {} — {}", notif.agent_name, status_text);
                    // 作为主人指令处理（触发 Agent 新轮次来处理子代理结果）
                    let cmd = PendingCommand {
                        message: formatted,
                        group_filter: vec![],
                        private_filter: vec![],
                    };
                    if let Err(e) = self.process_command_turn(cmd).await {
                        tracing::error!("子代理通知处理失败: {}", e);
                    }
                }

                // ===== QQ 消息通道关闭 =====
                WakeEvent::Closed => {
                    tracing::info!("Agent 消息通道已关闭，退出循环");
                    break;
                }
            }
        }

        // ===== 关闭所有运行中的子代理 =====
        self.swarm.shutdown_all().await;

        // ===== 清理 MCP 服务器连接 =====
        // 在 Agent 循环退出后，关闭所有 MCP 服务器子进程
        if let Some(ref mgr_arc) = self.dispatcher.mcp_manager {
            mgr_arc.lock().await.shutdown_all().await;
            tracing::info!("MCP 服务器已全部关闭");
        }

        tracing::info!("Agent 循环已退出");
    }

    /// 将 CLI chat 指令格式化为用户消息后作为独立轮次处理
    /// 指令以 `[主人指令]` 前缀标记，让 LLM 明确这是主人的直接命令
    async fn process_command_turn(&mut self, cmd: PendingCommand) -> anyhow::Result<()> {
        // 格式化为带前缀的用户消息
        let mut content = format!("[主人指令] {}", cmd.message);
        // 如果指定了白名单限制，附加到消息中供 LLM 参考
        if !cmd.group_filter.is_empty() {
            content.push_str(&format!("\n（本轮限定群: {:?}）", cmd.group_filter));
        }
        if !cmd.private_filter.is_empty() {
            content.push_str(&format!("\n（本轮限定私聊: {:?}）", cmd.private_filter));
        }
        // 构造一个只含该指令的批次，复用 process_turn
        // CLI 指令无 QQ 消息来源，source 为 None
        let event = MessageEvent {
            formatted: content,
            source: None,
        };
        self.process_turn(vec![event]).await
    }

    /// 将 CLI chat 指令格式化为用户消息文本
    /// 提取为独立方法，供 agentic 循环中的注入逻辑复用
    fn format_command_as_user_message(cmd: &PendingCommand) -> String {
        let mut content = format!("[主人指令] {}", cmd.message);
        if !cmd.group_filter.is_empty() {
            content.push_str(&format!("\n（本轮限定群: {:?}）", cmd.group_filter));
        }
        if !cmd.private_filter.is_empty() {
            content.push_str(&format!("\n（本轮限定私聊: {:?}）", cmd.private_filter));
        }
        content
    }

    /// 处理一批消息事件
    /// 将消息批次格式化为用户消息 → 进入 agentic 循环
    async fn process_turn(&mut self, batch: Vec<MessageEvent>) -> anyhow::Result<()> {
        // 收集本批次的消息来源（群/私聊），用于 LLM 首次成功后标记已读
        let sources: Vec<MessageSource> = batch
            .iter()
            .filter_map(|e| e.source.clone())
            .collect();

        // 将批次中的所有消息格式化为一条 user 消息
        let user_content = if batch.len() == 1 {
            batch[0].formatted.clone()
        } else {
            batch
                .iter()
                .map(|e| e.formatted.as_str())
                .collect::<Vec<_>>()
                .join("\n\n---\n\n")
        };

        // 双写到会话（JSONL 文件 + 内存缓存）
        self.session.push(ChatMessage::user(user_content))?;

        // 刷新系统提示中的日期时间
        prompt::refresh_prompt_datetime(&mut self.system_prompt);

        // 刷新系统提示中的群身份信息（管理员/群主状态可能已通过事件或定时同步变更）
        let group_roles_text = self.group_role_state.format_prompt_section();
        prompt::update_prompt_group_roles(&mut self.system_prompt, &group_roles_text);

        // 构建系统消息（使用配置的角色名称）
        let system_message = ChatMessage {
            role: self.system_role_name.clone(),
            content: self.system_prompt.clone(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            images: Vec::new(),
        };

        // Agentic 循环
        let mut steps = 0u32;
        // Finish 工具作为显式停止钩子：只有 LLM 调用 Finish 才能结束轮次
        let mut finish_called = false;
        // 临时提醒消息：当 LLM 未调用任何工具时，在下一次请求中临时追加此提醒
        // **不写入 session**，避免断流/重试场景下对话历史被污染
        // （断流可能连续发生成百上千次，如果每次都写入对话，会导致
        //   对话中累积大量重复的系统提醒，浪费 token 并干扰模型理解）
        let mut retry_reminder: Option<ChatMessage> = None;
        // 已读标记：仅在首次 LLM 请求成功后标记一次
        // 模拟真人行为——看到消息 → 标记已读 → 慢慢回复
        let mut read_marked = false;
        loop {
            // ===== 检查待处理的主人指令（工具调用之间注入） =====
            // 类似 Codex "Messages to be submitted after next tool call" 模式：
            // 当前 step（LLM 调用 + 工具执行）完成后，在下一次 LLM 请求前插入
            // 不等到 Finish — 在循环迭代之间注入，让 LLM 立即看到并响应
            while let Ok(cmd) = self.cmd_rx.try_recv() {
                let injected = Self::format_command_as_user_message(&cmd);
                tracing::info!("插入主人指令到 agentic 循环: {}", cmd.message);
                self.session.push(ChatMessage::user(injected))?;
            }

            // ===== 检查子代理完成通知（工具调用之间注入） =====
            // 子代理完成后通过 swarm_rx 发送通知，在此处注入到对话历史
            while let Ok(notif) = self.swarm_rx.try_recv() {
                let status_text = if notif.success { "完成" } else { "失败" };
                let injected = format!(
                    "[子代理{}] {}（{}）:\n{}",
                    status_text, notif.agent_name, notif.model_display, notif.result
                );
                tracing::info!("注入子代理通知: {}", notif.agent_name);
                self.session.push(ChatMessage::user(injected))?;
            }

            if steps >= self.max_steps {
                tracing::warn!(
                    "Agent 达到最大步数限制 ({})，强制停止",
                    self.max_steps
                );
                break;
            }
            steps += 1;

            // 尝试压缩上下文（如果超过阈值）
            // 从会话首条消息提取临时 CompactionState（解析 <summary> 标签）
            let mut temp_state = extract_compaction_state(self.session.messages());
            match compact::maybe_compact_history(
                &*self.provider,
                &self.model,
                &self.system_role_name,
                &system_message,
                self.reasoning_effort.as_deref(),
                &mut temp_state,
                self.session.messages_mut(),
                &self.config.compaction,
            )
            .await
            {
                Ok(Some(report)) => {
                    tracing::info!(
                        "上下文已压缩: {} → {} tokens（摘要 {} 条，保留 {} 条）",
                        report.tokens_before,
                        report.tokens_after,
                        report.summarized_messages,
                        report.kept_messages,
                    );

                    // 取出被截断后的保留消息
                    let kept = std::mem::take(self.session.messages_mut());

                    // 构建新会话的初始消息
                    let mut initial = Vec::new();
                    if let Some(summary) = temp_state.summary() {
                        // 摘要 + 前一个会话的文件路径引用
                        initial.push(ChatMessage::user(format!(
                            "{}{}{}\n\n上一条完整会话记录: {}",
                            compact::COMPACTION_SUMMARY_PREFIX,
                            summary,
                            compact::COMPACTION_SUMMARY_SUFFIX,
                            self.session.current_file_path(),
                        )));
                    }
                    initial.extend(kept);

                    // 创建新会话（旧会话文件保留完整历史）
                    self.session.rotate(initial)?;

                    // 更新系统提示中的会话引用
                    prompt::update_prompt_session_ref(
                        &mut self.system_prompt,
                        self.session.prev_session_path(),
                    );
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("上下文压缩失败（继续处理）: {}", e);
                }
            }

            // 构建请求消息
            // 摘要已经嵌入在 session.messages() 的首条消息中，无需 CompactionState 注入
            let mut request_messages =
                Vec::with_capacity(1 + self.session.messages().len());
            request_messages.push(system_message.clone());
            request_messages.extend(self.session.messages().iter().cloned());

            // 如果上一轮 LLM 未调用工具，临时追加提醒到请求体
            // .take() 消费并清除提醒，确保每次只追加一条，不会累积
            if let Some(reminder) = retry_reminder.take() {
                request_messages.push(reminder);
            }

            let request = ChatRequest {
                messages: &request_messages,
                tools: Some(&self.tool_specs),
            };

            // 调用 LLM
            tracing::info!(
                "Agent 调用 LLM (step {}/{}, 会话 {} 条消息, 请求 {} 条消息)",
                steps, self.max_steps, self.session.messages().len(), request_messages.len()
            );
            let response = self.provider.chat(request, &self.model, 0.7).await?;

            // ===== 首次 LLM 成功后标记消息已读 =====
            // 模拟真人行为：看到消息 → 标记已读 → 慢慢回复
            // 仅在首次成功时执行一次，后续 agentic 循环迭代不再重复
            if !read_marked && !sources.is_empty() {
                read_marked = true;
                self.mark_sources_as_read(&sources).await;
            }

            // 处理响应
            if response.has_tool_calls() {
                // LLM 请求执行工具调用
                let assistant_content = response.text.unwrap_or_default();
                // 如果助手附带了文本，输出它
                if !assistant_content.is_empty() {
                    tracing::info!("LLM 附带文本: {}", assistant_content);
                }
                // 记录助手消息（含工具调用）— 双写到会话
                self.session.push(ChatMessage::assistant_with_tool_calls(
                    assistant_content,
                    response.tool_calls.clone(),
                ))?;

                // 逐个执行工具调用
                for tc in &response.tool_calls {
                    // Finish 工具：显式停止钩子，不实际执行，仅标记结束
                    if tc.name == "Finish" {
                        // 提取 reason 参数用于日志
                        let reason = serde_json::from_str::<serde_json::Value>(&tc.arguments)
                            .ok()
                            .and_then(|v| v.get("reason").and_then(|r| r.as_str()).map(|s| s.to_string()))
                            .unwrap_or_else(|| "(无)".to_string());
                        tracing::info!("Finish({})", reason);
                        // 回传工具结果以保持对话完整性 — 双写到会话
                        self.session
                            .push(ChatMessage::tool_result(&tc.id, "轮次已结束。".to_string()))?;
                        finish_called = true;
                        continue;
                    }

                    tracing::debug!(
                        "工具调用: id={}, name={}, args_len={}",
                        tc.id, tc.name, tc.arguments.len()
                    );
                    let result = self.dispatch_tool(&tc.name, &tc.arguments).await;
                    tracing::debug!(
                        "工具结果 ({}): {}",
                        tc.name,
                        &result.text[..result.text.len().min(200)]
                    );
                    // 追加工具结果到会话（支持多模态图片返回）— 双写
                    if result.images.is_empty() {
                        self.session
                            .push(ChatMessage::tool_result(&tc.id, result.text))?;
                    } else {
                        self.session.push(
                            ChatMessage::tool_result_with_images(
                                &tc.id,
                                result.text,
                                result.images,
                            ),
                        )?;
                    }
                }

                // 如果本批工具调用中包含 Finish，结束循环
                if finish_called {
                    break;
                }
            } else {
                // LLM 结束轮次但没有调用 Finish（纯文本输出，常见于断流/截断）
                // 将 LLM 的文本回复记录到会话（这是真实输出）— 双写
                let text = response.text.unwrap_or_default();
                if !text.is_empty() {
                    self.session.push(ChatMessage::assistant(&text))?;
                }
                tracing::info!(
                    "Agent step {} 未调用 Finish，设置临时提醒 (文本: {} 字符)",
                    steps,
                    text.len()
                );
                // 设置临时提醒，仅在下一次请求构建时追加到请求体
                // **不写入 session**：避免断流场景下对话历史被污染
                retry_reminder = Some(ChatMessage::user(
                    "[系统提醒] 你没有调用任何工具就结束了。你的纯文本输出不会被任何人看到。\
                    请继续使用工具完成任务。当你确认所有工作已完成时，调用 Finish 工具结束本轮。".to_string()
                ));
                // 继续循环，不 break
            }
        }

        Ok(())
    }

    /// 将批次中的消息来源标记为已读
    /// 收集唯一的 group_id / user_id，分别调用 mark_group_msg_as_read / mark_private_msg_as_read
    /// 任何单个标记失败仅记录警告，不影响整体流程
    async fn mark_sources_as_read(&self, sources: &[MessageSource]) {
        // 收集唯一的群号和用户 QQ 号，避免重复调用
        let mut group_ids = std::collections::HashSet::new();
        let mut user_ids = std::collections::HashSet::new();
        for src in sources {
            match src {
                MessageSource::Group { group_id } => {
                    group_ids.insert(*group_id);
                }
                MessageSource::Private { user_id } => {
                    user_ids.insert(*user_id);
                }
                // 请求事件和通知事件没有关联的消息 ID，无需标记已读
                MessageSource::FriendRequest { .. }
                | MessageSource::GroupRequest { .. }
                | MessageSource::FriendAdded { .. } => {}
            }
        }

        // 标记群消息已读
        for gid in &group_ids {
            if let Err(e) = self.tool_ctx.api.message.mark_group_msg_as_read(*gid).await {
                tracing::warn!("标记群 {} 消息已读失败: {}", gid, e);
            } else {
                tracing::debug!("已标记群 {} 消息已读", gid);
            }
        }

        // 标记私聊消息已读
        for uid in &user_ids {
            if let Err(e) = self.tool_ctx.api.message.mark_private_msg_as_read(*uid).await {
                tracing::warn!("标记私聊 {} 消息已读失败: {}", uid, e);
            } else {
                tracing::debug!("已标记私聊 {} 消息已读", uid);
            }
        }
    }

    /// 分发工具调用到具体的实现函数
    /// 先检查是否为蜂群工具（SpawnAgent / MessageAgent / CheckAgent / WaitAgent），
    /// 如果是则路由到 SwarmManager，否则委托给 ToolDispatcher
    async fn dispatch_tool(&mut self, name: &str, arguments: &str) -> ToolResult {
        // 检查是否为蜂群工具
        if matches!(name, "SpawnAgent" | "MessageAgent" | "CheckAgent" | "WaitAgent") {
            // 解析参数 JSON
            let args: serde_json::Value = match serde_json::from_str(arguments) {
                Ok(v) => v,
                Err(e) => return ToolResult::text(format!("参数解析失败: {e}")),
            };
            // 调用蜂群工具分发函数（主代理深度为 0）
            let result = crate::swarm::tools::dispatch_swarm_tool(
                &self.swarm, name, &args, 0,
            ).await;
            return ToolResult::text(result);
        }
        // 委托给 ToolDispatcher 处理所有其他工具
        self.dispatcher.dispatch_tool(name, arguments).await
    }
}

// ============================================================================
// 会话状态辅助函数
// ============================================================================

/// 从会话消息列表中提取 CompactionState
/// 解析首条消息中的 `<summary>...</summary>` 标签，构建临时 CompactionState
/// 供 `maybe_compact_history` 使用（该函数需要 CompactionState 来决定是否增量更新摘要）
///
/// 如果首条消息不包含摘要标签，返回空的 CompactionState
fn extract_compaction_state(messages: &[ChatMessage]) -> CompactionState {
    let mut state = CompactionState::default();

    // 检查首条消息是否包含摘要
    if let Some(first) = messages.first() {
        if let Some(start) = first.content.find(compact::COMPACTION_SUMMARY_PREFIX) {
            let content_start = start + compact::COMPACTION_SUMMARY_PREFIX.len();
            if let Some(end) = first.content[content_start..].find(compact::COMPACTION_SUMMARY_SUFFIX)
            {
                let summary = first.content[content_start..content_start + end].to_string();
                if !summary.is_empty() {
                    state.set_summary(summary);
                }
            }
        }
    }

    state
}

// ============================================================================
// 工具规范定义
// ============================================================================

/// 消息段数组参数的详细描述（SendGroupMessage / SendPrivateMessage 共用）
/// 列出每种消息段类型及其 data 字段，让 LLM 能正确构造任意类型的消息
const MESSAGE_SEGMENTS_DESC: &str = "\
消息段数组（OneBot 11 格式）。每个元素为 {\"type\": \"<类型>\", \"data\": {<字段>}}。\n\
支持的类型及其 data 字段：\n\
- text: {\"text\": \"文本内容\"}\n\
- at: {\"qq\": \"用户QQ号\"} — @某人，qq 为字符串类型，\"all\" 表示 @全体成员\n\
- face: {\"id\": \"表情ID\"} — QQ 系统表情\n\
- reply: {\"id\": \"消息ID\"} — 回复某条消息，必须放在数组第一个位置\n\
- image: {\"file\": \"图片URL或路径\"} — 可选: summary(描述文本), sub_type(0=普通图片, 1=表情包)\n\
- record: {\"file\": \"语音URL或路径\"}\n\
- video: {\"file\": \"视频URL或路径\"}\n\
- json: {\"data\": \"JSON卡片字符串\"}\n\
示例（回复+文本）: [{\"type\": \"reply\", \"data\": {\"id\": \"12345\"}}, {\"type\": \"text\", \"data\": {\"text\": \"收到\"}}]\n\
示例（图片表情包）: [{\"type\": \"image\", \"data\": {\"file\": \"https://example.com/img.jpg\", \"sub_type\": 1}}]\n\
示例（@+文本）: [{\"type\": \"at\", \"data\": {\"qq\": \"123456\"}}, {\"type\": \"text\", \"data\": {\"text\": \" 你好\"}}]";

/// 构建所有内置工具的 JSON Schema 规范
/// 这些规范通过 OpenAI function calling 协议传递给 LLM
pub(crate) fn build_tool_specs() -> Vec<ToolSpec> {
    vec![
        // ===== 文件工具 =====
        ToolSpec {
            name: "Ls".to_string(),
            description: "列出工作区内指定目录的文件和子目录。路径相对于工作区根目录，\"/\" 或空字符串表示根目录。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "要列出的目录路径（相对于工作区根目录，\"/\" 表示根目录）"
                    }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "Read".to_string(),
            description: "读取工作区内的 UTF-8 文本文件。路径相对于工作区根目录。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "要读取的文件路径（相对于工作区根目录）"
                    }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "Write".to_string(),
            description: "创建新的 UTF-8 文本文件。如果文件已存在则失败，修改已有文件请用 Edit。"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "要创建的文件路径（相对于工作区根目录）"
                    },
                    "content": {
                        "type": "string",
                        "description": "文件内容"
                    }
                },
                "required": ["path", "content"]
            }),
        },
        ToolSpec {
            name: "Edit".to_string(),
            description: "编辑已存在的文件。必须先 Read 同一文件后才能 Edit，否则会报错拒绝。Read 一次后可多次 Edit。使用精确字符串替换。"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "要编辑的文件路径（相对于工作区根目录）"
                    },
                    "old_text": {
                        "type": "string",
                        "description": "要替换的原始文本"
                    },
                    "new_text": {
                        "type": "string",
                        "description": "替换后的新文本"
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "是否替换所有匹配（默认 false，仅替换第一处且要求唯一匹配）"
                    }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        },
        // ===== 消息工具 =====
        ToolSpec {
            name: "SendGroupMessage".to_string(),
            description: "向指定群发送消息。群号必须在发送白名单中。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "目标群号"
                    },
                    "message": {
                        "type": "array",
                        "description": MESSAGE_SEGMENTS_DESC,
                        "items": {
                            "type": "object",
                            "properties": {
                                "type": { "type": "string" },
                                "data": { "type": "object" }
                            },
                            "required": ["type", "data"]
                        }
                    }
                },
                "required": ["group_id", "message"]
            }),
        },
        ToolSpec {
            name: "SendPrivateMessage".to_string(),
            description: "向指定用户发送私聊消息。用户 QQ 号必须在发送白名单中。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "user_id": {
                        "type": "integer",
                        "description": "目标用户 QQ 号"
                    },
                    "message": {
                        "type": "array",
                        "description": MESSAGE_SEGMENTS_DESC,
                        "items": {
                            "type": "object",
                            "properties": {
                                "type": { "type": "string" },
                                "data": { "type": "object" }
                            },
                            "required": ["type", "data"]
                        }
                    }
                },
                "required": ["user_id", "message"]
            }),
        },
        ToolSpec {
            name: "DeleteMessage".to_string(),
            description: "撤回一条消息。只能撤回自己发送的消息。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "message_id": {
                        "type": "integer",
                        "description": "要撤回的消息 ID"
                    }
                },
                "required": ["message_id"]
            }),
        },
        ToolSpec {
            name: "SetMsgEmojiLike".to_string(),
            description: "对一条消息添加表情回应。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "message_id": {
                        "type": "integer",
                        "description": "目标消息 ID"
                    },
                    "emoji_id": {
                        "type": "integer",
                        "description": "表情 ID"
                    }
                },
                "required": ["message_id", "emoji_id"]
            }),
        },
        ToolSpec {
            name: "GroupPoke".to_string(),
            description: "在群聊中戳一戳某人。群号必须在发送白名单中。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    },
                    "user_id": {
                        "type": "integer",
                        "description": "要戳的用户 QQ 号"
                    }
                },
                "required": ["group_id", "user_id"]
            }),
        },
        ToolSpec {
            name: "FriendPoke".to_string(),
            description: "戳一戳好友。用户 QQ 号必须在发送白名单中。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "user_id": {
                        "type": "integer",
                        "description": "要戳的用户 QQ 号"
                    }
                },
                "required": ["user_id"]
            }),
        },
        // ===== 查询工具 =====
        ToolSpec {
            name: "GetFriendList".to_string(),
            description: "获取好友列表。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolSpec {
            name: "GetGroupList".to_string(),
            description: "获取已加入的群列表。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolSpec {
            name: "GetGroupMemberList".to_string(),
            description: "获取指定群的成员列表。群号必须在接收白名单中。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    }
                },
                "required": ["group_id"]
            }),
        },
        ToolSpec {
            name: "GetGroupMemberInfo".to_string(),
            description: "获取指定群成员的详细信息。群号必须在接收白名单中。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    },
                    "user_id": {
                        "type": "integer",
                        "description": "成员 QQ 号"
                    }
                },
                "required": ["group_id", "user_id"]
            }),
        },
        // ===== 社交工具 =====
        ToolSpec {
            name: "HandleFriendRequest".to_string(),
            description: "处理好友添加请求（同意或拒绝）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "flag": {
                        "type": "string",
                        "description": "请求标识（从好友请求事件中获取）"
                    },
                    "approve": {
                        "type": "boolean",
                        "description": "是否同意"
                    },
                    "remark": {
                        "type": "string",
                        "description": "同意后的备注名称（可选）"
                    }
                },
                "required": ["flag", "approve"]
            }),
        },
        ToolSpec {
            name: "HandleGroupRequest".to_string(),
            description: "处理群添加或群邀请请求（同意或拒绝）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "flag": {
                        "type": "string",
                        "description": "请求标识（从群请求事件中获取）"
                    },
                    "sub_type": {
                        "type": "string",
                        "description": "请求子类型：\"add\"（加群）或 \"invite\"（邀请）"
                    },
                    "approve": {
                        "type": "boolean",
                        "description": "是否同意"
                    },
                    "reason": {
                        "type": "string",
                        "description": "拒绝理由（可选，仅拒绝时有效）"
                    }
                },
                "required": ["flag", "sub_type", "approve"]
            }),
        },
        // ===== 好友请求查询工具 =====
        ToolSpec {
            name: "GetFriendRequests".to_string(),
            description: "从 NapCat 获取当前待处理的好友请求列表。包括历史未处理的好友请求。"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        // ===== 好友管理工具 =====
        ToolSpec {
            name: "SetFriendRemark".to_string(),
            description: "设置好友备注名称。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "user_id": {
                        "type": "integer",
                        "description": "好友 QQ 号"
                    },
                    "remark": {
                        "type": "string",
                        "description": "备注名称"
                    }
                },
                "required": ["user_id", "remark"]
            }),
        },
        ToolSpec {
            name: "DeleteFriend".to_string(),
            description: "删除好友。此操作不可撤销。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "user_id": {
                        "type": "integer",
                        "description": "要删除的好友 QQ 号"
                    }
                },
                "required": ["user_id"]
            }),
        },
        ToolSpec {
            name: "GetUnidirectionalFriendList".to_string(),
            description: "获取单向好友列表。单向好友是指对方加了你但你没有加对方的好友关系。"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        // ===== Skill 工具 =====
        ToolSpec {
            name: "Skill".to_string(),
            description: "加载已安装的技能文件。提供技能名称，可选提供技能内相对路径。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "技能名称"
                    },
                    "path": {
                        "type": "string",
                        "description": "技能内相对路径（默认为 SKILL.md）"
                    }
                },
                "required": ["name"]
            }),
        },
        // ===== 多模态工具 =====
        ToolSpec {
            name: "ReadImage".to_string(),
            description: "查看消息中的图片。通过消息 ID 定位图片并返回视觉内容。消息 ID 从消息记录的 MsgID 字段获取。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "message_id": {
                        "type": "integer",
                        "description": "包含图片的消息 ID（从消息记录的 MsgID 字段获取）"
                    },
                    "index": {
                        "type": "integer",
                        "description": "图片在消息中的位置索引（从 0 开始，默认 0 即第一张图片）"
                    }
                },
                "required": ["message_id"]
            }),
        },
        // ===== 搜索工具 =====
        ToolSpec {
            name: "GetMessage".to_string(),
            description: "通过消息 ID 获取原始消息内容（JSON 格式）。利用数据库索引快速定位，返回完整的原始数据。消息 ID 从消息记录的 MsgID 字段获取。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "message_id": {
                        "type": "integer",
                        "description": "要查找的消息 ID（从消息记录的 MsgID 字段获取）"
                    }
                },
                "required": ["message_id"]
            }),
        },
        ToolSpec {
            name: "SearchMessages".to_string(),
            description: "搜索消息记录。支持按发送者 QQ 号、群号、消息类型、时间范围等条件过滤。返回匹配消息的元数据列表。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "user_id": {
                        "type": "integer",
                        "description": "按发送者 QQ 号过滤"
                    },
                    "group_id": {
                        "type": "integer",
                        "description": "按群号过滤"
                    },
                    "message_type": {
                        "type": "string",
                        "description": "按消息类型过滤: \"group\"（群消息）或 \"private\"（私聊消息）"
                    },
                    "after": {
                        "type": "integer",
                        "description": "起始时间戳（Unix 秒，包含），只返回此时间之后的消息"
                    },
                    "before": {
                        "type": "integer",
                        "description": "截止时间戳（Unix 秒，包含），只返回此时间之前的消息"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "最大返回条数（默认 20，上限 100）"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "分页偏移量（默认 0）。翻页时使用。"
                    }
                },
                "required": []
            }),
        },
        ToolSpec {
            name: "Grep".to_string(),
            description: "在工作区文件中搜索文本。返回匹配的文件路径、行号和内容。支持指定搜索目录和文件类型过滤。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "要搜索的文本"
                    },
                    "path": {
                        "type": "string",
                        "description": "搜索起始目录（相对于工作区根，默认搜索整个工作区）"
                    },
                    "file_ext": {
                        "type": "string",
                        "description": "文件扩展名过滤（如 \"md\"、\"jsonl\"），不含前导点号"
                    },
                    "case_insensitive": {
                        "type": "boolean",
                        "description": "是否忽略大小写（默认 false）"
                    }
                },
                "required": ["pattern"]
            }),
        },
        // ===== 群管理工具（仅管理员/群主） =====
        ToolSpec {
            name: "SetGroupBan".to_string(),
            description: "群组单人禁言（仅管理员可用）。设置 duration=0 可解除禁言。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    },
                    "user_id": {
                        "type": "integer",
                        "description": "要禁言的用户 QQ 号"
                    },
                    "duration": {
                        "type": "integer",
                        "description": "禁言时长，单位秒。默认30分钟(1800)，0=解除禁言"
                    }
                },
                "required": ["group_id", "user_id"]
            }),
        },
        ToolSpec {
            name: "SetGroupWholeBan".to_string(),
            description: "群组全员禁言（仅管理员可用）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    },
                    "enable": {
                        "type": "boolean",
                        "description": "true=开启全员禁言，false=关闭全员禁言，默认 true"
                    }
                },
                "required": ["group_id"]
            }),
        },
        ToolSpec {
            name: "SetGroupKick".to_string(),
            description: "踢出群成员（仅管理员可用）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    },
                    "user_id": {
                        "type": "integer",
                        "description": "要踢出的用户 QQ 号"
                    },
                    "reject_add_request": {
                        "type": "boolean",
                        "description": "是否拒绝此人再次加群，默认 false"
                    }
                },
                "required": ["group_id", "user_id"]
            }),
        },
        ToolSpec {
            name: "SetGroupName".to_string(),
            description: "设置群名（仅管理员可用）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    },
                    "group_name": {
                        "type": "string",
                        "description": "新群名"
                    }
                },
                "required": ["group_id", "group_name"]
            }),
        },
        ToolSpec {
            name: "SetGroupPortrait".to_string(),
            description: "设置群头像（仅管理员可用）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    },
                    "file": {
                        "type": "string",
                        "description": "图片文件路径或 URL"
                    }
                },
                "required": ["group_id", "file"]
            }),
        },
        ToolSpec {
            name: "SetEssenceMsg".to_string(),
            description: "设置精华消息（仅管理员可用）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "message_id": {
                        "type": "integer",
                        "description": "要设为精华的消息 ID"
                    }
                },
                "required": ["message_id"]
            }),
        },
        ToolSpec {
            name: "DeleteEssenceMsg".to_string(),
            description: "移出精华消息（仅管理员可用）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "message_id": {
                        "type": "integer",
                        "description": "要移出精华的消息 ID"
                    }
                },
                "required": ["message_id"]
            }),
        },
        ToolSpec {
            name: "GetEssenceMsgList".to_string(),
            description: "获取精华消息列表（仅管理员可用）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    }
                },
                "required": ["group_id"]
            }),
        },
        ToolSpec {
            name: "GetGroupAtAllRemain".to_string(),
            description: "获取群 @全体成员 剩余次数（仅管理员可用）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    }
                },
                "required": ["group_id"]
            }),
        },
        ToolSpec {
            name: "GetGroupShutList".to_string(),
            description: "获取群禁言列表（仅管理员可用）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    }
                },
                "required": ["group_id"]
            }),
        },
        ToolSpec {
            name: "SendGroupNotice".to_string(),
            description: "发送群公告（仅管理员可用）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    },
                    "content": {
                        "type": "string",
                        "description": "公告内容"
                    },
                    "image": {
                        "type": "string",
                        "description": "图片 URL（可选）"
                    },
                    "pinned": {
                        "type": "integer",
                        "description": "是否置顶，1=置顶，0=不置顶"
                    }
                },
                "required": ["group_id", "content"]
            }),
        },
        ToolSpec {
            name: "DelGroupNotice".to_string(),
            description: "删除群公告（仅管理员可用）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    },
                    "notice_id": {
                        "type": "string",
                        "description": "要删除的公告 ID"
                    }
                },
                "required": ["group_id", "notice_id"]
            }),
        },
        ToolSpec {
            name: "SetGroupAdmin".to_string(),
            description: "设置或取消群管理员（仅群主可用）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    },
                    "user_id": {
                        "type": "integer",
                        "description": "目标用户 QQ 号"
                    },
                    "enable": {
                        "type": "boolean",
                        "description": "true=设置为管理员，false=取消管理员，默认 true"
                    }
                },
                "required": ["group_id", "user_id"]
            }),
        },
        ToolSpec {
            name: "SetGroupSpecialTitle".to_string(),
            description: "设置群组专属头衔（仅群主可用）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    },
                    "user_id": {
                        "type": "integer",
                        "description": "目标用户 QQ 号"
                    },
                    "special_title": {
                        "type": "string",
                        "description": "头衔内容，空字符串表示删除"
                    },
                    "duration": {
                        "type": "integer",
                        "description": "有效期，单位秒，-1=永久"
                    }
                },
                "required": ["group_id", "user_id", "special_title"]
            }),
        },
        // ===== 群组工具（无需管理员权限） =====
        ToolSpec {
            name: "SetGroupLeave".to_string(),
            description: "退出群组。如果是群主且 is_dismiss=true 则解散群。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    },
                    "is_dismiss": {
                        "type": "boolean",
                        "description": "是否解散群（仅群主有效），默认 false"
                    }
                },
                "required": ["group_id"]
            }),
        },
        ToolSpec {
            name: "GetGroupNotice".to_string(),
            description: "获取群公告列表。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    }
                },
                "required": ["group_id"]
            }),
        },
        ToolSpec {
            name: "GetGroupHonorInfo".to_string(),
            description: "获取群荣誉信息（龙王、群聊之火等）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    },
                    "honor_type": {
                        "type": "string",
                        "description": "荣誉类型：talkative=龙王, performer=群聊之火, legend=群聊炽焰, strong_newbie=冒尖小春笋, emotion=快乐之源, all=全部"
                    }
                },
                "required": ["group_id", "honor_type"]
            }),
        },
        ToolSpec {
            name: "SetGroupSign".to_string(),
            description: "群签到/打卡。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    }
                },
                "required": ["group_id"]
            }),
        },
        // ===== 流程控制工具 =====
        ToolSpec {
            name: "Finish".to_string(),
            description: "结束本轮处理。当你完成了所有需要做的事情（发送消息、更新记忆等），或明确决定沉默时，必须调用此工具。不调用 Finish 则轮次不会结束。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "reason": {
                        "type": "string",
                        "description": "结束原因的简要说明（如 \"已回复群消息\"、\"决定沉默：话题与我无关\"）"
                    }
                },
                "required": ["reason"]
            }),
        },
    ]
}
