// src/swarm/types.rs
// 蜂群子代理系统的类型定义
// 定义子代理的生成配置、运行状态、消息通道和完成通知等核心数据结构
//
// 这些类型在 SwarmManager、SubAgentRunner 和蜂群工具之间共享，
// 构成子代理系统的数据层基础

// ============================================================================
// 导入
// ============================================================================

use std::sync::Arc;      // 原子引用计数，跨线程共享所有权
use tokio::sync::mpsc;   // 异步多生产者单消费者通道

// ============================================================================
// 子代理生成配置
// ============================================================================

/// 子代理生成配置
/// 由 SpawnAgent 工具传入，描述要创建的子代理的基本参数
/// SwarmManager 根据此配置创建 Provider、构建 prompt、启动 SubAgentRunner
pub struct SpawnConfig {
    /// 子代理名称（在蜂群中必须唯一，用于消息路由和状态查询）
    pub name: String,
    /// 任务描述（作为首条 user 消息注入子代理的对话历史）
    pub task: String,
    /// 备选模型名称（可选，引用 config.toml 中 [[models]] 的 name 字段）
    /// None 表示使用主 [llm] 配置的模型
    pub model_name: Option<String>,
    /// 额外上下文（可选，注入子代理系统提示中帮助理解任务背景）
    pub context: Option<String>,
}

// ============================================================================
// 子代理状态
// ============================================================================

/// 子代理运行状态枚举
/// 通过 Arc<std::sync::Mutex<AgentStatus>> 在 SwarmManager 和 SubAgentRunner 之间共享
/// 使用 std::sync::Mutex 而非 tokio::sync::Mutex，因为状态更新是瞬时操作不需要 await
#[derive(Debug, Clone)]
pub enum AgentStatus {
    /// 运行中：子代理正在执行 agentic 循环
    Running,
    /// 已完成：子代理调用了 Finish 工具，result 为输出内容
    Completed {
        /// 子代理的完成结果（Finish 工具的 result 参数值）
        result: String,
    },
    /// 已失败：子代理因错误退出，error 为错误描述
    Failed {
        /// 错误描述信息
        error: String,
    },
}

// ============================================================================
// 子代理状态报告
// ============================================================================

/// 子代理状态报告
/// 由 SwarmManager 的 check_agent() 和 list_agents() 方法返回
/// 包含子代理的名称、模型、当前状态和已运行时间
pub struct AgentStatusReport {
    /// 子代理名称
    pub name: String,
    /// 子代理使用的模型显示名称（如 "DeepSeek V3"）
    pub model_display: String,
    /// 当前运行状态的快照
    pub status: AgentStatus,
    /// 从创建到现在的已运行时长
    pub elapsed: chrono::Duration,
}

// ============================================================================
// 子代理句柄
// ============================================================================

/// 子代理句柄
/// 由 SwarmManager 管理，存储在 agents HashMap 中
/// 持有子代理的元数据、状态引用、消息发送端和 tokio 任务句柄
pub struct AgentHandle {
    /// 子代理名称（与 HashMap 的 key 一致）
    pub name: String,
    /// 子代理使用的模型显示名称
    pub model_display: String,
    /// 子代理创建时间（本地时区）
    pub created_at: chrono::DateTime<chrono::Local>,
    /// 子代理运行状态的共享引用
    /// 通过 Arc<std::sync::Mutex<>> 实现 SwarmManager 和 SubAgentRunner 的双向同步
    pub status: Arc<std::sync::Mutex<AgentStatus>>,
    /// 向子代理发送消息的通道发送端
    /// SubAgentRunner 在 agentic 循环中通过 try_recv 非阻塞接收
    pub tx: mpsc::UnboundedSender<AgentMessage>,
    /// tokio 任务句柄（Option 因为 wait_agent 会 take 它进行 await）
    pub join_handle: Option<tokio::task::JoinHandle<()>>,
}

// ============================================================================
// 父 → 子消息
// ============================================================================

/// 父代理发送给子代理的消息
/// 通过 mpsc::UnboundedSender/Receiver 异步传递
/// 子代理在 agentic 循环的每次迭代开始时通过 try_recv 非阻塞检查
#[derive(Debug, Clone)]
pub enum AgentMessage {
    /// 用户消息：注入子代理对话历史作为新的 user 消息
    UserMessage(String),
    /// 关闭指令：请求子代理优雅退出
    Shutdown,
}

// ============================================================================
// 子代理完成通知
// ============================================================================

/// 子代理完成通知（子代理 → 主代理）
/// SubAgentRunner 在退出时通过 notify_tx 发送给 SwarmManager 的通知通道
/// AgentRunner 通过 swarm_rx 接收并注入到主代理的对话历史中
#[derive(Debug, Clone)]
pub struct SwarmNotification {
    /// 完成的子代理名称
    pub agent_name: String,
    /// 子代理使用的模型显示名称
    pub model_display: String,
    /// 子代理的输出结果（成功时为 Finish 的 result，失败时为错误描述）
    pub result: String,
    /// 是否成功完成（true = Completed，false = Failed）
    pub success: bool,
}
