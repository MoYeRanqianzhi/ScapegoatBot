// src/agent_state.rs
// AI Agent 运行状态管理
// 使用 AtomicBool 控制 AI 是否活跃
// stop 时消息收集/存储/风格解析继续，仅暂停 LLM 调用

use std::sync::atomic::{AtomicBool, Ordering};

/// AI Agent 全局运行状态
/// 通过原子布尔值控制 AI Agent 的开关，线程安全
#[derive(Debug)]
pub struct AgentState {
    /// AI Agent 是否活跃（true = 运行中，false = 已暂停）
    active: AtomicBool,
}

impl AgentState {
    /// 创建新状态
    /// `active` 为 true 表示 AI Agent 默认启动
    pub fn new(active: bool) -> Self {
        Self {
            active: AtomicBool::new(active),
        }
    }

    /// 检查 AI Agent 是否活跃
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// 启动 AI Agent
    pub fn activate(&self) {
        self.active.store(true, Ordering::Relaxed);
    }

    /// 暂停 AI Agent（消息收集继续，仅停止 LLM 调用）
    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Relaxed);
    }
}
