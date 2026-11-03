//! 动态视图：`/messages`、`/inbox`、`/sessions`、`/friends`、`/groups`、`/config`。
//!
//! 设计依据：设计 §6.2 / §6.3 / §8.2 / §8.4 / §6.9。
//!
//! 视图不落盘：每次读取都从 JSONL、SQLite 索引或 NapCat 实时查询生成。
//! 读状态规则：内容进入上下文就算读过；`LS` / `Stat` 只看元信息，不标记。
//!
//! 查询参数目前支持一个保守子集（后续与 monitor 的 `when` 合并为同一套求值器）：
//! `state:unread|read|dismissed`、`from:<qq>`、`contains:<text>`、
//! `after:<YYYY-MM-DD>`、`before:<YYYY-MM-DD>`、`unread`、`mentions_self`。
//! 多个条件用空白分隔，按 AND 组合。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};

use crate::agent::message::{AgentMessage, AssistantBlock};
use crate::config::models::ModelsFile;
use crate::config::{LoadedConfig, SbotConfig};
use crate::qq::event::{ChatId, QqMessage};
use crate::qq::{QqClient, format};
use crate::security::gate::Gate;
use crate::session::SessionStore;
use crate::storage::Database;
use crate::storage::messages::{InboxState, MessageStore};
use crate::vfs::sqlite::VfsStore;
use crate::vfs::{NodeKind, Vfs, VfsError, VfsPath, path::ParsedTarget};

/// 视图条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewEntry {
    pub path: String,
    pub kind: &'static str,
    pub size: u64,
    pub mtime: String,
}

/// 视图读取结果。
#[derive(Debug, Clone)]
pub enum ViewRead {
    Text(String),
    Metadata {
        kind: &'static str,
        size: u64,
        mime: String,
    },
}

/// 配置写入后的热更新回调。
pub type HotReload = Arc<dyn Fn(&SbotConfig) + Send + Sync>;

/// 动态视图服务。
mod config;
mod contacts;
mod inbox;
mod messages;
mod sessions;
mod system;

pub struct Views {
    vfs: Arc<VfsStore>,
    messages: Arc<MessageStore>,
    sessions: Arc<SessionStore>,
    client: Option<Arc<QqClient>>,
    /// `/config` 与 `/system` 使用的运行时快照；未接入时为 `None`。
    context: ViewContext,
    /// 配置写入后的热更新回调（filter/queue 等内存副本）。
    hot_reload: std::sync::Mutex<Option<HotReload>>,
}

/// `/config` 与 `/system` 的只读上下文。
#[derive(Default, Clone)]
pub struct ViewContext {
    /// 当前配置；`SetModel` / `SetEffort` 会原地更新。
    pub config: Option<Arc<std::sync::RwLock<LoadedConfig>>>,
    pub models: Option<ModelsFile>,
    pub gate: Option<Arc<Gate>>,
    pub ai_enabled: Option<Arc<AtomicBool>>,
    pub estop_file: Option<PathBuf>,
    pub started_at: Option<DateTime<Utc>>,
    pub db: Option<Arc<Database>>,
}

impl Views {
    pub fn new(
        vfs: Arc<VfsStore>,
        messages: Arc<MessageStore>,
        sessions: Arc<SessionStore>,
        client: Option<Arc<QqClient>>,
    ) -> Self {
        Self {
            vfs,
            messages,
            sessions,
            client,
            context: ViewContext::default(),
            hot_reload: std::sync::Mutex::new(None),
        }
    }

    /// 接入配置热更新回调；写入成功后调用。
    pub fn set_hot_reload(&self, callback: HotReload) {
        *self.hot_reload.lock().expect("hot reload lock") = Some(callback);
    }

    /// 通知热更新；回调失败不影响写入结果。
    fn notify(&self, config: &SbotConfig) {
        let callback = self.hot_reload.lock().expect("hot reload lock").clone();
        if let Some(callback) = callback {
            callback(config);
        }
    }

    /// 接入 `/config` 与 `/system` 所需的运行时句柄。
    pub fn with_context(mut self, context: ViewContext) -> Self {
        self.context = context;
        self
    }

