// src/mcp/manager.rs
// MCP 连接管理器
// 管理多个 MCP 服务器的连接生命周期和工具路由
// 负责：初始化握手 → 工具发现 → 工具调用路由 → 优雅关闭
//
// 工具命名规则：
// - 内部 MCP 工具名：由 MCP 服务器自行定义（如 "search", "read_file"）
// - 暴露给 Agent 的工具名：`mcp.<服务器名>.<工具名>`（全局唯一）
// - 分割规则：去掉 "mcp." 前缀后，以第一个 "." 分割为服务器名和工具名

use anyhow::Context as _;
use serde_json::json;
use std::collections::HashMap;

use super::McpToolDef;
use super::transport::StdioTransport;
use crate::config::McpServerConfig;
use crate::providers::traits::ToolSpec;

// ============================================================================
// MCP 服务器连接
// ============================================================================

/// 单个 MCP 服务器的活跃连接
/// 持有传输层和该服务器提供的工具列表
pub struct McpServerConnection {
    /// Stdio 传输层（子进程 stdin/stdout 通信）
    transport: StdioTransport,
    /// 该服务器提供的工具列表（通过 tools/list 获取）
    #[allow(dead_code)]
    tools: Vec<McpToolDef>,
}

// ============================================================================
// MCP 连接管理器
// ============================================================================

/// MCP 连接管理器
/// 管理所有已连接的 MCP 服务器，提供统一的工具路由接口
pub struct McpConnectionManager {
    /// 按服务器名称索引的连接映射
    /// 键为配置中的服务器名称，值为活跃连接
    servers: HashMap<String, McpServerConnection>,
}

impl McpConnectionManager {
    /// 连接所有已启用的 MCP 服务器
    ///
    /// # 参数
    /// - `configs`: 服务器配置映射（键为服务器名称，值为配置）
    ///
    /// # 连接流程（每个服务器）
    /// 1. 检查 enabled 标志，跳过禁用的服务器
    /// 2. 启动子进程并建立 Stdio 管道
    /// 3. 发送 "initialize" 请求进行 MCP 握手
    /// 4. 发送 "notifications/initialized" 通知确认初始化完成
    /// 5. 发送 "tools/list" 请求获取工具列表
    ///
    /// # 容错
    /// 单个服务器连接失败不影响其他服务器，仅记录警告日志
    /// 所有服务器都失败时仍返回 Ok（空管理器）
    pub async fn connect_all(
        configs: &HashMap<String, McpServerConfig>,
    ) -> anyhow::Result<Self> {
        // 预分配服务器连接映射
        let mut servers = HashMap::new();

        // 遍历所有配置的 MCP 服务器
        for (name, cfg) in configs {
            // 跳过未启用的服务器
            if !cfg.enabled {
                tracing::info!("MCP 服务器 '{}' 已禁用，跳过", name);
                continue;
            }

            // 尝试连接单个服务器（失败时记录警告并继续）
            match Self::connect_one(name, cfg).await {
                Ok(conn) => {
                    tracing::info!(
                        "MCP 服务器 '{}' 已连接 — {} 个工具",
                        name,
                        conn.tools.len()
                    );
                    servers.insert(name.clone(), conn);
                }
                Err(e) => {
                    // 连接失败是非致命的，Agent 可以在无此 MCP 服务器的情况下继续
                    tracing::warn!("MCP 服务器 '{}' 连接失败: {}", name, e);
                }
            }
        }

        Ok(Self { servers })
    }

