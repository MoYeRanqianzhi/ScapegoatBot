// src/tools/mod.rs
// 工具模块
// 将 NapCatLink API 映射为 LLM 可调用的内置工具
// 在工具执行层进行白名单检查
// 通过 cap-std 限制文件工具在 workspace 内

pub mod message;
pub mod query;
pub mod skill;
pub mod social;
pub mod workspace;

use crate::skills::SkillRegistry;
use crate::whitelist::Whitelist;
use napcat_link::OneBotApi;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

// ============================================================================
// 工具执行上下文
// ============================================================================

/// 工具执行上下文
/// 包含所有工具执行所需的共享资源
/// 通过 Arc 在多个异步任务间共享
#[derive(Clone)]
pub struct ToolContext {
    /// NapCatLink API 聚合器
    pub api: OneBotApi,
    /// 白名单检查器
    pub whitelist: Arc<Whitelist>,
    /// Agent 发送的消息 ID 集合
    /// 用于 master=self 时过滤：收到自己 QQ 发的消息时，
    /// 如果 message_id 在此集合中说明是 agent 发的，不是主人亲自发的
    pub agent_sent_ids: Arc<Mutex<HashSet<i64>>>,
    /// Skill 注册表（通过 cap-std 沙箱保护每个 skill 目录）
    pub skills: Arc<SkillRegistry>,
}

impl ToolContext {
    /// 创建工具执行上下文
    pub fn new(
        api: OneBotApi,
        whitelist: Whitelist,
        skills: SkillRegistry,
    ) -> Self {
        Self {
            api,
            whitelist: Arc::new(whitelist),
            agent_sent_ids: Arc::new(Mutex::new(HashSet::new())),
            skills: Arc::new(skills),
        }
    }

    /// 记录一个由 agent 发送的消息 ID
    pub fn record_agent_sent(&self, message_id: i64) {
        if let Ok(mut ids) = self.agent_sent_ids.lock() {
            ids.insert(message_id);
        }
    }

    /// 检查消息是否由 agent 发送
    pub fn is_agent_sent(&self, message_id: i64) -> bool {
        self.agent_sent_ids
            .lock()
            .map(|ids| ids.contains(&message_id))
            .unwrap_or(false)
    }
}

// ============================================================================
// 工具执行结果
// ============================================================================

/// 工具执行结果
/// 包含成功时的文本输出或失败时的错误消息
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// 是否成功
    pub success: bool,
    /// 输出内容（成功时为结果描述，失败时为错误消息）
    pub output: String,
}

impl ToolResult {
    /// 创建成功结果
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            success: true,
            output: output.into(),
        }
    }

    /// 创建失败结果
    pub fn err(output: impl Into<String>) -> Self {
        Self {
            success: false,
            output: output.into(),
        }
    }
}