    /// 当前配置快照；未接入时返回 `None`。
    fn config_snapshot(&self) -> Option<SbotConfig> {
        self.context
            .config
            .as_ref()
            .map(|handle| handle.read().expect("config lock poisoned").config.clone())
    }

    /// 路径是否属于动态视图。
    pub fn handles(path: &VfsPath) -> bool {
        crate::vfs::path::is_dynamic(path)
    }

    /// 列出目录。
    pub async fn list(&self, target: &ParsedTarget) -> Result<Vec<ViewEntry>, VfsError> {
        let path = &target.path;
        let mut entries = match path.segments() {
            [root] if root == "messages" => self.list_messages_root(),
            [root] if root == "inbox" => self.list_inbox_root(),
            [root] if root == "sessions" => self.list_sessions_root(),
            [root] if root == "friends" => self.list_friends().await,
            [root] if root == "groups" => self.list_groups().await,
            [root, scope] if root == "messages" => self.list_message_scope(scope),
            [root, scope, peer] if root == "messages" => self.list_message_peer(scope, peer),
            [root, scope, peer, month] if root == "messages" => {
                self.list_message_month(scope, peer, month)
            }
            [root, chat] if root == "inbox" => self.list_inbox_chat(chat),
            [root, scope, peer] if root == "friends" => self.list_friend(scope, peer).await,
            [root, group] if root == "groups" => self.list_group(group).await,
            [root, group, members] if root == "groups" && members == "members" => {
                self.list_group_members(group).await
            }
            [root] if root == "config" => Ok(self.list_config_root()),
            [root, section] if root == "config" => Ok(self.list_config_section(section)),
            [root, _, _] if root == "config" => Ok(Vec::new()),
            [root] if root == "system" => Ok(self.list_system()),
            _ => self.vfs_list_fallback(path),
        }?;

        // 过滤作用于条目路径与元信息；列表不标记读。
        if let Some(query) = &target.query
            && let Some(filter) = &query.filter
        {
            entries.retain(|entry| filter_matches_path(filter, &entry.path));
            entries.truncate(query.limit);
        }
        Ok(entries)
    }

    /// 读取一个动态文件；内容进入上下文即标记已读。
    pub async fn read(&self, target: &ParsedTarget) -> Result<ViewRead, VfsError> {
        self.read_internal(target, true).await
    }

    /// 读取但不标记已读；`Grep` 与 `Stat` 用它。
    ///
    /// 工具描述承诺“只扫描的文件不算读过”；搜索时把候选文件整体读一遍
    /// 不能悄悄把未读清空。
    pub async fn peek(&self, target: &ParsedTarget) -> Result<ViewRead, VfsError> {
        self.read_internal(target, false).await
    }

    async fn read_internal(
        &self,
        target: &ParsedTarget,
        mark_read: bool,
    ) -> Result<ViewRead, VfsError> {
        let path = &target.path;
        let filter = target.query.as_ref().and_then(|query| query.filter.clone());
        let limit = target.query.as_ref().map(|query| query.limit).unwrap_or(50);

        match path.segments() {
            [root] if root == "messages" => self.read_messages_digest(filter.as_deref(), limit),
            [root] if root == "sessions" => self.read_sessions_digest(),
            [root, scope, peer, month, day] if root == "messages" && day.ends_with(".md") => {
                let day = day.trim_end_matches(".md");
                self.read_message_day(scope, peer, month, day, filter.as_deref(), limit, mark_read)
            }
            [root] if root == "inbox" => self.read_inbox_root(filter.as_deref(), limit, mark_read),
            [root, chat] if root == "inbox" => {
                self.read_inbox_chat(chat, filter.as_deref(), limit, mark_read)
            }
            [root, chat, id] if root == "inbox" && id.ends_with(".md") => {
                if id == "index.md" {
                    self.read_inbox_chat(chat, filter.as_deref(), limit, mark_read)
                } else {
                    let message_id: i64 = id
                        .trim_end_matches(".md")
                        .parse()
                        .map_err(|_| VfsError::NotFound(path.to_string()))?;
                    self.read_inbox_message(chat, message_id, mark_read)
                }
            }
            [root, file] if root == "sessions" => {
                if file == "index.jsonl" {
                    self.read_sessions_index()
                } else {
                    self.read_session(file)
                }
            }
            [root, scope, peer] if root == "friends" => self.read_friend_file(scope, peer).await,
            [root, group, member] if root == "groups" && member.ends_with(".md") => {
                self.read_group_member(group, member.trim_end_matches(".md"))
                    .await
            }
            [root, group, file] if root == "groups" => self.read_group_file(group, file).await,
            [root] if root == "config.toml" => self.read_config_toml(),
            [root] if root == "config" => self.read_config_digest(),
            [root, file] if root == "config" && file == "models.json" => self.read_models_json(),
            [root, section] if root == "config" => self.read_config_section(section),
            [root, section, key] if root == "config" => self.read_config_value(section, key),
            [root] if root == "system" => self.read_system("status"),
            [root, section] if root == "system" => self.read_system(section),
            _ => Err(VfsError::NotFound(path.to_string())),
        }
    }