    /// 连接单个 MCP 服务器（内部方法）
    ///
    /// # 参数
    /// - `name`: 服务器名称（仅用于日志）
    /// - `cfg`: 服务器配置
    ///
    /// # 返回
    /// 成功时返回已初始化的服务器连接
    async fn connect_one(
        name: &str,
        cfg: &McpServerConfig,
    ) -> anyhow::Result<McpServerConnection> {
        // 计算超时时间（从配置的秒数转换为 Duration）
        let timeout = std::time::Duration::from_secs(cfg.tool_timeout_secs);

        // 步骤 1：启动子进程并建立 Stdio 管道
        tracing::info!(
            "MCP 服务器 '{}' 启动中: {} {:?}",
            name, cfg.command, cfg.args
        );
        let mut transport = StdioTransport::spawn(
            &cfg.command,
            &cfg.args,
            &cfg.env,
            timeout,
        )
        .await
        .with_context(|| format!("MCP 服务器 '{}' 启动失败", name))?;

        // 步骤 2：MCP initialize 握手
        // 发送客户端信息和协议版本，服务器返回其能力声明
        let _init_result = transport
            .request(
                "initialize",
                Some(json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "ScapegoatBot",
                        "version": "0.1.0"
                    }
                })),
            )
            .await
            .with_context(|| format!("MCP 服务器 '{}' 初始化握手失败", name))?;

        // 步骤 3：发送 initialized 通知
        // 这是 JSON-RPC 通知（无 id），告知服务器客户端已完成初始化
        // MCP 协议要求在 initialize 成功后发送此通知
        transport
            .notify("notifications/initialized", None)
            .await
            .with_context(|| format!("MCP 服务器 '{}' 发送 initialized 通知失败", name))?;

        // 步骤 4：获取工具列表
        // 调用 tools/list 方法，获取服务器提供的所有工具定义
        let tools_result = transport
            .request("tools/list", None)
            .await
            .with_context(|| format!("MCP 服务器 '{}' 获取工具列表失败", name))?;

        // 解析工具列表：从 result.tools 字段提取 Vec<McpToolDef>
        let tools: Vec<McpToolDef> = if let Some(tools_array) = tools_result.get("tools") {
            serde_json::from_value(tools_array.clone())
                .with_context(|| format!("MCP 服务器 '{}' 工具列表解析失败", name))?
        } else {
            // 没有 tools 字段时视为该服务器没有提供任何工具
            tracing::warn!("MCP 服务器 '{}' 返回的工具列表为空", name);
            Vec::new()
        };

        Ok(McpServerConnection { transport, tools })
    }

    /// 列出所有已连接 MCP 服务器提供的工具规范
    ///
    /// # 工具名命名规则
    /// 每个 MCP 工具转换为 Agent 可用的 ToolSpec：
    /// - name: `mcp.<服务器名>.<工具名>`（全局唯一标识符）
    /// - description: `[MCP: <服务器名>] <工具描述>`（标记来源）
    /// - parameters: 直接使用 MCP 工具的 inputSchema（JSON Schema）
    ///
    /// # 返回
    /// 所有 MCP 工具的 ToolSpec 列表，可直接追加到 Agent 的工具列表中
    pub fn list_all_tool_specs(&self) -> Vec<ToolSpec> {
        // 预估容量：每个服务器平均提供若干个工具
        let mut specs = Vec::new();

        // 遍历所有已连接的服务器
        for (server_name, conn) in &self.servers {
            // 遍历该服务器提供的每个工具
            for tool in &conn.tools {
                specs.push(ToolSpec {
                    // 工具名：mcp.<服务器名>.<工具名>
                    name: format!("mcp.{}.{}", server_name, tool.name),
                    // 描述：标记 MCP 来源 + 原始描述
                    description: format!(
                        "[MCP: {}] {}",
                        server_name,
                        tool.description.as_deref().unwrap_or("")
                    ),
                    // 参数 schema：直接透传 MCP 工具的 inputSchema
                    parameters: tool.input_schema.clone(),
                });
            }
        }

        specs
    }

    /// 调用指定 MCP 服务器上的工具
    ///
    /// # 参数
    /// - `server_name`: 目标服务器名称（配置中的键名）
    /// - `tool_name`: MCP 工具名称（服务器内的原始工具名）
    /// - `arguments`: 工具调用参数（JSON 对象）
    ///
    /// # 返回
    /// 成功时返回工具输出的文本内容
    /// 如果工具返回 isError=true，文本前缀会加上 "[MCP 错误] "
    ///
    /// # 错误
    /// 服务器不存在、通信失败、响应解析失败时返回错误
    pub async fn call_tool(
        &mut self,
        server_name: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<String> {
        // 查找目标服务器连接
        let conn = self.servers.get_mut(server_name)
            .ok_or_else(|| anyhow::anyhow!("MCP 服务器 '{}' 未连接", server_name))?;

        // 调用 tools/call 方法
        let result = conn.transport
            .request(
                "tools/call",
                Some(json!({
                    "name": tool_name,
                    "arguments": arguments
                })),
            )
            .await
            .with_context(|| format!(
                "MCP 工具调用失败: {}.{}",
                server_name, tool_name
            ))?;

        // 解析 CallToolResult
        let call_result: super::CallToolResult = serde_json::from_value(result)
            .with_context(|| format!(
                "MCP 工具结果解析失败: {}.{}",
                server_name, tool_name
            ))?;

        // 拼接所有内容块中的文本
        let mut output = String::new();
        for content in &call_result.content {
            // 当前仅处理 text 类型的内容块
            if let Some(text) = &content.text {
                if !output.is_empty() {
                    // 多个内容块之间用换行分隔
                    output.push('\n');
                }
                output.push_str(text);
            }
        }

        // 如果标记为错误结果，在输出前添加错误前缀
        if call_result.is_error == Some(true) {
            Ok(format!("[MCP 错误] {}", output))
        } else {
            Ok(output)
        }
    }

    /// 关闭所有 MCP 服务器连接
    ///
    /// 遍历所有已连接的服务器，逐个关闭传输层
    /// 关闭操作不会失败——即使单个服务器关闭出错也静默忽略
    pub async fn shutdown_all(&mut self) {
        // 遍历所有服务器，逐个关闭
        for (name, conn) in self.servers.iter_mut() {
            tracing::info!("正在关闭 MCP 服务器: {}", name);
            conn.transport.shutdown().await;
        }
    }
}

