use super::*;

/// per_chat 的会话与历史缓存。
///
/// 这些状态原本散在 `Runner` 的五个字段里；收进一个结构后，`Runner` 只保留
/// 真正的运行时句柄，`clear_all_context` 与测试也只需面对一个入口。
pub(super) struct ScopeCache {
    pub(super) per_chat: bool,
    sessions: Mutex<HashMap<String, Arc<SessionStore>>>,
    pub(super) histories: Mutex<HashMap<String, Arc<Mutex<Vec<AgentMessage>>>>>,
}

impl ScopeCache {
    pub(super) fn new(per_chat: bool) -> Self {
        Self {
            per_chat,
            sessions: Mutex::new(HashMap::new()),
            histories: Mutex::new(HashMap::new()),
        }
    }

    async fn session_for(&self, scope: &str, base: &Arc<SessionStore>) -> Arc<SessionStore> {
        let mut sessions = self.sessions.lock().await;
        if let Some(store) = sessions.get(scope) {
            return Arc::clone(store);
        }
        let store = Arc::new(base.for_scope(scope));
        sessions.insert(scope.to_owned(), Arc::clone(&store));
        store
    }

    async fn history_for(
        &self,
        scope: &str,
        base: &Arc<SessionStore>,
    ) -> Arc<Mutex<Vec<AgentMessage>>> {
        let mut histories = self.histories.lock().await;
        if let Some(history) = histories.get(scope) {
            return Arc::clone(history);
        }
        let sessions = Arc::clone(base);
        let sessions = sessions.for_scope(scope);
        let messages = sessions
            .current()
            .and_then(|current| sessions.read_segment(&current.file))
            .unwrap_or_default();
        let history = Arc::new(Mutex::new(messages));
        histories.insert(scope.to_owned(), Arc::clone(&history));
        history
    }

    pub(super) async fn all_sessions(&self) -> Vec<Arc<SessionStore>> {
        self.sessions.lock().await.values().cloned().collect()
    }

    pub(super) async fn clear_histories(&self) {
        for history in self.histories.lock().await.values() {
            history.lock().await.clear();
        }
    }
}

impl Runner {
    /// 共享历史句柄（`agent.*` 操作使用）。
    pub fn history(&self) -> Arc<Mutex<Vec<AgentMessage>>> {
        Arc::clone(&self.history)
    }

    /// chat → transcript 作用域名。
    fn chat_scope(chat: ChatId) -> String {
        match chat {
            ChatId::Group(id) => format!("group-{id}"),
            ChatId::Private(id) => format!("private-{id}"),
        }
    }

    /// 当前回合归属的作用域；shared 模式统一记 `shared`。
    pub(super) fn scope_name(&self, chat: ChatId) -> String {
        if self.scopes.per_chat {
            Self::chat_scope(chat)
        } else {
            "shared".to_owned()
        }
    }

    pub(super) async fn turn_lock_for(&self, chat: ChatId) -> Arc<tokio::sync::Mutex<()>> {
        let scope = self.scope_name(chat);
        let mut locks = self.turn_locks.lock().await;
        Arc::clone(
            locks
                .entry(scope)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    /// 取该 chat 的分段存储；shared 模式返回全局存储。
    pub(super) async fn session_for(&self, chat: ChatId) -> Arc<SessionStore> {
        if !self.scopes.per_chat {
            return Arc::clone(&self.sessions);
        }
        self.scopes
            .session_for(&Self::chat_scope(chat), &self.sessions)
            .await
    }

    /// 取该 chat 的历史句柄；首次访问时从对应分段载入。
    pub(super) async fn history_for(&self, chat: ChatId) -> Arc<Mutex<Vec<AgentMessage>>> {
        if !self.scopes.per_chat {
            return Arc::clone(&self.history);
        }
        self.scopes
            .history_for(&Self::chat_scope(chat), &self.sessions)
            .await
    }
}