    /// 元信息（不标记读）。
    pub async fn stat(&self, path: &VfsPath) -> Result<ViewEntry, VfsError> {
        let target = ParsedTarget {
            path: path.clone(),
            query: None,
        };
        if let Some(found) = self
            .list(&target)
            .await?
            .into_iter()
            .find(|entry| entry.path == path.to_string())
        {
            return Ok(found);
        }
        // 文件本身不在父目录的列举结果里时，构造一个条目。
        match self.peek(&target).await {
            Ok(ViewRead::Text(text)) => Ok(ViewEntry {
                path: path.to_string(),
                kind: "text",
                size: text.len() as u64,
                mtime: String::new(),
            }),
            Ok(ViewRead::Metadata { kind, size, .. }) => Ok(ViewEntry {
                path: path.to_string(),
                kind,
                size,
                mtime: String::new(),
            }),
            Err(error) => Err(error),
        }
    }

    /// 递归遍历（Glob / Grep 使用）。
    pub async fn walk(&self, path: &VfsPath) -> Result<Vec<ViewEntry>, VfsError> {
        let mut out = Vec::new();
        let mut stack = vec![path.clone()];
        while let Some(current) = stack.pop() {
            let target = ParsedTarget {
                path: current,
                query: None,
            };
            for entry in self.list(&target).await? {
                if let Ok(child) = VfsPath::parse(&entry.path)
                    && entry.kind == "dir"
                {
                    stack.push(child);
                }
                out.push(entry);
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// `/inbox` 的忽略语义：`dismiss` 不删除消息本体。
    pub fn dismiss(&self, target: &ParsedTarget) -> Result<usize, VfsError> {
        let path = &target.path;
        match path.segments() {
            [root, chat] if root == "inbox" => {
                let chat = parse_chat_segment(chat)?;
                let ids: Vec<i64> = self
                    .messages
                    .inbox_for_chat(chat, InboxState::Unread)
                    .map_err(storage)?
                    .iter()
                    .map(|message| message.message_id)
                    .collect();
                self.messages.dismiss(chat, &ids).map_err(storage)
            }
            [root, chat, id] if root == "inbox" && id.ends_with(".md") => {
                let chat = parse_chat_segment(chat)?;
                if id == "index.md" {
                    let ids: Vec<i64> = self
                        .messages
                        .inbox_for_chat(chat, InboxState::Unread)
                        .map_err(storage)?
                        .iter()
                        .map(|message| message.message_id)
                        .collect();
                    self.messages.dismiss(chat, &ids).map_err(storage)
                } else {
                    let message_id: i64 = id
                        .trim_end_matches(".md")
                        .parse()
                        .map_err(|_| VfsError::NotFound(path.to_string()))?;
                    self.messages.dismiss(chat, &[message_id]).map_err(storage)
                }
            }
            [root] if root == "inbox" => {
                let Some(filter) = target
                    .query
                    .as_ref()
                    .and_then(|query| query.filter.as_ref())
                else {
                    return Err(VfsError::Storage(
                        "RM /inbox requires a filter; refusing to dismiss everything".to_owned(),
                    ));
                };
                let mut total = 0;
                for message in self.messages.unread(None).map_err(storage)? {
                    if filter_matches_message(filter, &message, Some(InboxState::Unread)) {
                        total += self
                            .messages
                            .dismiss(message.chat, &[message.message_id])
                            .map_err(storage)?;
                    }
                }
                Ok(total)
            }
            _ => Err(VfsError::NotFound(path.to_string())),
        }
    }

    /// 动态路径中的 grep：只返回匹配，并把匹配条目记为已读。
    pub async fn grep(
        &self,
        root: &VfsPath,
        regex: &regex::Regex,
        file_ext: Option<&str>,
    ) -> Result<Vec<(String, u64, String)>, VfsError> {
        let mut matches = Vec::new();
        let entries = self.walk(root).await?;
        for entry in entries {
            if entry.kind == "dir" {
                continue;
            }
            if let Some(ext) = file_ext
                && !entry.path.ends_with(&format!(".{ext}"))
            {
                continue;
            }
            let Ok(path) = VfsPath::parse(&entry.path) else {
                continue;
            };
            let target = ParsedTarget {
                path: path.clone(),
                query: None,
            };
            let Ok(ViewRead::Text(text)) = self.peek(&target).await else {
                continue;
            };
            for (line_no, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    matches.push((entry.path.clone(), line_no as u64 + 1, line.to_owned()));
                }
            }
        }
        Ok(matches)
    }

    // ---------------------------------------------------------------------
    // /messages
    // ---------------------------------------------------------------------

    fn vfs_list_fallback(&self, path: &VfsPath) -> Result<Vec<ViewEntry>, VfsError> {
        let mut out = Vec::new();
        for meta in self.vfs.list(path)? {
            out.push(ViewEntry {
                path: meta.path.to_string(),
                kind: match meta.kind {
                    NodeKind::Dir => "dir",
                    _ => "file",
                },
                size: meta.size,
                mtime: meta.updated_at,
            });
        }
        Ok(out)
    }
}

fn dir_entry(path: &str) -> ViewEntry {
    ViewEntry {
        path: path.to_owned(),
        kind: "dir",
        size: 0,
        mtime: String::new(),
    }
}

fn text_entry(path: &str) -> ViewEntry {
    ViewEntry {
        path: path.to_owned(),
        kind: "text",
        size: 0,
        mtime: String::new(),
    }
}

fn normalize_scope(scope: &str) -> Result<&'static str, VfsError> {
    match scope {
        "groups" => Ok("groups"),
        "private" => Ok("private"),
        _ => Err(VfsError::NotFound(format!("/messages/{scope}"))),
    }
}

fn chat_from(scope: &str, peer: &str) -> Result<ChatId, VfsError> {
    let peer: u64 = peer
        .parse()
        .map_err(|_| VfsError::NotFound(format!("/messages/{scope}/{peer}")))?;
    Ok(if scope == "groups" {
        ChatId::Group(peer)
    } else {
        ChatId::Private(peer)
    })
}

fn parse_chat_segment(segment: &str) -> Result<ChatId, VfsError> {
    ChatId::parse(segment).ok_or_else(|| VfsError::NotFound(format!("/inbox/{segment}")))
}

fn storage(error: anyhow::Error) -> VfsError {
    VfsError::Storage(error.to_string())
}

fn render_agent_message(message: &AgentMessage) -> String {
    match message {
        AgentMessage::System { text } => format!("[system]\n{text}"),
        AgentMessage::User { text } => format!("[user]\n{text}"),
        AgentMessage::Assistant(assistant) => {
            let mut out = String::from("[assistant]");
            for block in &assistant.content {
                match block {
                    AssistantBlock::Text { text } => {
                        out.push('\n');
                        out.push_str(text);
                    }
                    AssistantBlock::Thinking { text, .. } => {
                        out.push_str("\n[thinking] ");
                        out.push_str(text);
                    }
                    AssistantBlock::ToolCall(call) => {
                        out.push_str(&format!("\n[tool_call {} {}]", call.name, call.arguments));
                    }
                    AssistantBlock::ServerToolUse { name, input, .. } => {
                        out.push_str(&format!("\n[server_tool {name} {input}]"));
                    }
                    AssistantBlock::ServerToolResult { content, .. } => {
                        out.push_str(&format!("\n[server_result {content}]"));
                    }
                }
            }
            out
        }
        AgentMessage::Tool(tool) => format!(
            "[tool {} {}]\n{}",
            tool.tool_name,
            if tool.is_error { "error" } else { "ok" },
            tool.content
        ),
    }
}

/// 视图过滤器的保守子集。
fn filter_matches_message(filter: &str, message: &QqMessage, state: Option<InboxState>) -> bool {
    let text = message.plain_text();
    for term in filter.split_whitespace() {
        if term == "unread" {
            if state != Some(InboxState::Unread) {
                return false;
            }
            continue;
        }
        if term == "mentions_self" {
            if !message.segments.iter().any(|segment| {
                matches!(segment, crate::qq::event::Segment::At { qq } if qq.parse::<u64>().is_ok())
            }) {
                return false;
            }
            continue;
        }
        if let Some(value) = term.strip_prefix("from:") {
            if message.sender.qq.map(|qq| qq.to_string()).as_deref() != Some(value) {
                return false;
            }
            continue;
        }
        if let Some(value) = term.strip_prefix("state:") {
            let want = InboxState::parse(value);
            if want != state {
                return false;
            }
            continue;
        }
        if let Some(value) = term.strip_prefix("contains:") {
            let value = value.trim_matches('"');
            if !text.contains(value) {
                return false;
            }
            continue;
        }
        if let Some(value) = term.strip_prefix("after:") {
            let date = message
                .time
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string();
            if date.as_str() < value {
                return false;
            }
            continue;
        }
        if let Some(value) = term.strip_prefix("before:") {
            let date = message
                .time
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string();
            if date.as_str() > value {
                return false;
            }
            continue;
        }
        // 未知条件按不匹配处理，避免把全部内容当成命中。
        return false;
    }
    true
}

fn filter_matches_path(filter: &str, path: &str) -> bool {
    filter
        .split_whitespace()
        .filter_map(|term| term.strip_prefix("contains:"))
        .all(|value| path.contains(value.trim_matches('"')))
}

/// 判断工具是否应该走动态视图。
pub fn is_dynamic_target(target: &ParsedTarget) -> bool {
    target.query.is_some() || Views::handles(&target.path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SbotConfig;
    use crate::storage::crypto::MasterKey;
    use crate::vfs::quota::Limits;

    fn views() -> (Views, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([5u8; 32]));
        let vfs = Arc::new(
            VfsStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                Limits::default(),
            )
            .unwrap(),
        );
        let messages = Arc::new(
            MessageStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                crate::config::EncryptionMode::None,
            )
            .unwrap(),
        );
        let sessions = Arc::new(
            SessionStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                crate::config::EncryptionMode::None,
            )
            .unwrap(),
        );
        let config = SbotConfig::default();
        let path = dir.path().join("sbot.toml");
        std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        let handle = Arc::new(std::sync::RwLock::new(LoadedConfig {
            config,
            path,
            dir: dir.path().to_path_buf(),
        }));
        let models = ModelsFile::parse(include_str!("../../assets/models.default.json")).unwrap();
        let views = Views::new(vfs, messages, sessions, None).with_context(ViewContext {
            config: Some(handle),
            models: Some(models),
            ..ViewContext::default()
        });
        (views, dir)
    }

    fn message(chat: crate::qq::event::ChatId, id: i64, text: &str) -> crate::qq::event::QqMessage {
        crate::qq::event::QqMessage {
            message_id: id,
            chat,
            sender: crate::qq::event::Sender {
                qq: Some(10001),
                nickname: "Alice".to_owned(),
                card: String::new(),
                role: "member".to_owned(),
            },
            time: chrono::Utc::now(),
            segments: vec![crate::qq::event::Segment::Text {
                text: text.to_owned(),
            }],
            reply_to: None,
            is_self: false,
            backfilled: false,
            raw: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn searching_inbox_does_not_clear_unread() {
        let (views, _dir) = views();
        let chat = crate::qq::event::ChatId::Group(42);
        views
            .messages
            .append(&message(chat, 1, "hello there"))
            .unwrap();
        assert_eq!(views.messages.unread_counts().unwrap(), vec![(chat, 1)]);

        let regex = regex::Regex::new("zzz-no-match").unwrap();
        let matches = views
            .grep(&VfsPath::parse("/inbox").unwrap(), &regex, None)
            .await
            .unwrap();
        assert!(matches.is_empty());
        // 扫描不等于读过。
        assert_eq!(views.messages.unread_counts().unwrap(), vec![(chat, 1)]);

        // 真正的 Read 才标记已读。
        let target = target("/inbox/group:42");
        assert!(matches!(
            views.read(&target).await.unwrap(),
            ViewRead::Text(_)
        ));
        assert!(views.messages.unread_counts().unwrap().is_empty());
    }

    fn target(path: &str) -> ParsedTarget {
        ParsedTarget {
            path: VfsPath::parse(path).unwrap(),
            query: None,
        }
    }

    #[tokio::test]
    async fn config_tree_and_leaves_are_readable() {
        let (views, _dir) = views();
        let sections = views.list(&target("/config")).await.unwrap();
        assert!(sections.iter().any(|entry| entry.path == "/config/agent"));
        assert!(
            sections
                .iter()
                .any(|entry| entry.path == "/config/models.json")
        );

        let keys = views.list(&target("/config/agent")).await.unwrap();
        assert!(keys.iter().any(|entry| entry.path == "/config/agent/model"));

        match views.read(&target("/config/agent/model")).await.unwrap() {
            ViewRead::Text(text) => assert!(text.contains("claude-opus-5-5")),
            other => panic!("expected text, got {other:?}"),
        }

        // 整文件视图返回原始 TOML（不含凭据）。
        match views.read(&target("/config.toml")).await.unwrap() {
            ViewRead::Text(text) => assert!(text.contains("version = 1")),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn system_view_reports_runtime_defaults() {
        let (views, _dir) = views();
        match views.read(&target("/system/status")).await.unwrap() {
            ViewRead::Text(text) => {
                assert!(text.contains("mode: StandIn"), "{text}");
                assert!(text.contains("session_scope: Shared"), "{text}");
            }
            other => panic!("expected text, got {other:?}"),
        }
        match views.read(&target("/system/context")).await.unwrap() {
            ViewRead::Text(text) => assert!(text.contains("static_prompt_tokens")),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn config_writes_are_validated_and_whitelisted() {
        let (views, _dir) = views();
        // 可写键：写入后立即反映在视图与配置句柄里。
        views
            .write_config(&target("/config/agent/thinking_level"), "\"low\"")
            .unwrap();
        match views
            .read(&target("/config/agent/thinking_level"))
            .await
            .unwrap()
        {
            ViewRead::Text(text) => assert_eq!(text.trim(), "\"low\""),
            other => panic!("expected text, got {other:?}"),
        }
        let text = views.write_config(&target("/config.toml"), "").unwrap_err();
        // 空文档首先触发只读键对比（缺少受保护键），同样被拒绝。
        assert!(text.to_string().contains("read-only"), "{text}");

        // 只读键：叶子写入被拒绝。
        let error = views
            .write_config(
                &target("/config/agent/model"),
                "\"anthropic/claude-haiku-5-5\"",
            )
            .unwrap_err();
        assert!(error.to_string().contains("read-only"), "{error}");

        // 整文件写入：修改只读键整体拒绝。
        let current = match views.read(&target("/config.toml")).await.unwrap() {
            ViewRead::Text(text) => text,
            other => panic!("expected text, got {other:?}"),
        };
        assert!(
            current.contains("compaction_model"),
            "leaf write dropped sibling keys:\n{current}"
        );
        let altered = current.replace("path = \"./data\"", "path = \"./other-data\"");
        assert_ne!(altered, current);
        let error = views
            .write_config(&target("/config.toml"), &altered)
            .unwrap_err();
        assert!(error.to_string().contains("read-only"), "{error}");

        // 整文件写入：允许修改白名单键。
        let allowed = current.replace("thinking_level = \"low\"", "thinking_level = \"high\"");
        views
            .write_config(&target("/config.toml"), &allowed)
            .unwrap();
        match views
            .read(&target("/config/agent/thinking_level"))
            .await
            .unwrap()
        {
            ViewRead::Text(text) => assert_eq!(text.trim(), "\"high\""),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn config_writes_notify_hot_reload() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (views, _dir) = views();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        views.set_hot_reload(Arc::new(move |_config| {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        views
            .write_config(&target("/config/agent/thinking_level"), "\"low\"")
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
