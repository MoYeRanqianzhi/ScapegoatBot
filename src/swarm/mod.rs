// src/swarm/mod.rs
// 蜂群（Swarm）模块
// 管理多 Agent 协作的核心基础设施
//
// 架构：
// - types.rs：蜂群类型定义（SpawnConfig、AgentStatus、AgentHandle 等）
// - dispatch.rs：ToolDispatcher — 提取自 AgentRunner 的工具分发逻辑
// - runner.rs：SubAgentRunner — 子代理的 agentic 循环实现
// - prompt.rs：子代理系统提示构建器
// - tools.rs：蜂群工具定义（SpawnAgent / MessageAgent / CheckAgent / WaitAgent）
// - mod.rs（本文件）：SwarmManager — 蜂群管理器，协调子代理的生命周期

// ============================================================================
// 子模块声明
// ============================================================================

/// 工具分发器：将 LLM 工具调用路由到具体实现
/// 提取自 AgentRunner，使工具分发逻辑可在多个 Agent 之间复用
pub mod dispatch;
/// 蜂群类型定义：SpawnConfig、AgentStatus、AgentHandle、SwarmNotification 等
pub mod types;
/// 子代理运行器：子代理的核心 agentic 循环
pub mod runner;
/// 子代理系统提示构建器
pub mod prompt;
/// 蜂群工具定义与分发
pub mod tools;

// 重导出核心类型
pub use types::*;

// ============================================================================
// 导入
// ============================================================================

use crate::config::AppConfig;                    // 全局应用配置
use crate::providers::traits::ToolSpec;          // 工具规范定义
use crate::tools::ToolContext;                   // 工具执行上下文
use crate::tools::workspace::{WorkspaceTools, WorkspaceToolSession}; // 工作区工具
use anyhow::bail;                                // 错误处理
use std::collections::HashMap;                   // 哈希表
use std::path::PathBuf;                          // 路径类型
use std::sync::Arc;                              // 原子引用计数
use tokio::sync::mpsc;                           // 异步通道

// ============================================================================
// 蜂群管理器
// ============================================================================

/// 蜂群管理器
/// 协调子代理的完整生命周期：创建、消息传递、状态查询、等待完成和关闭
/// 通过 Arc<SwarmManager> 在主代理和子代理之间共享
pub struct SwarmManager {
    /// 子代理句柄映射表（名称 → AgentHandle）
    /// 使用 tokio::sync::Mutex 因为 spawn/wait 等操作是异步的
    agents: tokio::sync::Mutex<HashMap<String, AgentHandle>>,
    /// 全局应用配置（用于读取 swarm 参数和 models 列表）
    config: Arc<AppConfig>,
    /// 工具执行上下文（API + 白名单 + Skill + 图片注册表 + 消息索引）
    /// 所有子代理共享同一个 ToolContext
    tool_ctx: Arc<ToolContext>,
    /// 工作区根目录路径
    workspace_dir: PathBuf,
    /// MCP 连接管理器（所有子代理共享）
    /// None 表示未配置 MCP 或连接失败
    mcp_manager: Option<Arc<tokio::sync::Mutex<crate::mcp::McpConnectionManager>>>,
    /// 工具规范列表（所有子代理共享同一套工具定义）
    /// 注意：这里包含蜂群工具自身（SpawnAgent 等），因为子代理也可以 spawn 子代理
    tool_specs: Arc<Vec<ToolSpec>>,
    /// 完成通知发送通道（子代理 → AgentRunner）
    /// 所有子代理共享同一个发送端的克隆
    notify_tx: mpsc::UnboundedSender<SwarmNotification>,
}

impl SwarmManager {
    /// 创建蜂群管理器
    ///
    /// # 参数
    /// - `config`: 全局应用配置
    /// - `tool_ctx`: 工具执行上下文（共享）
    /// - `workspace_dir`: 工作区根目录
    /// - `mcp_manager`: MCP 连接管理器（可选，共享）
    /// - `tool_specs`: 工具规范列表（含蜂群工具）
    /// - `notify_tx`: 完成通知发送通道
    pub fn new(
        config: Arc<AppConfig>,
        tool_ctx: Arc<ToolContext>,
        workspace_dir: PathBuf,
        mcp_manager: Option<Arc<tokio::sync::Mutex<crate::mcp::McpConnectionManager>>>,
        tool_specs: Arc<Vec<ToolSpec>>,
        notify_tx: mpsc::UnboundedSender<SwarmNotification>,
    ) -> Self {
        Self {
            agents: tokio::sync::Mutex::new(HashMap::new()),
            config,
            tool_ctx,
            workspace_dir,
            mcp_manager,
            tool_specs,
            notify_tx,
        }
    }

