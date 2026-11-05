use super::*;

impl Runner {
    /// 全局历史的句柄（供 runtime 注入与复盘）。
    pub(crate) fn history(&self) -> Arc<Mutex<Vec<AgentMessage>>> {
        Arc::clone(&self.history)
    }

    /// 单会话：所有回合共用一把锁。
    pub(super) async fn turn_lock_for(&self, _chat: ChatId) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(&self.turn_lock)
    }

    /// 单会话：总是返回全局 transcript。
    pub(super) async fn session_for(&self, _chat: ChatId) -> Arc<SessionStore> {
        Arc::clone(&self.deps.sessions)
    }

    /// 单会话：总是返回全局历史。
    pub(super) async fn history_for(&self, _chat: ChatId) -> Arc<Mutex<Vec<AgentMessage>>> {
        Arc::clone(&self.history)
    }
}