// ============================================================================
// 工具名分割辅助函数
// ============================================================================

/// 从完整的 MCP 工具名中分割出服务器名和工具名
///
/// # 命名格式
/// `mcp.<服务器名>.<工具名>`
/// - 服务器名：配置中的键名（不含点号）
/// - 工具名：MCP 服务器内的原始工具名（可能包含点号）
///
/// # 分割规则
/// 去掉 "mcp." 前缀后，以第一个 "." 分割：
/// - `mcp.server.tool` → ("server", "tool")
/// - `mcp.server.ns.tool` → ("server", "ns.tool")
/// - `mcp.server` → None（无工具名部分）
/// - `other.name` → None（不以 "mcp." 开头）
///
/// # 返回
/// Some((服务器名, 工具名)) 或 None（格式不合法）
pub fn split_mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    // 检查并去掉 "mcp." 前缀
    let rest = name.strip_prefix("mcp.")?;
    // 以第一个 "." 分割为服务器名和工具名
    let dot_pos = rest.find('.')?;
    // 分割：dot_pos 之前为服务器名，dot_pos+1 之后为工具名
    let server_name = &rest[..dot_pos];
    let tool_name = &rest[dot_pos + 1..];
    // 服务器名和工具名都不能为空
    if server_name.is_empty() || tool_name.is_empty() {
        return None;
    }
    Some((server_name, tool_name))
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试：标准 MCP 工具名分割
    #[test]
    fn split_standard_name() {
        // 标准格式：mcp.server.tool
        let result = split_mcp_tool_name("mcp.filesystem.read_file");
        assert_eq!(result, Some(("filesystem", "read_file")));
    }

    /// 测试：工具名包含点号
    #[test]
    fn split_dotted_tool_name() {
        // 工具名含点号：mcp.server.ns.tool → server="server", tool="ns.tool"
        let result = split_mcp_tool_name("mcp.myserver.namespace.tool_name");
        assert_eq!(result, Some(("myserver", "namespace.tool_name")));
    }

    /// 测试：非 MCP 工具名返回 None
    #[test]
    fn split_non_mcp_name() {
        // 不以 "mcp." 开头
        assert_eq!(split_mcp_tool_name("Read"), None);
        assert_eq!(split_mcp_tool_name("SendGroupMessage"), None);
        assert_eq!(split_mcp_tool_name("other.server.tool"), None);
    }

    /// 测试：格式不完整返回 None
    #[test]
    fn split_incomplete_name() {
        // 缺少工具名部分
        assert_eq!(split_mcp_tool_name("mcp.server"), None);
        // 空服务器名
        assert_eq!(split_mcp_tool_name("mcp..tool"), None);
        // 空工具名
        assert_eq!(split_mcp_tool_name("mcp.server."), None);
        // 仅前缀
        assert_eq!(split_mcp_tool_name("mcp."), None);
    }
}