    /// 生成子代理
    ///
    /// 验证名称唯一性、深度限制、并发限制后，
    /// 创建 Provider、ToolDispatcher、构建子代理 prompt，
    /// 在新的 tokio 任务中启动 SubAgentRunner
    ///
    /// # 参数
    /// - `self_arc`: SwarmManager 的 Arc 引用（用于传递给子代理）
    /// - `spawn_config`: 子代理生成配置
    /// - `parent_depth`: 调用方的嵌套深度（主代理为 0）
    ///
    /// # 返回
    /// 成功时返回确认消息字符串
    pub fn spawn(
        self_arc: Arc<SwarmManager>,
        spawn_config: SpawnConfig,
        parent_depth: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>> {
        Box::pin(Self::spawn_inner(self_arc, spawn_config, parent_depth))
    }

    /// spawn 的内部实现（async fn）
    /// 通过 Box::pin 包装为显式的 Pin<Box<dyn Future + Send>>
    /// 解决递归子代理 spawn 导致的 future not Send 问题
    async fn spawn_inner(
        self_arc: Arc<SwarmManager>,
        spawn_config: SpawnConfig,
        parent_depth: u32,
    ) -> anyhow::Result<String> {
        // 计算子代理的深度
        let child_depth = parent_depth + 1;

        // ===== 安全检查 =====

        // 检查蜂群功能是否启用
        if !self_arc.config.swarm.enabled {
            bail!("蜂群子代理功能未启用（swarm.enabled = false）");
        }

        // 检查嵌套深度限制
        if child_depth > self_arc.config.swarm.max_depth {
            bail!(
                "嵌套深度超限: 当前深度 {} + 1 > 最大深度 {}",
                parent_depth, self_arc.config.swarm.max_depth
            );
        }

        // ===== 占位符插入 =====
        // 在创建 Provider 等耗时操作之前，先向 agents 映射表插入占位符 AgentHandle
        // 这样可以防止并发 spawn 使用相同名称通过验证（修复竞态条件 C1）
        // 同时确保子代理在 tokio 任务启动前就已注册（修复 I5）

        // 预先创建通信通道和共享状态
        let (tx, rx) = mpsc::unbounded_channel();
        let status = Arc::new(std::sync::Mutex::new(AgentStatus::Running));

        {
            let mut agents = self_arc.agents.lock().await;

            // 检查名称唯一性
            if agents.contains_key(&spawn_config.name) {
                bail!(
                    "子代理名称 '{}' 已存在（请使用唯一名称）",
                    spawn_config.name
                );
            }

            // 检查并发限制（统计所有运行中的子代理）
            let running_count = agents
                .values()
                .filter(|h| {
                    matches!(
                        *h.status.lock().unwrap_or_else(|e| e.into_inner()),
                        AgentStatus::Running
                    )
                })
                .count() as u32;
            if running_count >= self_arc.config.swarm.max_concurrency {
                bail!(
                    "并发子代理数超限: 当前 {} >= 最大 {}",
                    running_count, self_arc.config.swarm.max_concurrency
                );
            }

            // 插入占位符 AgentHandle（join_handle 为 None，后续更新）
            // 占位符确保：
            // 1. 其他并发 spawn 无法使用同一名称
            // 2. 子代理在 tokio 任务启动前就已在注册表中
            let placeholder = AgentHandle {
                name: spawn_config.name.clone(),
                model_display: String::new(), // 占位，后续更新
                created_at: chrono::Local::now(),
                status: status.clone(),
                tx: tx.clone(),
                join_handle: None, // 占位，任务启动后更新
            };
            agents.insert(spawn_config.name.clone(), placeholder);
            // 锁在此处释放（agents 超出作用域）
        }

        // ===== 模型解析 + Provider 创建 + Runner 启动 =====
        // 以下所有可失败步骤包裹在 spawn_after_placeholder 中，
        // 失败时自动从 agents 映射表移除占位符，确保不会残留无效条目

        /// 占位符清理宏：从 agents 映射表中移除指定名称的占位符
        /// 用于在 Provider 创建、ToolDispatcher 创建等步骤失败时的回滚
        macro_rules! remove_placeholder_on_err {
            ($self_arc:expr, $name:expr, $err:expr) => {{
                // 移除占位符条目
                let mut agents = $self_arc.agents.lock().await;
                agents.remove($name);
                return Err($err);
            }};
        }

        // ----- 模型解析 -----
        // 统一路径：无论是否指定 model_name，都通过 find_model() 解析
        // 未指定 model_name 时使用 config.llm.primary_model 作为默认
        let model_ref_name = spawn_config.model_name
            .as_deref()
            .unwrap_or(&self_arc.config.llm.primary_model);
        let mc = match self_arc.config.find_model(model_ref_name) {
            Some(mc) => mc,
            None => {
                // 模型不存在，移除占位符后返回错误
                remove_placeholder_on_err!(
                    self_arc, &spawn_config.name,
                    anyhow::anyhow!("未找到模型 '{}'", model_ref_name)
                );
            }
        };
        let (
            provider_type,
            base_url,
            api_key,
            max_tokens,
            timeout,
            wire_api,
            model_name,
            model_display,
            system_role_name,
            max_steps,
            _context_window, // 保留用于未来的上下文压缩
        ) = (
            mc.provider.as_str().to_string(),
            mc.base_url.clone(),
            mc.api_key.clone(),
            mc.max_tokens,
            mc.timeout_secs,
            mc.wire_api.as_str().to_string(),
            mc.model.clone(),
            mc.display_name.clone(),
            mc.effective_system_role_name().to_string(),
            mc.max_steps,
            mc.context_window,
        );

        // ----- 创建 Provider -----
        let provider = crate::providers::create_provider(
            &provider_type,
            &base_url,
            &api_key,
            max_tokens,
            timeout,
            &wire_api,
        );

        // ----- 创建 ToolDispatcher -----
        // 子代理使用独立的 WorkspaceTools 和 WorkspaceToolSession（cap-std 沙箱）
        let workspace_tools = match WorkspaceTools::new(&self_arc.workspace_dir) {
            Ok(wt) => wt,
            Err(e) => {
                // 工作区工具创建失败，移除占位符后返回错误
                remove_placeholder_on_err!(
                    self_arc, &spawn_config.name,
                    anyhow::anyhow!("创建子代理工作区工具失败").context(e)
                );
            }
        };
        let dispatcher = dispatch::ToolDispatcher {
            tool_ctx: self_arc.tool_ctx.clone(),
            workspace_tools,
            workspace_session: WorkspaceToolSession::default(),
            config: self_arc.config.clone(),
            mcp_manager: self_arc.mcp_manager.clone(),
        };

        // ----- 构建子代理系统提示 -----
        // 父代理名称：如果是主代理(depth=0)调用则为 "main"，否则取调用者的实际名称
        let parent_name = if parent_depth == 0 {
            "main".to_string()
        } else {
            format!("depth-{}", parent_depth)
        };
        let system_prompt = prompt::build_subagent_system_prompt(
            &spawn_config.name,
            &spawn_config.task,
            &parent_name,
            &model_display,
            &self_arc.workspace_dir,
            spawn_config.context.as_deref(),
        );

        // ----- 为子代理定制 Finish 工具（I6 修复） -----
        // 子代理的 Finish 使用 result 字段（向父代理返回任务结果），
        // 而主代理的 Finish 使用 reason 字段（描述轮次结束原因）
        // 克隆工具规范列表并替换 Finish 的描述和参数
        let subagent_tool_specs = {
            let mut specs: Vec<ToolSpec> = (*self_arc.tool_specs).clone();
            // 查找并替换 Finish 工具的定义
            if let Some(finish_spec) = specs.iter_mut().find(|s| s.name == "Finish") {
                finish_spec.description = "完成当前任务并向父代理返回结果。\
                    当你已完成所有工作时，必须调用此工具。在 result 字段中详细描述你的工作成果。\
                    不调用 Finish 则任务不会结束。".to_string();
                finish_spec.parameters = serde_json::json!({
                    "type": "object",
                    "properties": {
                        "result": {
                            "type": "string",
                            "description": "任务执行结果的详细描述（将返回给父代理）"
                        }
                    },
                    "required": ["result"]
                });
            }
            Arc::new(specs)
        };

        // ----- 创建 SubAgentRunner -----
        let runner = runner::SubAgentRunner::new(
            provider,
            model_name,
            system_role_name,
            max_steps,
            dispatcher,
            subagent_tool_specs,
            system_prompt,
            spawn_config.task.clone(),
            rx,
            status.clone(),
            self_arc.notify_tx.clone(),
            self_arc.clone(),
            child_depth,
            spawn_config.name.clone(),
            model_display.clone(),
        );

        // ----- 启动 tokio 任务 -----
        let task_name = spawn_config.name.clone();
        let join_handle = tokio::spawn(async move {
            runner.run().await;
        });

        // ----- 更新占位符 AgentHandle -----
        // 用实际的 JoinHandle 和模型显示名称更新之前插入的占位符
        {
            let mut agents = self_arc.agents.lock().await;
            if let Some(handle) = agents.get_mut(&spawn_config.name) {
                handle.model_display = model_display.clone();
                handle.join_handle = Some(join_handle);
            }
        }

        tracing::info!(
            "已生成子代理 `{}` (model={}, depth={})",
            task_name, model_display, child_depth
        );

        Ok(format!(
            "已生成子代理 `{}`（模型: {model_display}，深度: {child_depth}）。\
             使用 CheckAgent 查看状态，WaitAgent 等待完成。",
            spawn_config.name
        ))
    }

    /// 向指定子代理发送消息
    ///
    /// 使用 async + lock().await 获取锁，与其他异步方法保持一致
    ///
    /// # 参数
    /// - `name`: 目标子代理名称
    /// - `msg`: 消息内容（将作为 user 消息注入子代理对话历史）
    pub async fn message_agent(&self, name: &str, msg: String) -> anyhow::Result<()> {
        // 使用 async lock().await 获取锁，与 check_agent/wait_agent 等方法一致
        let agents = self.agents.lock().await;

        // 查找目标子代理
        let handle = agents
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("子代理 '{}' 不存在", name))?;

        // 检查子代理是否仍在运行
        {
            let st = handle.status.lock().unwrap_or_else(|e| e.into_inner());
            if !matches!(*st, AgentStatus::Running) {
                bail!("子代理 '{}' 已不在运行中", name);
            }
        }

        // 发送消息
        handle.tx.send(AgentMessage::UserMessage(msg))
            .map_err(|_| anyhow::anyhow!("子代理 '{}' 的消息通道已关闭", name))?;

        Ok(())
    }

