// src/mcp/transport.rs
// MCP Stdio 传输层
// 通过子进程的 stdin/stdout 进行 JSON-RPC 2.0 通信
// 每个 MCP 服务器作为一个子进程启动，通过标准输入/输出管道交换 JSON 消息
//
// 协议要求：
// - 每条 JSON 消息以换行符 `\n` 结尾
// - 请求发送到子进程 stdin，响应从子进程 stdout 读取
// - 子进程的 stderr 被丢弃（避免 MCP 服务器的调试输出干扰通信）

use anyhow::{Context as _, bail};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout};

use super::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, next_request_id};

// ============================================================================
// StdioTransport — Stdio 传输层
// ============================================================================

/// Stdio 传输层
/// 管理一个 MCP 服务器子进程的 stdin/stdout 通信
/// 提供 JSON-RPC 请求/响应和通知发送功能
pub struct StdioTransport {
    /// MCP 服务器子进程句柄（用于关闭时 kill）
    child: Child,
    /// 子进程 stdin 的带缓冲写入器（发送 JSON-RPC 请求）
    stdin: BufWriter<ChildStdin>,
    /// 子进程 stdout 的带缓冲读取器（接收 JSON-RPC 响应）
    stdout: BufReader<ChildStdout>,
    /// 单次请求的超时时间（从发送请求到收到响应的最大等待时间）
    timeout: Duration,
}

impl StdioTransport {
    /// 启动 MCP 服务器子进程并建立 Stdio 通信管道
    ///
    /// # 参数
    /// - `command`: 启动命令（如 "npx", "python", "node" 等可执行文件名或路径）
    /// - `args`: 命令行参数列表（传递给子进程）
    /// - `env`: 额外环境变量（追加到子进程的环境中，不覆盖系统环境）
    /// - `timeout`: 单次 JSON-RPC 请求的超时时间
    ///
    /// # 返回
    /// 成功时返回已连接的 StdioTransport 实例
    /// 失败时返回错误（如命令不存在、无法创建管道等）
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        // 构建子进程命令
        // stdin/stdout 设为 piped 以建立通信管道
        // stderr 设为 null 以丢弃 MCP 服务器的调试输出
        let mut child = tokio::process::Command::new(command)
            .args(args)                             // 设置命令行参数
            .envs(env)                              // 追加额外环境变量
            .stdin(std::process::Stdio::piped())    // stdin 管道（发送请求）
            .stdout(std::process::Stdio::piped())   // stdout 管道（接收响应）
            .stderr(std::process::Stdio::null())    // stderr 丢弃（避免干扰）
            .spawn()                                // 启动子进程
            .with_context(|| format!("启动 MCP 服务器失败: {} {:?}", command, args))?;

        // 从子进程句柄提取 stdin/stdout 管道
        // take() 将 Option<ChildStdin> 转为 ChildStdin，若 None 则表示管道创建失败
        let child_stdin = child.stdin.take()
            .ok_or_else(|| anyhow::anyhow!("无法获取 MCP 服务器的 stdin 管道"))?;
        let child_stdout = child.stdout.take()
            .ok_or_else(|| anyhow::anyhow!("无法获取 MCP 服务器的 stdout 管道"))?;

        // 包裹为带缓冲的读写器，提高 I/O 效率
        let stdin = BufWriter::new(child_stdin);
        let stdout = BufReader::new(child_stdout);

