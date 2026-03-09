// src/control.rs
// 控制通道协议定义
// 守护进程在 localhost TCP 端口监听，CLI 客户端发送 ControlRequest，接收 ControlResponse
// 帧格式：4 字节大端长度 + JSON payload

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 默认控制端口
pub const DEFAULT_CONTROL_PORT: u16 = 19800;

/// 控制请求（CLI → 守护进程）
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ControlRequest {
    /// 请求关闭守护进程
    Shutdown,
    /// 查询运行状态
    Status,
    /// 控制 AI Agent
    Ai {
        /// AI 控制动作
        action: AiControlAction,
    },
    /// 发送自然语言 Chat 指令
    Chat {
        /// 自然语言消息内容
        message: String,
        /// 本轮限定群白名单（空 = 不限定，使用全局）
        group_filter: Vec<i64>,
        /// 本轮限定私聊白名单（空 = 不限定，使用全局）
        private_filter: Vec<i64>,
    },
}

/// AI Agent 控制动作
#[derive(Debug, Serialize, Deserialize)]
pub enum AiControlAction {
    /// 启动 AI Agent
    Start,
    /// 暂停 AI Agent（消息收集继续）
    Stop,
    /// 重启 AI Agent
    Restart,
}

/// 控制响应（守护进程 → CLI）
#[derive(Debug, Serialize, Deserialize)]
pub struct ControlResponse {
    /// 是否成功
    pub ok: bool,
    /// 消息（成功时为状态信息，失败时为错误描述）
    pub message: String,
}

impl ControlResponse {
    /// 创建成功响应
    pub fn ok(msg: impl Into<String>) -> Self {
        Self { ok: true, message: msg.into() }
    }
    /// 创建失败响应
    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, message: msg.into() }
    }
}

/// 发送一条 JSON 消息（长度前缀帧）
/// 帧格式：[4字节大端长度][JSON payload]
pub async fn send_msg<T: Serialize>(stream: &mut TcpStream, msg: &T) -> anyhow::Result<()> {
    // 序列化为 JSON 字节
    let json = serde_json::to_vec(msg)?;
    // 写入 4 字节大端长度前缀
    let len = (json.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    // 写入 JSON payload
    stream.write_all(&json).await?;
    Ok(())
}

/// 接收一条 JSON 消息（长度前缀帧）
/// 先读取 4 字节长度前缀，再读取对应长度的 JSON payload
pub async fn recv_msg<T: serde::de::DeserializeOwned>(stream: &mut TcpStream) -> anyhow::Result<T> {
    // 读取 4 字节长度前缀
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    // 防止恶意超大负载（上限 1MB）
    anyhow::ensure!(len <= 1_048_576, "控制消息过大: {len} bytes");
    // 读取 JSON payload
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    // 反序列化
    Ok(serde_json::from_slice(&buf)?)
}