    /// 查询指定子代理的状态
    ///
    /// # 参数
    /// - `name`: 目标子代理名称
    ///
    /// # 返回
    /// AgentStatusReport，包含子代理名称、模型、状态和已运行时间
    pub async fn check_agent(&self, name: &str) -> anyhow::Result<AgentStatusReport> {
        let agents = self.agents.lock().await;

        // 查找目标子代理
        let handle = agents
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("子代理 '{}' 不存在", name))?;

        // 读取当前状态
        let status = handle.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // 计算已运行时间
        let elapsed = chrono::Local::now() - handle.created_at;

        Ok(AgentStatusReport {
            name: handle.name.clone(),
            model_display: handle.model_display.clone(),
            status,
            elapsed,
        })
    }

    /// 列出所有子代理的状态
    ///
    /// # 返回
    /// 所有子代理的 AgentStatusReport 列表
    pub async fn list_agents(&self) -> Vec<AgentStatusReport> {
        let agents = self.agents.lock().await;
        let now = chrono::Local::now();

        agents
            .values()
            .map(|handle| {
                // 读取状态
                let status = handle.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
                // 计算时间
                let elapsed = now - handle.created_at;
                AgentStatusReport {
                    name: handle.name.clone(),
                    model_display: handle.model_display.clone(),
                    status,
                    elapsed,
                }
            })
            .collect()
    }

    /// 等待指定子代理完成并返回结果
    ///
    /// 取出子代理的 JoinHandle 并 await 它
    /// 注意：同一个子代理只能 wait 一次（JoinHandle 被 take）
    ///
    /// # 参数
    /// - `name`: 目标子代理名称
    ///
    /// # 返回
    /// 子代理的完成结果字符串
    pub async fn wait_agent(&self, name: &str) -> anyhow::Result<String> {
        // 取出 JoinHandle（需要锁）
        let join_handle = {
            let mut agents = self.agents.lock().await;
            let handle = agents
                .get_mut(name)
                .ok_or_else(|| anyhow::anyhow!("子代理 '{}' 不存在", name))?;
            handle
                .join_handle
                .take()
                .ok_or_else(|| anyhow::anyhow!("子代理 '{}' 已经被等待过或已完成", name))?
        };
        // 释放锁后 await JoinHandle（避免持锁等待导致死锁）

        // 等待子代理任务完成
        join_handle.await
            .map_err(|e| anyhow::anyhow!("子代理 '{}' 任务 panic: {}", name, e))?;

        // 读取最终状态
        let agents = self.agents.lock().await;
        let handle = agents
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("子代理 '{}' 不存在（已在等待后被移除？）", name))?;
        let status = handle.status.lock().unwrap_or_else(|e| e.into_inner()).clone();

        match status {
            AgentStatus::Completed { result } => Ok(result),
            AgentStatus::Failed { error } => Ok(format!("[失败] {error}")),
            AgentStatus::Running => Ok("[异常] 任务已结束但状态仍为运行中".to_string()),
        }
    }

    /// 等待多个指定子代理完成
    ///
    /// 使用 tokio::task::JoinSet 并行等待所有 JoinHandle，
    /// 而非逐个顺序等待（修复 C2：避免一个慢子代理阻塞其他子代理的结果收集）
    ///
    /// # 参数
    /// - `names`: 子代理名称列表
    ///
    /// # 返回
    /// (名称, 结果) 元组列表
    pub async fn wait_agents(&self, names: &[String]) -> anyhow::Result<Vec<(String, String)>> {
        // 收集所有 JoinHandle（需要锁）
        let mut handles: Vec<(String, tokio::task::JoinHandle<()>)> = Vec::new();
        {
            let mut agents = self.agents.lock().await;
            for name in names {
                let handle = agents
                    .get_mut(name)
                    .ok_or_else(|| anyhow::anyhow!("子代理 '{}' 不存在", name))?;
                if let Some(jh) = handle.join_handle.take() {
                    handles.push((name.clone(), jh));
                }
                // 如果 join_handle 已经 None（已等待过），跳过
            }
        }
        // 释放锁后并行等待所有任务

        // 使用 JoinSet 并行等待所有 JoinHandle
        // JoinSet 会在内部并发调度所有 future，任何一个完成时立即返回
        let mut join_set = tokio::task::JoinSet::new();
        for (name, jh) in handles {
            // 将每个 JoinHandle 包装成返回名称的 future，加入 JoinSet
            join_set.spawn(async move {
                // 等待子代理任务完成（忽略 panic 错误，后续通过状态判断）
                if let Err(e) = jh.await {
                    tracing::error!("子代理 '{}' 任务 panic: {}", name, e);
                }
                name
            });
        }

        // 收集所有已完成子代理的结果
        let mut results = Vec::new();
        while let Some(join_result) = join_set.join_next().await {
            // JoinSet::join_next 返回 Result<T, JoinError>
            // 这里的 JoinError 是 JoinSet 内部 spawn 的 task panic，不太可能发生
            if let Ok(name) = join_result {
                // 读取最终状态
                let agents = self.agents.lock().await;
                if let Some(handle) = agents.get(&name) {
                    let status = handle.status.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let result = match status {
                        AgentStatus::Completed { result } => result,
                        AgentStatus::Failed { error } => format!("[失败] {error}"),
                        AgentStatus::Running => "[异常] 状态仍为运行中".to_string(),
                    };
                    results.push((name, result));
                }
            }
        }

        Ok(results)
    }

    /// 等待所有运行中的子代理完成
    ///
    /// # 返回
    /// (名称, 结果) 元组列表
    pub async fn wait_all(&self) -> anyhow::Result<Vec<(String, String)>> {
        // 收集所有运行中子代理的名称
        let names: Vec<String> = {
            let agents = self.agents.lock().await;
            agents
                .values()
                .filter(|h| {
                    matches!(*h.status.lock().unwrap_or_else(|e| e.into_inner()), AgentStatus::Running)
                        && h.join_handle.is_some()
                })
                .map(|h| h.name.clone())
                .collect()
        };

        if names.is_empty() {
            return Ok(Vec::new());
        }

        // 委托给 wait_agents
        self.wait_agents(&names).await
    }

    /// 向所有运行中的子代理发送关闭指令
    /// 在主代理退出时调用，确保所有子代理优雅关闭
    pub async fn shutdown_all(&self) {
        let agents = self.agents.lock().await;
        let mut shutdown_count = 0u32;

        for handle in agents.values() {
            // 只向运行中的子代理发送关闭指令
            let is_running = matches!(
                *handle.status.lock().unwrap_or_else(|e| e.into_inner()),
                AgentStatus::Running
            );
            if is_running {
                // 发送关闭消息（忽略发送失败——通道可能已关闭）
                let _ = handle.tx.send(AgentMessage::Shutdown);
                shutdown_count += 1;
            }
        }

        if shutdown_count > 0 {
            tracing::info!("已向 {} 个运行中的子代理发送关闭指令", shutdown_count);
        }
    }

    /// 获取当前运行中的子代理数量
    pub async fn running_count(&self) -> usize {
        let agents = self.agents.lock().await;
        agents
            .values()
            .filter(|h| {
                matches!(*h.status.lock().unwrap_or_else(|e| e.into_inner()), AgentStatus::Running)
            })
            .count()
    }
}
