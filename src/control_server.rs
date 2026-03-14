// src/control_server.rs
// 控制通道服务端
// 守护进程在 localhost:port 监听来自 CLI 的控制指令
// 处理 Shutdown / Status / Ai / Chat 四类请求

use crate::agent;
use crate::agent_state::AgentState;
use crate::control::*;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};

// ============================================================================
// 控制服务端上下文
// ============================================================================

/// 控制服务端共享上下文
/// 持有 AI Agent 状态、关闭信号发送端、CLI chat 指令转发通道
pub struct ControlServer {
    /// AI Agent 状态（活跃/暂停）
    pub agent_state: Arc<AgentState>,
    /// 关闭信号发送端，发送后守护进程主循环退出
    pub shutdown_tx: broadcast::Sender<()>,
    /// CLI chat 指令发送端 → Agent 接收
    /// 通过此通道将 `sbot chat` 命令转发给 Agent 循环
    pub cmd_tx: mpsc::UnboundedSender<agent::PendingCommand>,
}

// ============================================================================
// 服务端启动
// ============================================================================

/// 启动控制服务端（在后台 tokio 任务中运行）
/// 在 127.0.0.1:port 上监听 TCP 连接，每个连接 spawn 一个处理任务
pub async fn start_control_server(
    port: u16,
    ctx: Arc<ControlServer>,
) -> anyhow::Result<()> {
    // 绑定到 localhost 指定端口
    let listener = TcpListener::bind(format!("127.0.0.1:{port}")).await?;
    // 日志输出监听地址，便于调试和确认端口绑定成功
    tracing::info!("控制通道已监听: 127.0.0.1:{port}");

    loop {
        // 接受新连接，返回 (TcpStream, SocketAddr)
        let (mut stream, addr) = listener.accept().await?;
        // 克隆 Arc 引用，传入 spawned 任务
        let ctx = ctx.clone();
        // 为每个连接 spawn 独立处理任务，互不阻塞
        tokio::spawn(async move {
            // 处理连接，出错时记录警告日志
            if let Err(e) = handle_control_connection(&mut stream, &ctx).await {
                tracing::warn!("控制连接 {addr} 处理失败: {e}");
            }
        });
    }
}

// ============================================================================
// 连接处理
// ============================================================================

/// 处理单个控制连接：接收请求 -> 分发处理 -> 发送响应
/// 每个连接只处理一次请求-响应，处理完毕后连接自动关闭
async fn handle_control_connection(
    stream: &mut tokio::net::TcpStream,
    ctx: &ControlServer,
) -> anyhow::Result<()> {
    // 从 TCP 流中接收一条 ControlRequest（长度前缀帧 + JSON）
    let req: ControlRequest = recv_msg(stream).await?;
    // 根据请求类型分发到对应的处理逻辑
    let resp = match req {
        // ---- 关闭指令 ----
        ControlRequest::Shutdown => {
            tracing::info!("收到关闭指令");
            // 通过 broadcast channel 发送关闭信号
            // 守护进程主循环监听此信号并退出
            // 忽略发送结果（如果没有接收者也不影响）
            let _ = ctx.shutdown_tx.send(());
            // 返回确认消息
            ControlResponse::ok("正在关闭...")
        }
        // ---- 状态查询 ----
        ControlRequest::Status => {
            // 读取 AI Agent 当前状态
            let ai_status = if ctx.agent_state.is_active() {
                "运行中"
            } else {
                "已暂停"
            };
            // 返回格式化的状态信息
            ControlResponse::ok(format!("sbot 运行中 | AI Agent: {ai_status}"))
        }
        // ---- AI Agent 控制 ----
        ControlRequest::Ai { action } => {
            // 委托给专门的 AI 控制处理函数
            handle_ai_action(&ctx.agent_state, action)
        }
        // ---- Chat 自然语言指令 ----
        ControlRequest::Chat {
            message,
            group_filter,
            private_filter,
        } => {
            // 记录收到的指令内容和白名单过滤条件
            tracing::info!(
                "收到 chat 指令: {:?} (group_filter={:?}, private_filter={:?})",
                message,
                group_filter,
                private_filter
            );
            // 通过命令通道将指令转发给 Agent 循环
            // Agent 空闲时立即触发新轮次，agentic 循环中在工具调用之间注入
            let display = message.clone();
            match ctx.cmd_tx.send(agent::PendingCommand {
                message,
                group_filter,
                private_filter,
            }) {
                Ok(()) => ControlResponse::ok(format!("指令已提交: {display}")),
                // 发送失败说明 Agent 的 cmd_rx 已被丢弃（Agent 未启动或已退出）
                Err(_) => ControlResponse::err("Agent 未运行，无法接收指令"),
            }
        }
    };
    // 将处理结果序列化后通过 TCP 流发送回 CLI
    send_msg(stream, &resp).await?;
    Ok(())
}

// ============================================================================
// AI Agent 控制
// ============================================================================

/// 处理 AI Agent 控制命令（Start / Stop / Restart）
/// 根据当前状态和请求动作，执行状态切换并返回操作结果
fn handle_ai_action(state: &AgentState, action: AiControlAction) -> ControlResponse {
    match action {
        // ---- 启动 AI Agent ----
        AiControlAction::Start => {
            // 检查是否已经在运行，避免重复启动
            if state.is_active() {
                ControlResponse::ok("AI Agent 已经在运行中")
            } else {
                // 激活 AI Agent（原子操作，线程安全）
                state.activate();
                tracing::info!("AI Agent 已启动");
                ControlResponse::ok("AI Agent 已启动")
            }
        }
        // ---- 暂停 AI Agent ----
        AiControlAction::Stop => {
            // 检查是否已经暂停，避免重复操作
            if !state.is_active() {
                ControlResponse::ok("AI Agent 已经处于暂停状态")
            } else {
                // 停用 AI Agent（消息收集继续，仅停止 LLM 调用）
                state.deactivate();
                tracing::info!("AI Agent 已暂停（消息收集继续）");
                ControlResponse::ok("AI Agent 已暂停（消息收集继续）")
            }
        }
        // ---- 重启 AI Agent ----
        AiControlAction::Restart => {
            // 先停用再激活，确保状态完全重置
            state.deactivate();
            state.activate();
            tracing::info!("AI Agent 已重启");
            ControlResponse::ok("AI Agent 已重启")
        }
    }
}