        Ok(Self {
            child,
            stdin,
            stdout,
            timeout,
        })
    }

    /// 发送 JSON-RPC 请求并等待响应
    ///
    /// # 参数
    /// - `method`: JSON-RPC 方法名（如 "initialize", "tools/list", "tools/call"）
    /// - `params`: 方法参数（可选的 JSON 值）
    ///
    /// # 返回
    /// 成功时返回响应的 result 字段值
    /// 失败时返回错误（超时、子进程退出、JSON-RPC 错误等）
    ///
    /// # 协议流程
    /// 1. 构造 JsonRpcRequest，分配唯一 id
    /// 2. 序列化为 JSON 字符串 + 换行符，写入 stdin
    /// 3. 从 stdout 读取一行 JSON 响应
    /// 4. 解析为 JsonRpcResponse，检查 error 字段
    /// 5. 返回 result 字段值
    pub async fn request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value> {
        // 构造 JSON-RPC 请求体
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),    // 协议版本固定
            id: next_request_id(),          // 分配唯一递增 ID
            method: method.to_string(),     // 方法名
            params,                         // 参数
        };

        // 序列化为 JSON 字符串
        let mut line = serde_json::to_string(&req)
            .with_context(|| format!("序列化 JSON-RPC 请求失败: {}", method))?;
        // 追加换行符作为消息分隔符
        line.push('\n');

        // 写入子进程 stdin 并刷新缓冲区
        self.stdin.write_all(line.as_bytes()).await
            .with_context(|| format!("写入 MCP 请求失败: {}", method))?;
        self.stdin.flush().await
            .with_context(|| format!("刷新 MCP stdin 失败: {}", method))?;

        // 带超时地从 stdout 读取一行响应
        let mut response_line = String::new();
        let read_result = tokio::time::timeout(
            self.timeout,
            self.stdout.read_line(&mut response_line),
        )
        .await
        .map_err(|_| anyhow::anyhow!("MCP 请求超时 ({}s): {}", self.timeout.as_secs(), method))?
        .with_context(|| format!("读取 MCP 响应失败: {}", method))?;

        // read_line 返回 0 表示 EOF（子进程已退出）
        if read_result == 0 {
            bail!("MCP 服务器已关闭连接 (EOF): {}", method);
        }

        // 解析 JSON-RPC 响应
        let resp: JsonRpcResponse = serde_json::from_str(&response_line)
            .with_context(|| format!(
                "解析 MCP 响应 JSON 失败: {}\n  原始响应: {:?}",
                method,
                response_line.trim()
            ))?;

        // 检查是否包含错误
        if let Some(err) = resp.error {
            bail!("MCP 服务器返回错误 ({}): {}", method, err);
        }

        // 返回 result 字段，无 result 时返回 null
        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    }

    /// 发送 JSON-RPC 通知（无 id 字段，不期待响应）
    ///
    /// # 参数
    /// - `method`: 通知方法名（如 "notifications/initialized"）
    /// - `params`: 通知参数（可选）
    ///
    /// # 说明
    /// MCP 协议中的 "initialized" 是通知类型消息，没有 id 字段，
    /// 服务器不会发送响应。只需写入 stdin 即可。
    pub async fn notify(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        // 构造通知体（无 id 字段）
        let notification = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),    // 协议版本固定
            method: method.to_string(),     // 通知方法名
            params,                         // 参数
        };

        // 序列化为 JSON 字符串
        let mut line = serde_json::to_string(&notification)
            .with_context(|| format!("序列化 JSON-RPC 通知失败: {}", method))?;
        // 追加换行符作为消息分隔符
        line.push('\n');

        // 写入子进程 stdin 并刷新缓冲区
        self.stdin.write_all(line.as_bytes()).await
            .with_context(|| format!("写入 MCP 通知失败: {}", method))?;
        self.stdin.flush().await
            .with_context(|| format!("刷新 MCP stdin 失败 (通知): {}", method))?;

        Ok(())
    }

    /// 关闭 MCP 服务器连接
    ///
    /// # 关闭流程
    /// 1. 丢弃 stdin 写入器，让子进程收到 EOF
    /// 2. 杀死子进程（防止子进程未正常退出）
    /// 3. 等待子进程退出并回收资源
    ///
    /// # 注意
    /// 此方法不会失败——即使 kill/wait 出错也静默忽略，
    /// 因为关闭时子进程可能已经退出
    pub async fn shutdown(&mut self) {
        // 尝试杀死子进程（忽略错误，子进程可能已退出）
        let _ = self.child.kill().await;
        // 等待子进程完全退出并回收系统资源（忽略错误）
        let _ = self.child.wait().await;
    }
}
