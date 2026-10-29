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

/// 动态视图服务。
pub struct Views {
    vfs: Arc<VfsStore>,
    messages: Arc<MessageStore>,
    sessions: Arc<SessionStore>,
    client: Option<Arc<QqClient>>,
    /// `/config` 与 `/system` 使用的运行时快照；未接入时为 `None`。
    context: ViewContext,
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
        let path = &target.path;
        let filter = target.query.as_ref().and_then(|query| query.filter.clone());
        let limit = target.query.as_ref().map(|query| query.limit).unwrap_or(50);

        match path.segments() {
            [root] if root == "messages" => self.read_messages_digest(filter.as_deref(), limit),
            [root] if root == "inbox" => self.read_inbox_root(filter.as_deref(), limit),
            [root] if root == "sessions" => self.read_sessions_digest(),
            [root, scope, peer, month, day] if root == "messages" && day.ends_with(".md") => {
                let day = day.trim_end_matches(".md");
                self.read_message_day(scope, peer, month, day, filter.as_deref(), limit)
            }
            [root, chat] if root == "inbox" => self.read_inbox_chat(chat, filter.as_deref(), limit),
            [root, chat, id] if root == "inbox" && id.ends_with(".md") => {
                if id == "index.md" {
                    self.read_inbox_chat(chat, filter.as_deref(), limit)
                } else {
                    let message_id: i64 = id
                        .trim_end_matches(".md")
                        .parse()
                        .map_err(|_| VfsError::NotFound(path.to_string()))?;
                    self.read_inbox_message(chat, message_id)
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
        match self.read(&target).await {
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
            let Ok(ViewRead::Text(text)) = self.read(&target).await else {
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

    fn list_messages_root(&self) -> Result<Vec<ViewEntry>, VfsError> {
        Ok(vec![
            dir_entry("/messages/groups"),
            dir_entry("/messages/private"),
        ])
    }

    fn list_message_scope(&self, scope: &str) -> Result<Vec<ViewEntry>, VfsError> {
        let scope = normalize_scope(scope)?;
        let mut entries = Vec::new();
        for peer in self.messages.peers(scope).map_err(storage)? {
            entries.push(dir_entry(&format!("/messages/{scope}/{peer}")));
        }
        Ok(entries)
    }

    fn list_message_peer(&self, scope: &str, peer: &str) -> Result<Vec<ViewEntry>, VfsError> {
        let scope = normalize_scope(scope)?;
        let chat = chat_from(scope, peer)?;
        let mut entries = Vec::new();
        for month in self.messages.months(chat).map_err(storage)? {
            entries.push(dir_entry(&format!("/messages/{scope}/{peer}/{month}")));
        }
        Ok(entries)
    }

    fn list_message_month(
        &self,
        scope: &str,
        peer: &str,
        month: &str,
    ) -> Result<Vec<ViewEntry>, VfsError> {
        let scope = normalize_scope(scope)?;
        let chat = chat_from(scope, peer)?;
        let mut entries = Vec::new();
        for day in self.messages.days(chat, month).map_err(storage)? {
            let dd = day.rsplit('-').next().unwrap_or(&day);
            entries.push(text_entry(&format!(
                "/messages/{scope}/{peer}/{month}/{dd}.md"
            )));
        }
        Ok(entries)
    }

    fn read_messages_digest(
        &self,
        _filter: Option<&str>,
        _limit: usize,
    ) -> Result<ViewRead, VfsError> {
        let counts = self.messages.unread_counts().map_err(storage)?;
        let mut out = String::from("# Unread messages\n");
        for (chat, count) in counts {
            out.push_str(&format!("- {chat}: {count} unread\n"));
        }
        if out.trim_end() == "# Unread messages" {
            out.push_str("- nothing unread\n");
        }
        Ok(ViewRead::Text(out))
    }

    fn read_message_day(
        &self,
        scope: &str,
        peer: &str,
        month: &str,
        day: &str,
        filter: Option<&str>,
        limit: usize,
    ) -> Result<ViewRead, VfsError> {
        let scope = normalize_scope(scope)?;
        let chat = chat_from(scope, peer)?;
        let date = format!("{month}-{day:0>2}");
        let messages = self
            .messages
            .messages_for_day(chat, &date)
            .map_err(storage)?;
        let mut selected = Vec::new();
        for message in messages {
            if filter.is_none_or(|filter| filter_matches_message(filter, &message, None)) {
                selected.push(message);
            }
        }
        selected.truncate(limit);
        let ids: Vec<i64> = selected.iter().map(|message| message.message_id).collect();
        self.messages.mark_read(chat, &ids).map_err(storage)?;
        Ok(ViewRead::Text(
            selected
                .iter()
                .map(format::render_inbound)
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }

    // ---------------------------------------------------------------------
    // /inbox
    // ---------------------------------------------------------------------

    fn list_inbox_root(&self) -> Result<Vec<ViewEntry>, VfsError> {
        let mut entries = Vec::new();
        for (chat, count) in self.messages.unread_counts().map_err(storage)? {
            entries.push(ViewEntry {
                path: format!("/inbox/{chat}"),
                kind: "dir",
                size: count,
                mtime: String::new(),
            });
        }
        Ok(entries)
    }

    fn list_inbox_chat(&self, chat: &str) -> Result<Vec<ViewEntry>, VfsError> {
        let chat = parse_chat_segment(chat)?;
        let mut entries = vec![text_entry(&format!("/inbox/group:{}/index.md", chat.id()))];
        if !chat.is_group() {
            entries[0] = text_entry(&format!("/inbox/private:{}/index.md", chat.id()));
        }
        let prefix = if chat.is_group() { "group" } else { "private" };
        for message in self
            .messages
            .inbox_for_chat(chat, InboxState::Unread)
            .map_err(storage)?
        {
            entries.push(text_entry(&format!(
                "/inbox/{prefix}:{}/{}.md",
                chat.id(),
                message.message_id
            )));
        }
        Ok(entries)
    }

    fn read_inbox_root(&self, filter: Option<&str>, limit: usize) -> Result<ViewRead, VfsError> {
        let mut out = String::from("# Inbox\n");
        let mut rendered = 0usize;
        for message in self.messages.unread(None).map_err(storage)? {
            if rendered >= limit {
                break;
            }
            if filter.is_some_and(|filter| {
                !filter_matches_message(filter, &message, Some(InboxState::Unread))
            }) {
                continue;
            }
            out.push_str(&format::render_inbound(&message));
            out.push_str("\n\n");
            // 内容进入上下文即已读。
            self.messages
                .mark_read(message.chat, &[message.message_id])
                .map_err(storage)?;
            rendered += 1;
        }
        if rendered == 0 {
            out.push_str("Nothing unread.\n");
        }
        Ok(ViewRead::Text(out))
    }

    fn read_inbox_chat(
        &self,
        chat: &str,
        filter: Option<&str>,
        limit: usize,
    ) -> Result<ViewRead, VfsError> {
        let chat = parse_chat_segment(chat)?;
        let messages = self
            .messages
            .inbox_for_chat(chat, InboxState::Unread)
            .map_err(storage)?;
        let mut out = format!("# Inbox: {chat}\n");
        let mut selected = Vec::new();
        for message in messages {
            if selected.len() >= limit {
                break;
            }
            if filter.is_some_and(|filter| {
                !filter_matches_message(filter, &message, Some(InboxState::Unread))
            }) {
                continue;
            }
            selected.push(message.message_id);
            out.push_str(&format::render_inbound(&message));
            out.push_str("\n\n");
        }
        if selected.is_empty() {
            out.push_str("Nothing unread.\n");
        }
        self.messages.mark_read(chat, &selected).map_err(storage)?;
        Ok(ViewRead::Text(out))
    }

    fn read_inbox_message(&self, chat: &str, message_id: i64) -> Result<ViewRead, VfsError> {
        let chat = parse_chat_segment(chat)?;
        let message = self
            .messages
            .load(chat, message_id)
            .map_err(storage)?
            .ok_or_else(|| VfsError::NotFound(format!("/inbox/{chat}/{message_id}.md")))?;
        self.messages
            .mark_read(chat, &[message_id])
            .map_err(storage)?;
        Ok(ViewRead::Text(format::render_inbound(&message)))
    }

    // ---------------------------------------------------------------------
    // /sessions
    // ---------------------------------------------------------------------

    fn list_sessions_root(&self) -> Result<Vec<ViewEntry>, VfsError> {
        let mut entries = vec![text_entry("/sessions/index.jsonl")];
        for file in self.sessions.list().map_err(storage)? {
            entries.push(ViewEntry {
                path: format!("/sessions/{}", file.file),
                kind: "text",
                size: file.bytes,
                mtime: file.started_at,
            });
        }
        Ok(entries)
    }

    fn read_sessions_digest(&self) -> Result<ViewRead, VfsError> {
        let list = self.sessions.list().map_err(storage)?;
        let mut out = String::from("# Sessions\n");
        for file in list {
            out.push_str(&format!(
                "- /sessions/{} ({} messages, {} bytes{})\n",
                file.file,
                file.message_count,
                file.bytes,
                if file.ended_at.is_some() {
                    ", closed"
                } else {
                    ", current"
                }
            ));
        }
        Ok(ViewRead::Text(out))
    }

    fn read_sessions_index(&self) -> Result<ViewRead, VfsError> {
        let list = self.sessions.list().map_err(storage)?;
        let mut out = String::new();
        for file in list {
            let line = serde_json::json!({
                "file": file.file,
                "prev": file.prev_file,
                "started_at": file.started_at,
                "ended_at": file.ended_at,
                "message_count": file.message_count,
                "bytes": file.bytes,
                "pruned": self.sessions.info(&file.file).map(|info| info.is_none()).unwrap_or(false),
            });
            out.push_str(&line.to_string());
            out.push('\n');
        }
        Ok(ViewRead::Text(out))
    }

    fn read_session(&self, file: &str) -> Result<ViewRead, VfsError> {
        let messages = self.sessions.read_segment(file).map_err(storage)?;
        Ok(ViewRead::Text(
            messages
                .iter()
                .map(render_agent_message)
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }

    // ---------------------------------------------------------------------
    // /friends /groups（离线时实时文件报错，notes 仍可读写）
    // ---------------------------------------------------------------------

    async fn list_friends(&self) -> Result<Vec<ViewEntry>, VfsError> {
        let Some(client) = &self.client else {
            // 离线：仍然列出 VFS 里已有的 notes 目录。
            return self.vfs_list_fallback(&VfsPath::parse("/friends").expect("valid"));
        };
        let response = client
            .friend_list()
            .await
            .map_err(|error| VfsError::Offline(error.to_string()))?;
        let list = response
            .get("data")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(list
            .iter()
            .filter_map(|friend| friend.get("user_id").and_then(serde_json::Value::as_u64))
            .map(|qq| dir_entry(&format!("/friends/{qq}")))
            .collect())
    }

    async fn list_groups(&self) -> Result<Vec<ViewEntry>, VfsError> {
        let Some(client) = &self.client else {
            return self.vfs_list_fallback(&VfsPath::parse("/groups").expect("valid"));
        };
        let response = client
            .group_list()
            .await
            .map_err(|error| VfsError::Offline(error.to_string()))?;
        let list = response
            .get("data")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(list
            .iter()
            .filter_map(|group| group.get("group_id").and_then(serde_json::Value::as_u64))
            .map(|gid| dir_entry(&format!("/groups/{gid}")))
            .collect())
    }

    async fn list_friend(&self, _scope: &str, peer: &str) -> Result<Vec<ViewEntry>, VfsError> {
        Ok(vec![
            text_entry(&format!("/friends/{peer}/profile.md")),
            text_entry(&format!("/friends/{peer}/info.json")),
            text_entry(&format!("/friends/{peer}/notes.md")),
        ])
    }

    async fn list_group(&self, group: &str) -> Result<Vec<ViewEntry>, VfsError> {
        Ok(vec![
            text_entry(&format!("/groups/{group}/profile.md")),
            text_entry(&format!("/groups/{group}/info.json")),
            dir_entry(&format!("/groups/{group}/members")),
            text_entry(&format!("/groups/{group}/notes.md")),
        ])
    }

    async fn list_group_members(&self, group: &str) -> Result<Vec<ViewEntry>, VfsError> {
        let Some(client) = &self.client else {
            return Err(VfsError::Offline("/groups members".to_owned()));
        };
        let response = client
            .call(
                "get_group_member_list",
                serde_json::json!({"group_id": group.parse::<u64>().unwrap_or(0)}),
            )
            .await
            .map_err(|error| VfsError::Offline(error.to_string()))?;
        let list = response
            .get("data")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(list
            .iter()
            .filter_map(|member| member.get("user_id").and_then(serde_json::Value::as_u64))
            .map(|qq| text_entry(&format!("/groups/{group}/members/{qq}.md")))
            .collect())
    }

    async fn read_friend_file(&self, _scope: &str, peer: &str) -> Result<ViewRead, VfsError> {
        let path = VfsPath::parse(&format!("/friends/{peer}"))?;
        let Some(client) = &self.client else {
            return Err(VfsError::Offline(format!("/friends/{peer}")));
        };
        let response = client
            .friend_list()
            .await
            .map_err(|error| VfsError::Offline(error.to_string()))?;
        let list = response
            .get("data")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let friend = list
            .iter()
            .find(|item| {
                item.get("user_id")
                    .and_then(serde_json::Value::as_u64)
                    .map(|id| id.to_string())
                    .as_deref()
                    == Some(peer)
            })
            .ok_or_else(|| VfsError::NotFound(path.to_string()))?;
        Ok(ViewRead::Text(format!(
            "# Friend {peer}\n\n```json\n{}\n```\n",
            serde_json::to_string_pretty(friend).unwrap_or_default()
        )))
    }

    async fn read_group_file(&self, group: &str, file: &str) -> Result<ViewRead, VfsError> {
        let _ = file;
        let Some(client) = &self.client else {
            return Err(VfsError::Offline(format!("/groups/{group}")));
        };
        let response = client
            .group_list()
            .await
            .map_err(|error| VfsError::Offline(error.to_string()))?;
        let list = response
            .get("data")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let found = list.iter().find(|item| {
            item.get("group_id")
                .and_then(serde_json::Value::as_u64)
                .map(|id| id.to_string())
                .as_deref()
                == Some(group)
        });
        let group = found.ok_or_else(|| VfsError::NotFound(format!("/groups/{group}")))?;
        Ok(ViewRead::Text(format!(
            "# Group {group}\n\n```json\n{}\n```\n",
            serde_json::to_string_pretty(group).unwrap_or_default()
        )))
    }

    async fn read_group_member(&self, group: &str, member: &str) -> Result<ViewRead, VfsError> {
        let Some(client) = &self.client else {
            return Err(VfsError::Offline(format!("/groups/{group}/members")));
        };
        let response = client
            .call(
                "get_group_member_info",
                serde_json::json!({
                    "group_id": group.parse::<u64>().unwrap_or(0),
                    "user_id": member.parse::<u64>().unwrap_or(0),
                }),
            )
            .await
            .map_err(|error| VfsError::Offline(error.to_string()))?;
        Ok(ViewRead::Text(format!(
            "# Member {member} of {group}\n\n```json\n{}\n```\n",
            serde_json::to_string_pretty(response.get("data").unwrap_or(&response))
                .unwrap_or_default()
        )))
    }

    // ---------------------------------------------------------------------
    // /config /system
    // ---------------------------------------------------------------------

    /// 把当前配置序列化成 TOML 表；失败时视为视图不可用。
    fn config_table(&self) -> Option<toml::Table> {
        let config = self.config_snapshot()?;
        let text = toml::to_string(&config).ok()?;
        toml::from_str(&text).ok()
    }

    fn list_config_root(&self) -> Vec<ViewEntry> {
        let mut entries: Vec<ViewEntry> = self
            .config_table()
            .map(|table| {
                table
                    .keys()
                    .map(|name| text_entry(&format!("/config/{name}")))
                    .collect()
            })
            .unwrap_or_default();
        entries.push(text_entry("/config/models.json"));
        entries
    }

    fn list_config_section(&self, section: &str) -> Vec<ViewEntry> {
        if section == "models.json" {
            return Vec::new();
        }
        let Some(table) = self.config_table() else {
            return Vec::new();
        };
        let Some(value) = table.get(section) else {
            return Vec::new();
        };
        // 数组（如 [[monitor]]）作为叶子整体读取。
        let Some(section_table) = value.as_table() else {
            return Vec::new();
        };
        section_table
            .keys()
            .map(|key| text_entry(&format!("/config/{section}/{key}")))
            .collect()
    }

    fn read_config_digest(&self) -> Result<ViewRead, VfsError> {
        let Some(table) = self.config_table() else {
            return Err(VfsError::Offline("/config".to_owned()));
        };
        let mut out = String::from("# Configuration\n");
        for (name, value) in &table {
            let kind = if value.is_table() { "table" } else { "leaf" };
            out.push_str(&format!("- /config/{name} ({kind})\n"));
        }
        out.push_str("- /config/models.json (read-only)\n");
        Ok(ViewRead::Text(out))
    }

    fn read_config_section(&self, section: &str) -> Result<ViewRead, VfsError> {
        let table = self
            .config_table()
            .ok_or_else(|| VfsError::Offline("/config".to_owned()))?;
        let value = table
            .get(section)
            .ok_or_else(|| VfsError::NotFound(format!("/config/{section}")))?;
        let text = match value {
            toml::Value::Table(_) => toml::to_string(value)
                .unwrap_or_else(|_| value.to_string()),
            _ => value.to_string(),
        };
        Ok(ViewRead::Text(text))
    }

    fn read_config_value(&self, section: &str, key: &str) -> Result<ViewRead, VfsError> {
        let table = self
            .config_table()
            .ok_or_else(|| VfsError::Offline("/config".to_owned()))?;
        let value = table
            .get(section)
            .and_then(|section| section.get(key))
            .ok_or_else(|| VfsError::NotFound(format!("/config/{section}/{key}")))?;
        Ok(ViewRead::Text(value.to_string()))
    }

    /// `/config.toml`：`sbot.toml` 原文；凭据不在其中。
    fn read_config_toml(&self) -> Result<ViewRead, VfsError> {
        let path = self
            .context
            .config
            .as_ref()
            .map(|handle| handle.read().expect("config lock poisoned").path.clone())
            .ok_or_else(|| VfsError::Offline("/config.toml".to_owned()))?;
        let text = std::fs::read_to_string(&path)
            .map_err(|error| VfsError::Storage(error.to_string()))?;
        Ok(ViewRead::Text(text))
    }

    /// `/config/models.json`：只读模型定义。
    fn read_models_json(&self) -> Result<ViewRead, VfsError> {
        let models = self
            .context
            .models
            .as_ref()
            .ok_or_else(|| VfsError::Offline("/config/models.json".to_owned()))?;
        Ok(ViewRead::Text(
            serde_json::to_string_pretty(models).unwrap_or_default(),
        ))
    }

    fn list_system(&self) -> Vec<ViewEntry> {
        ["status", "storage", "security", "gate", "hooks", "context"]
            .into_iter()
            .map(|name| text_entry(&format!("/system/{name}")))
            .collect()
    }

    fn read_system(&self, section: &str) -> Result<ViewRead, VfsError> {
        let config = self
            .config_snapshot()
            .ok_or_else(|| VfsError::Offline("/system".to_owned()))?;
        let text = match section {
            "status" => {
                let mut out = String::from("# Runtime\n");
                out.push_str(&format!("mode: {:?}\n", config.agent.mode));
                out.push_str(&format!("session_scope: {:?}\n", config.session.scope));
                out.push_str(&format!("model: {}\n", config.agent.model));
                out.push_str(&format!("thinking_level: {}\n", config.agent.thinking_level));
                if let Some(enabled) = &self.context.ai_enabled {
                    out.push_str(&format!(
                        "ai_enabled: {}\n",
                        enabled.load(Ordering::Relaxed)
                    ));
                }
                if let Some(gate) = &self.context.gate {
                    out.push_str(&format!("hold: {}\n", gate.hold_active()));
                }
                if let Some(estop) = &self.context.estop_file {
                    out.push_str(&format!("estop: {}\n", estop.exists()));
                }
                if let Some(started) = self.context.started_at {
                    out.push_str(&format!("started_at: {}\n", started.to_rfc3339()));
                    out.push_str(&format!(
                        "uptime_secs: {}\n",
                        (Utc::now() - started).num_seconds().max(0)
                    ));
                }
                out
            }
            "storage" => {
                let mut out = String::from("# Storage\n");
                if let Ok(usage) = self.vfs.usage() {
                    out.push_str(&format!(
                        "vfs: {} active bytes, {} trash bytes, {} nodes, {} trashed\n",
                        usage.active_bytes, usage.trash_bytes, usage.nodes, usage.trash_nodes
                    ));
                }
                if let Ok((bytes, count)) = self.messages.usage() {
                    out.push_str(&format!("messages: {bytes} bytes, {count} messages\n"));
                }
                if let Ok((bytes, count)) = self.sessions.usage() {
                    out.push_str(&format!("sessions: {bytes} bytes, {count} messages\n"));
                }
                out.push_str(&format!(
                    "retention: messages {} days, sessions {} days\n",
                    config.storage.message_retention_days, config.storage.session_retention_days
                ));
                out
            }
            "security" => {
                let mut out = format!("# Security\n\nhold: {}\n", self
                    .context
                    .gate
                    .as_ref()
                    .map(|gate| gate.hold_active().to_string())
                    .unwrap_or_else(|| "unknown".to_owned()));
                out.push_str(&format!(
                    "guard: enabled={} mode={:?} backend={:?} threshold={}\n",
                    config.security.guard.enabled,
                    config.security.guard.mode,
                    config.security.guard.backend,
                    config.security.guard.threshold
                ));
                if let Some(db) = &self.context.db {
                    if let Ok(events) = crate::security::events::recent(db, 10) {
                        out.push_str("\n## Recent events\n");
                        for event in events {
                            out.push_str(&format!(
                                "- {} {:?} allowed={} score={:.2} {}\n",
                                event.created_at,
                                event.checkpoint,
                                event.allowed,
                                event.score,
                                event.reason
                            ));
                        }
                    }
                }
                out
            }
            "gate" => format!(
                "# API gate\n\nmode: {:?}\nreceive_scopes: {:?}\nsend_scopes: {:?}\n",
                config.gate.mode, config.agent.receive_scopes, config.agent.send_scopes
            ),
            "hooks" => {
                let mut out = String::from("# Context hooks\n");
                for hook in &config.hooks.context {
                    out.push_str(&format!(
                        "- {} path={} kind={:?} inject={:?} enabled={}\n",
                        hook.name, hook.path, hook.kind, hook.inject, hook.enabled
                    ));
                }
                out
            }
            "context" => {
                let static_prompt = crate::prompt::render_static(config.agent.mode);
                format!(
                    "# Context budget\n\nstatic_prompt_tokens≈{}\nturn_context_budget: {}\nkeep_recent_tokens: {}\nmax_output_tokens: {}\n",
                    crate::session::compaction::estimate_tokens(&static_prompt),
                    config.compaction.turn_context_budget,
                    config.compaction.keep_recent_tokens,
                    config.agent.max_output_tokens
                )
            }
            _ => return Err(VfsError::NotFound(format!("/system/{section}"))),
        };
        Ok(ViewRead::Text(text))
    }

    // ---------------------------------------------------------------------
    // 兜底
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
}
