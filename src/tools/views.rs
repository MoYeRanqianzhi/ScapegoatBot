//! 动态视图：`/messages`、`/inbox`、`/sessions`、`/friends`、`/groups`、`/config`。
//!
//! 设计依据：设计 §6.2 / §6.3 / §8.2 / §8.4 / §6.9。
//!
//! 视图不落盘：每次读取都从 JSONL、SQLite 索引或 NapCat 实时查询生成。
//! 读状态规则：内容进入上下文就算读过；`LS` / `Stat` 只看元信息，不标记。
//!
//! 查询串（`?filter=…&limit=…&offset=…&order=…`）：
//! - `Read` 在 `/inbox`、`/inbox/<chat>`、`/messages` 与 `/messages/.../<DD>.md` 上接受；
//!   其余单文档视图带查询串直接报错，不悄悄忽略；
//! - `LS` 在任何动态目录上接受，`filter` 只认 `contains:`（比较条目路径）。
//!
//! `filter` 的语法在 [`crate::vfs::filter`]，求值与分页在 `views/filter.rs`。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};

use crate::config::models::ModelsFile;
use crate::config::{LoadedConfig, SbotConfig};
use crate::qq::event::{ChatId, QqMessage};
use crate::qq::format;
use crate::security::gate::Gate;
use crate::session::SessionStore;
use crate::storage::Database;
use crate::storage::messages::{InboxState, MessageStore};
use crate::transcript::{AgentMessage, AssistantBlock};
use crate::vfs::path::{ParsedTarget, ViewQuery};
use crate::vfs::sqlite::VfsStore;
use crate::vfs::{NodeKind, PathError, Vfs, VfsError, VfsPath};

pub(crate) use filter::Page;
pub(crate) use inbox::Dismissed;

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

// 各视图的读写实现，由下面 `Views` 的 `list` / `read` / `write` 路由调用。
mod config;
mod contacts;
mod filter;
mod inbox;
mod messages;
mod sessions;
mod system;

/// 动态视图服务。
pub struct Views {
    vfs: Arc<VfsStore>,
    messages: Arc<MessageStore>,
    sessions: Arc<SessionStore>,
    /// NapCat 网关（生产是 `QqClient`）；`/friends`、`/groups` 用它实时查询。
    client: Option<Arc<dyn crate::tools::api::NapcatGateway>>,
    /// `/config` 与 `/system` 使用的运行时快照；未接入时为 `None`。
    context: ViewContext,
    /// 配置写入后的热更新回调（filter/queue 等内存副本）。
    hot_reload: std::sync::Mutex<Option<HotReload>>,
}

/// `/config` 与 `/system` 的只读上下文。
#[derive(Clone)]
pub struct ViewContext {
    /// 当前配置；`SetModel` / `SetEffort` 会原地更新。
    pub config: Option<Arc<std::sync::RwLock<LoadedConfig>>>,
    pub models: Option<ModelsFile>,
    pub gate: Option<Arc<Gate>>,
    pub ai_enabled: Option<Arc<AtomicBool>>,
    pub estop_file: Option<PathBuf>,
    pub started_at: Option<DateTime<Utc>>,
    pub db: Option<Arc<Database>>,
    pub key: Option<Arc<crate::storage::crypto::MasterKey>>,
    /// 允许读取的 chat；`["*"]` = 全部，空 = 拒绝一切。
    pub receive_scopes: Vec<String>,
    /// 本账号 QQ；`filter` 的 `mentions_self` 用它判断“@ 的是不是自己”。
    pub self_id: u64,
}

impl Default for ViewContext {
    fn default() -> Self {
        Self {
            config: None,
            models: None,
            gate: None,
            ai_enabled: None,
            estop_file: None,
            started_at: None,
            db: None,
            key: None,
            receive_scopes: vec!["*".to_owned()],
            self_id: 0,
        }
    }
}

impl Views {
    pub(crate) fn new(
        vfs: Arc<VfsStore>,
        messages: Arc<MessageStore>,
        sessions: Arc<SessionStore>,
        client: Option<Arc<dyn crate::tools::api::NapcatGateway>>,
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
    pub(crate) fn set_hot_reload(&self, callback: HotReload) {
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
    /// 换一个 NapCat 网关（测试用假网关）。
    #[cfg(test)]
    pub(crate) fn with_gateway(
        mut self,
        client: Arc<dyn crate::tools::api::NapcatGateway>,
    ) -> Self {
        self.client = Some(client);
        self
    }

    pub(crate) fn with_context(mut self, context: ViewContext) -> Self {
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
    pub(crate) fn handles(path: &VfsPath) -> bool {
        crate::vfs::path::is_dynamic(path)
    }

    /// 列出目录（不标记读）。
    ///
    /// 带查询串时按 `filter`（只认 `contains:`）、`order`、`offset`、`limit` 截取；
    /// 不带时给出全部条目。
    pub(crate) async fn list(&self, target: &ParsedTarget) -> Result<Page<ViewEntry>, VfsError> {
        check_listing_query(target.query.as_ref())?;
        let entries = self.list_all(&target.path).await?;
        Ok(page_entries(entries, target.query.as_ref()))
    }

    /// 递归列出（`LS` 的 `recursive`）；查询串的语义与 [`Self::list`] 相同，作用于整棵树。
    pub(crate) async fn list_tree(
        &self,
        target: &ParsedTarget,
    ) -> Result<Page<ViewEntry>, VfsError> {
        check_listing_query(target.query.as_ref())?;
        let entries = self.walk(&target.path).await?;
        Ok(page_entries(entries, target.query.as_ref()))
    }

    /// 一个目录的全部条目，按路由分派。
    async fn list_all(&self, path: &VfsPath) -> Result<Vec<ViewEntry>, VfsError> {
        match path.segments() {
            [root] if root == "messages" => self.list_messages_root(),
            [root] if root == "inbox" => self.list_inbox_root(),
            [root] if root == "sessions" => self.list_sessions_root(),
            [root] if root == "friends" => self.list_friends().await,
            [root] if root == "groups" => self.list_groups().await,
            [root, group] if root == "groups" => self.list_group_members(group).await,
            [root, scope] if root == "messages" => self.list_message_scope(scope),
            [root, scope, peer] if root == "messages" => self.list_message_peer(scope, peer),
            [root, scope, peer, month] if root == "messages" => {
                self.list_message_month(scope, peer, month)
            }
            [root, chat] if root == "inbox" => self.list_inbox_chat(chat),
            [root] if root == "config" => Ok(self.list_config_root()),
            [root, section] if root == "config" => Ok(self.list_config_section(section)),
            [root, _, _] if root == "config" => Ok(Vec::new()),
            [root] if root == "system" => Ok(self.list_system()),
            // 整文件配置视图是文件，不是目录。
            [root] if root == "config.toml" => Err(VfsError::NotADirectory(path.to_string())),
            _ => self.vfs_list_fallback(path),
        }
    }

    /// 读取一个动态文件；内容进入上下文即标记已读。
    ///
    /// 不带查询串时按默认参数（`limit` 50）取第一页；接受查询串的视图见模块文档。
    pub(crate) async fn read(&self, target: &ParsedTarget) -> Result<ViewRead, VfsError> {
        let query = match &target.query {
            Some(_) if !accepts_read_query(&target.path) => {
                return Err(PathError::QueryNotSupported(target.path.to_string()).into());
            }
            Some(query) => query.clone(),
            None => ViewQuery::default(),
        };
        self.read_internal(&target.path, &query, true).await
    }

    /// 完整读取但不标记已读、不分页；`Grep` 与 `Stat` 用它。
    ///
    /// 工具描述承诺“只扫描的文件不算读过”：搜索时把候选文件整体读一遍不能悄悄把未读
    /// 清空；也不能只搜第一页，否则一天里第 51 条之后的消息永远搜不到。
    pub(crate) async fn scan(&self, path: &VfsPath) -> Result<ViewRead, VfsError> {
        self.read_internal(path, &ViewQuery::unbounded(), false)
            .await
    }

    async fn read_internal(
        &self,
        path: &VfsPath,
        query: &ViewQuery,
        mark_read: bool,
    ) -> Result<ViewRead, VfsError> {
        match path.segments() {
            [root] if root == "messages" => self.read_messages_digest(query),
            [root] if root == "sessions" => self.read_sessions_digest(),
            [root, scope, peer, month, day] if root == "messages" && day.ends_with(".md") => {
                let chat = chat_from(normalize_scope(scope)?, peer)?;
                let date = format!("{month}-{:0>2}", day.trim_end_matches(".md"));
                self.read_message_day(path, chat, &date, query, mark_read)
            }
            [root] if root == "inbox" => self.read_inbox(None, query, mark_read),
            [root, chat] if root == "inbox" => self.read_inbox(Some(chat), query, mark_read),
            [root, chat, id] if root == "inbox" && id == "index.md" => {
                self.read_inbox(Some(chat), query, mark_read)
            }
            [root, chat, id] if root == "inbox" && id.ends_with(".md") => {
                let message_id = parse_message_file(path, id)?;
                self.read_inbox_message(chat, message_id, mark_read)
            }
            [root, file] if root == "sessions" => {
                if file == "index.jsonl" {
                    self.read_sessions_index()
                } else {
                    self.read_session(file)
                }
            }
            [root, file] if root == "friends" => self.read_friend(file).await,
            [root, file] if root == "groups" => self.read_group(file).await,
            [root, group, file] if root == "groups" => self.read_group_member(group, file).await,
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
    ///
    /// 依次尝试：父目录列举里的条目（带未读数等元信息）；视图根本身（能列举就是目录）；
    /// 最后完整读一遍取大小（父目录不列出的文件，例如已读过的单条消息）。
    pub(crate) async fn stat(&self, path: &VfsPath) -> Result<ViewEntry, VfsError> {
        let wanted = path.to_string();
        // 父目录本身也是动态视图时才去列举它；`/inbox` 的父目录是根，不在此列。
        if let Some(parent) = path.parent().filter(Self::handles)
            && let Ok(entries) = self.list_all(&parent).await
            && let Some(found) = entries.into_iter().find(|entry| entry.path == wanted)
        {
            return Ok(found);
        }
        if path.segments().len() == 1 {
            match self.list_all(path).await {
                Ok(children) => {
                    return Ok(ViewEntry {
                        path: wanted,
                        kind: "dir",
                        size: children.len() as u64,
                        mtime: String::new(),
                    });
                }
                // `/config.toml` 是文件：继续按文件处理。
                Err(VfsError::NotADirectory(_)) => {}
                // 离线、越界等错误原样交出，不伪装成“不存在”。
                Err(error) => return Err(error),
            }
        }
        let (kind, size) = match self.scan(path).await? {
            ViewRead::Text(text) => ("text", text.len() as u64),
            ViewRead::Metadata { kind, size, .. } => (kind, size),
        };
        Ok(ViewEntry {
            path: wanted,
            kind,
            size,
            mtime: String::new(),
        })
    }

    /// 递归遍历（Glob / Grep 使用）。
    pub(crate) async fn walk(&self, path: &VfsPath) -> Result<Vec<ViewEntry>, VfsError> {
        let mut out = Vec::new();
        let mut stack = vec![path.clone()];
        while let Some(current) = stack.pop() {
            for entry in self.list_all(&current).await? {
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

    #[cfg(test)]
    fn messages_store(&self) -> Arc<MessageStore> {
        Arc::clone(&self.messages)
    }

    /// 当前 `[agent].receive_scopes` 是否允许读取该 chat。
    pub(super) fn receive_allows(&self, chat: ChatId) -> bool {
        let chat = chat.to_string();
        self.context
            .receive_scopes
            .iter()
            .any(|scope| scope == "*" || scope == &chat)
    }

    /// 越界读取按 NotFound 处理，不泄露该 chat 是否存在。
    pub(super) fn ensure_receive(&self, chat: ChatId) -> Result<(), VfsError> {
        if self.receive_allows(chat) {
            Ok(())
        } else {
            Err(VfsError::NotFound(format!(
                "{chat}: outside receive_scopes"
            )))
        }
    }

    /// `/inbox` 的忽略语义：条目离开收件箱、不标记已读，消息本体不删除。
    ///
    /// - `/inbox/<chat>/<id>.md`：一条；不接受查询串。
    /// - `/inbox/<chat>`、`/inbox/<chat>/index.md`：不带查询串时是该 chat 的全部未读，
    ///   带查询串时与 `Read` 同样筛选、分页，只处理这一页；
    /// - `/inbox`：必须带 `filter`，跨全部 chat，同样只处理一页。
    pub(crate) fn dismiss(&self, target: &ParsedTarget) -> Result<Dismissed, VfsError> {
        let path = &target.path;
        match path.segments() {
            [root, chat] if root == "inbox" => self.dismiss_chat(chat, target.query.as_ref()),
            [root, chat, file] if root == "inbox" && file == "index.md" => {
                self.dismiss_chat(chat, target.query.as_ref())
            }
            [root, chat, id] if root == "inbox" && id.ends_with(".md") => {
                if target.query.is_some() {
                    return Err(PathError::QueryNotSupported(path.to_string()).into());
                }
                let chat = parse_chat_segment(chat)?;
                self.ensure_receive(chat)?;
                self.dismiss_one(chat, parse_message_file(path, id)?)
            }
            [root] if root == "inbox" => {
                let Some(query) = target.query.as_ref().filter(|query| query.filter.is_some())
                else {
                    return Err(VfsError::Storage(
                        "RM /inbox requires a filter; refusing to dismiss everything".to_owned(),
                    ));
                };
                self.dismiss_unread(None, query)
            }
            _ => Err(VfsError::NotFound(path.to_string())),
        }
    }

    /// 动态路径中的 grep：只返回匹配，不标记已读（内容没有进入上下文）。
    ///
    /// 与静态树同一套预算：`root` 的每个直接子节点下最多扫描 `max_files` 个文件，总共最多
    /// 返回 `max_matches` 行。到了预算的子节点记进返回的第二项（`<路径> (file budget)` /
    /// `<路径> (match budget)`），之前找到的匹配照常返回。没有预算的话，一次对 `/messages`
    /// 的搜索会解密并扫描整个历史，所有匹配都留在内存里。
    ///
    /// 每个子节点下从路径最大的文件扫起：`/messages` 与 `/sessions` 的路径按日期排，路径
    /// 最大的就是最新的，预算不够时留下的是近期的内容。
    pub(crate) async fn grep(
        &self,
        root: &VfsPath,
        regex: &regex::Regex,
        file_ext: Option<&str>,
        max_files: usize,
        max_matches: usize,
    ) -> Result<(Vec<(String, u64, String)>, Vec<String>), VfsError> {
        let mut matches = Vec::new();
        let mut skipped = Vec::new();
        // 列不出子节点时，`root` 可能本身就是一个文件：读得出来就只搜它自己，读不出来才是
        // 错误（路径不存在）。
        let children = match self.list_all(root).await {
            Ok(children) => children
                .into_iter()
                .map(|entry| (entry.path, entry.kind == "dir"))
                .collect(),
            Err(error) => match self.scan(root).await {
                Ok(ViewRead::Text(_)) => vec![(root.to_string(), false)],
                _ => return Err(error),
            },
        };
        for (child, is_dir) in children {
            let mut files: Vec<String> = if is_dir {
                let Ok(path) = VfsPath::parse(&child) else {
                    continue;
                };
                self.walk(&path)
                    .await?
                    .into_iter()
                    .filter(|entry| entry.kind != "dir")
                    .map(|entry| entry.path)
                    .collect()
            } else {
                vec![child.clone()]
            };
            if let Some(ext) = file_ext {
                let suffix = format!(".{ext}");
                files.retain(|file| file.ends_with(&suffix));
            }
            files.sort_unstable_by(|a, b| b.cmp(a));
            for (scanned, file) in files.iter().enumerate() {
                if scanned >= max_files {
                    skipped.push(format!("{child} (file budget)"));
                    break;
                }
                let Ok(path) = VfsPath::parse(file) else {
                    continue;
                };
                let Ok(ViewRead::Text(text)) = self.scan(&path).await else {
                    continue;
                };
                for (line_no, line) in text.lines().enumerate() {
                    if regex.is_match(line) {
                        matches.push((file.clone(), line_no as u64 + 1, line.to_owned()));
                        if matches.len() >= max_matches {
                            skipped.push(format!("{child} (match budget)"));
                            return Ok((matches, skipped));
                        }
                    }
                }
            }
        }
        Ok((matches, skipped))
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

/// `/inbox/<chat>/<message_id>.md` 的文件名 → 消息 id；不是数字就是不存在的文件。
fn parse_message_file(path: &VfsPath, file: &str) -> Result<i64, VfsError> {
    file.trim_end_matches(".md")
        .parse()
        .map_err(|_| VfsError::NotFound(path.to_string()))
}

/// `Read` 接受查询串的视图：可筛选、可翻页的消息集合。
fn accepts_read_query(path: &VfsPath) -> bool {
    match path.segments() {
        [root] => root == "inbox" || root == "messages",
        [root, _] => root == "inbox",
        [root, _, file] => root == "inbox" && file == "index.md",
        [root, _, _, _, day] => root == "messages" && day.ends_with(".md"),
        _ => false,
    }
}

/// 列举的查询串里，`filter` 只能有 `contains:`；先于列举检查，写错时不白做一次列举。
fn check_listing_query(query: Option<&ViewQuery>) -> Result<(), VfsError> {
    match query.and_then(|query| query.filter.as_ref()) {
        Some(filter) => filter::check_listing_filter(filter),
        None => Ok(()),
    }
}

/// 列举结果按查询串筛选、分页；没有查询串时整页给出。
fn page_entries(mut entries: Vec<ViewEntry>, query: Option<&ViewQuery>) -> Page<ViewEntry> {
    let Some(query) = query else {
        return Page::all(entries);
    };
    if let Some(filter) = &query.filter {
        entries.retain(|entry| filter::matches_path(filter, &entry.path));
    }
    filter::page(entries, query)
}

fn storage(error: anyhow::Error) -> VfsError {
    VfsError::Storage(error.to_string())
}

fn render_agent_message(message: &AgentMessage) -> String {
    match message {
        AgentMessage::System { text } => format!("[system]\n{text}"),
        AgentMessage::User { text, .. } => format!("[user]\n{text}"),
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

    /// 子模块测试共用的最小 `Views`（离线、无配置）。
    pub(super) fn views_for_tests() -> (Views, tempfile::TempDir) {
        views()
    }

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
        let models = ModelsFile::embedded().unwrap();
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
        let (matches, skipped) = views
            .grep(&VfsPath::parse("/inbox").unwrap(), &regex, None, 200, 200)
            .await
            .unwrap();
        assert!(matches.is_empty() && skipped.is_empty());
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
        ParsedTarget::parse(path).unwrap()
    }

    /// 2026-11-05 本地正午起每条间隔一秒：同一天、顺序确定。
    fn day_message(chat: ChatId, id: i64, text: &str) -> QqMessage {
        use chrono::TimeZone;
        let mut message = message(chat, id, text);
        message.time = chrono::Local
            .with_ymd_and_hms(2026, 11, 5, 12, 0, id as u32)
            .unwrap()
            .with_timezone(&Utc);
        message
    }

    fn text(read: ViewRead) -> String {
        match read {
            ViewRead::Text(text) => text,
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stat_describes_dynamic_files_and_directories_without_reading() {
        let (views, _dir) = views();
        let chat = ChatId::Group(42);
        views.messages.append(&day_message(chat, 1, "one")).unwrap();
        views.messages.append(&day_message(chat, 2, "two")).unwrap();

        // 视图根：能列举就是目录，大小是条目数。
        let root = views
            .stat(&VfsPath::parse("/inbox").unwrap())
            .await
            .unwrap();
        assert_eq!((root.kind, root.size), ("dir", 1));
        // 父目录列举里的条目：chat 目录的大小是未读数。
        let chat_dir = views
            .stat(&VfsPath::parse("/inbox/group:42").unwrap())
            .await
            .unwrap();
        assert_eq!((chat_dir.kind, chat_dir.size), ("dir", 2));
        for file in [
            "/inbox/group:42/1.md",
            "/messages/groups/42/2026-11/05.md",
            "/config.toml",
            "/config/agent/model",
        ] {
            let entry = views.stat(&VfsPath::parse(file).unwrap()).await.unwrap();
            assert_eq!(entry.path, file);
            assert_eq!(entry.kind, "text", "{file}");
        }
        // 已读消息不在父目录列举里，仍可 Stat。
        views.messages.mark_read(chat, &[1]).unwrap();
        let read = views
            .stat(&VfsPath::parse("/inbox/group:42/1.md").unwrap())
            .await
            .unwrap();
        assert!(read.size > 0);
        assert!(
            views
                .stat(&VfsPath::parse("/inbox/group:42/999.md").unwrap())
                .await
                .is_err()
        );
        assert!(
            views
                .stat(&VfsPath::parse("/messages/groups/42/2026-11/06.md").unwrap())
                .await
                .is_err(),
            "a day without messages does not exist"
        );
        // Stat 不标记已读。
        assert_eq!(views.messages.unread_counts().unwrap(), vec![(chat, 1)]);
    }

    #[tokio::test]
    async fn inbox_reads_page_and_mark_only_what_they_show() {
        let (views, _dir) = views();
        let chat = ChatId::Group(42);
        for id in 1..=5 {
            views
                .messages
                .append(&day_message(chat, id, &format!("msg-{id}")))
                .unwrap();
        }
        let first = text(
            views
                .read(&target("/inbox/group:42?limit=2&order=desc"))
                .await
                .unwrap(),
        );
        // 新的在前；页外 3 条保持未读并提示。
        assert!(
            first.find("msg-5").unwrap() < first.find("msg-4").unwrap(),
            "{first}"
        );
        assert!(!first.contains("msg-3"), "{first}");
        assert!(first.contains("3 more unread"), "{first}");
        assert_eq!(views.messages.unread_counts().unwrap(), vec![(chat, 3)]);

        // offset 跳过未读里的前几条，被跳过的不标记。
        let skipped = text(
            views
                .read(&target("/inbox?offset=1&limit=1"))
                .await
                .unwrap(),
        );
        assert!(
            skipped.contains("msg-2") && !skipped.contains("msg-1"),
            "{skipped}"
        );
        assert_eq!(views.messages.unread_counts().unwrap(), vec![(chat, 2)]);

        let none = text(
            views
                .read(&target("/inbox?filter=from:99999"))
                .await
                .unwrap(),
        );
        assert!(none.contains("No unread message matches"), "{none}");
        let digest = text(
            views
                .read(&target("/messages?filter=contains:MSG-3"))
                .await
                .unwrap(),
        );
        assert!(digest.contains("group:42: 1 unread"), "{digest}");
    }

    #[tokio::test]
    async fn message_days_support_paging_and_state_filters() {
        let (views, _dir) = views();
        let chat = ChatId::Group(42);
        for id in 1..=4 {
            views
                .messages
                .append(&day_message(chat, id, &format!("day-{id}")))
                .unwrap();
        }
        views.messages.mark_read(chat, &[1, 2]).unwrap();
        let day = "/messages/groups/42/2026-11/05.md";

        // 扫描（Grep / Stat 的读法）给出全部、不分页、不标记。
        assert!(text(views.scan(&VfsPath::parse(day).unwrap()).await.unwrap()).contains("day-4"));
        assert_eq!(views.messages.unread_counts().unwrap(), vec![(chat, 2)]);

        let unread = text(
            views
                .read(&target(&format!(
                    "{day}?filter=state:unread&order=desc&limit=1"
                )))
                .await
                .unwrap(),
        );
        assert!(
            unread.contains("day-4") && !unread.contains("day-3"),
            "{unread}"
        );
        assert!(
            unread.contains("[showing 1-1 of 2; continue with offset=1]"),
            "{unread}"
        );
        // 读过的 day-4 变成已读：按状态筛选看到的是当前状态。
        let read = text(
            views
                .read(&target(&format!("{day}?filter=state:read")))
                .await
                .unwrap(),
        );
        assert!(
            read.contains("day-1") && read.contains("day-4") && !read.contains("day-3"),
            "{read}"
        );
        let middle = text(
            views
                .read(&target(&format!("{day}?order=desc&offset=1&limit=2")))
                .await
                .unwrap(),
        );
        assert!(
            middle.find("day-3").unwrap() < middle.find("day-2").unwrap()
                && !middle.contains("day-4")
                && !middle.contains("day-1"),
            "{middle}"
        );
    }

    #[tokio::test]
    async fn queries_are_rejected_where_they_mean_nothing() {
        let (views, _dir) = views();
        for path in ["/config.toml?limit=5", "/system/status?order=desc"] {
            let error = views.read(&target(path)).await.unwrap_err();
            assert!(error.to_string().contains("does not take query"), "{error}");
        }
        // 列举只认 contains:。
        let error = views
            .list(&target("/inbox?filter=unread"))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("listings only support"),
            "{error}"
        );
        let config = views
            .list(&target("/config?filter=contains:AGENT&limit=1"))
            .await
            .unwrap();
        assert_eq!(config.items.len(), 1);
        assert_eq!(config.items[0].path, "/config/agent");
    }

    #[tokio::test]
    async fn mentions_self_uses_the_account_qq() {
        let (views, _dir) = views();
        let views = views.with_context(ViewContext {
            self_id: 999,
            ..ViewContext::default()
        });
        let chat = ChatId::Group(42);
        let mut other = day_message(chat, 1, "for someone else");
        other.segments.push(crate::qq::event::Segment::At {
            qq: "555".to_owned(),
        });
        let mut mine = day_message(chat, 2, "for me");
        mine.segments.push(crate::qq::event::Segment::At {
            qq: "999".to_owned(),
        });
        views.messages.append(&other).unwrap();
        views.messages.append(&mine).unwrap();
        let shown = text(
            views
                .read(&target("/inbox?filter=mentions_self"))
                .await
                .unwrap(),
        );
        assert!(
            shown.contains("for me") && !shown.contains("someone else"),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn dismissing_with_a_query_handles_one_page() {
        let (views, _dir) = views();
        let chat = ChatId::Group(42);
        for id in 1..=3 {
            views
                .messages
                .append(&day_message(chat, id, &format!("x-{id}")))
                .unwrap();
        }
        views
            .messages
            .append(&day_message(ChatId::Group(7), 9, "x-other"))
            .unwrap();
        // 根目录不带 filter 拒绝；带 filter 时只处理一页并报告剩余。
        assert!(views.dismiss(&target("/inbox")).is_err());
        let dismissed = views
            .dismiss(&target("/inbox?filter=contains:x-&limit=2"))
            .unwrap();
        assert_eq!(
            dismissed,
            Dismissed {
                count: 2,
                remaining: 2
            }
        );
        // 单条不接受查询串；整个 chat 不带查询串时全部忽略。
        assert!(
            views
                .dismiss(&target("/inbox/group:42/3.md?limit=1"))
                .is_err()
        );
        let rest = views.dismiss(&target("/inbox/group:42/index.md")).unwrap();
        assert_eq!(rest.count, 1);
        assert_eq!(
            views.messages.unread_counts().unwrap(),
            vec![(ChatId::Group(7), 1)]
        );
    }

    #[tokio::test]
    async fn config_tree_and_leaves_are_readable() {
        let (views, _dir) = views();
        let sections = views.list(&target("/config")).await.unwrap().items;
        assert!(sections.iter().any(|entry| entry.path == "/config/agent"));
        assert!(
            sections
                .iter()
                .any(|entry| entry.path == "/config/models.json")
        );

        let keys = views.list(&target("/config/agent")).await.unwrap().items;
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
    async fn config_is_read_only_during_hold() {
        let (mut views, dir) = views();
        let gate = Arc::new(crate::security::gate::Gate::from_config(
            &crate::config::GuardConfig {
                enabled: true,
                backend: crate::config::GuardBackend::Off,
                ..crate::config::GuardConfig::default()
            },
            None,
            None,
            crate::security::gate::hold_path(dir.path()),
        ));
        views.context.gate = Some(Arc::clone(&gate));
        gate.force_hold("test");
        let error = views
            .write_config(&target("/config/filter/ignore_users"), "[2]")
            .unwrap_err();
        assert!(error.to_string().contains("during hold"), "{error}");
    }

    #[tokio::test]
    async fn mcp_secrets_never_reach_the_config_views() {
        let (views, _dir) = views();
        // 配一个带 token 的 MCP 服务器：磁盘文件与内存句柄都更新。
        let handle = views.context.config.clone().unwrap();
        let path = {
            let mut loaded = handle.write().unwrap();
            loaded
                .config
                .mcp
                .servers
                .push(crate::config::McpServerConfig {
                    name: "gh".to_owned(),
                    command: "npx".to_owned(),
                    env: [("GITHUB_TOKEN".to_owned(), "s3cret".to_owned())].into(),
                    ..crate::config::McpServerConfig::default()
                });
            std::fs::write(&loaded.path, toml::to_string(&loaded.config).unwrap()).unwrap();
            loaded.path.clone()
        };
        let text = |read: ViewRead| match read {
            ViewRead::Text(text) => text,
            other => panic!("expected text, got {other:?}"),
        };
        let section = text(views.read(&target("/config/mcp")).await.unwrap());
        assert!(
            !section.contains("s3cret") && section.contains("GITHUB_TOKEN"),
            "{section}"
        );
        let whole = text(views.read(&target("/config.toml")).await.unwrap());
        assert!(!whole.contains("s3cret"), "{whole}");

        // 把读到的（遮蔽后的）原文改一个可写键写回：成功，且磁盘上的 token 原样保留。
        let edited = whole.replace("require_mention = false", "require_mention = true");
        assert_ne!(edited, whole);
        views
            .write_config(&target("/config.toml"), &edited)
            .unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("s3cret"), "{on_disk}");
        assert!(handle.read().unwrap().config.filter.require_mention);
    }

    /// 动态视图上的搜索有预算：每个直接子节点下只扫有限个文件（从最新的扫起），匹配的行数
    /// 有总上限，到了预算的子节点在结果里注明。没有预算时一次搜索会扫完整个历史。
    #[tokio::test]
    async fn searching_message_history_is_budgeted() {
        use chrono::TimeZone;
        let (views, _dir) = views();
        let chat = crate::qq::event::ChatId::Group(42);
        // 三天，每天一条。
        for (id, day) in [(1, 3), (2, 4), (3, 5)] {
            let mut stored = message(chat, id, &format!("needle on day {day}"));
            stored.time = chrono::Local
                .with_ymd_and_hms(2026, 11, day, 12, 0, 0)
                .unwrap()
                .with_timezone(&chrono::Utc);
            views.messages.append(&stored).unwrap();
        }
        let regex = regex::Regex::new("needle").unwrap();
        let root = VfsPath::parse("/messages").unwrap();
        let days = |matches: &[(String, u64, String)]| {
            matches
                .iter()
                .map(|(path, _, _)| path.rsplit('/').next().unwrap().to_owned())
                .collect::<Vec<_>>()
        };

        // 预算够：三天都搜到，没有注记。
        let (matches, skipped) = views.grep(&root, &regex, None, 200, 200).await.unwrap();
        assert_eq!(matches.len(), 3, "{matches:?}");
        assert!(skipped.is_empty(), "{skipped:?}");

        // 每个子节点只许扫两个文件：留下的是最新的两天，并注明这个子节点没扫完。
        let (matches, skipped) = views.grep(&root, &regex, None, 2, 200).await.unwrap();
        assert_eq!(days(&matches), ["05.md", "04.md"], "{matches:?}");
        assert_eq!(skipped, ["/messages/groups (file budget)"]);

        // 匹配行数到上限就停。
        let (matches, skipped) = views.grep(&root, &regex, None, 200, 1).await.unwrap();
        assert_eq!(days(&matches), ["05.md"], "{matches:?}");
        assert_eq!(skipped, ["/messages/groups (match budget)"]);

        // 根就是一个文件时只搜它自己。
        let file = VfsPath::parse(&matches[0].0).unwrap();
        let (matches, skipped) = views.grep(&file, &regex, None, 200, 200).await.unwrap();
        assert_eq!(days(&matches), ["05.md"], "{matches:?}");
        assert!(skipped.is_empty(), "{skipped:?}");
        // 搜索不改变未读状态。
        assert_eq!(views.messages.unread_counts().unwrap(), vec![(chat, 3)]);
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

    /// 环境变量设的值在 `/config` 写入之后仍然生效：换进内存的配置与启动加载得到的是同一份。
    /// 以前写入时只解析文件文本，agent 改一次配置，环境变量设的值就退回文件里的值，直到重启。
    #[tokio::test]
    async fn config_writes_keep_environment_overrides() {
        let (views, _dir) = views();
        let handle = Arc::clone(views.context.config.as_ref().unwrap());
        // 用别的测试不检查的键，避免与并行的测试互相影响。
        unsafe { std::env::set_var("SBOT__NAPCAT__URL", "ws://from-env:3999") };
        let written = views.write_config(&target("/config/agent/thinking_level"), "\"low\"");
        unsafe { std::env::remove_var("SBOT__NAPCAT__URL") };
        written.unwrap();
        let loaded = handle.read().unwrap();
        assert_eq!(loaded.config.agent.thinking_level, "low");
        assert_eq!(loaded.config.napcat.url, "ws://from-env:3999");
        // 覆盖只在内存里：文件里不会多出环境变量的值。
        let on_disk = std::fs::read_to_string(&loaded.path).unwrap();
        assert!(!on_disk.contains("from-env"), "{on_disk}");
    }

    /// 热更新回调被调用时，配置句柄里已经是新配置，而且回调可以去读它（写锁已经放掉）。
    /// 以前先通知、后换句柄：回调读句柄看到的是旧配置。
    #[tokio::test]
    async fn hot_reload_runs_after_the_handle_holds_the_new_config() {
        let (views, _dir) = views();
        let handle = Arc::clone(views.context.config.as_ref().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        views.set_hot_reload(Arc::new(move |config| {
            let in_handle = handle
                .try_read()
                .expect("the config lock is free during the callback")
                .config
                .agent
                .thinking_level
                .clone();
            record
                .lock()
                .unwrap()
                .push((config.agent.thinking_level.clone(), in_handle));
        }));
        views
            .write_config(&target("/config/agent/thinking_level"), "\"low\"")
            .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("low".to_owned(), "low".to_owned())]
        );
    }

    /// 同时写不同的键：每一次写都读到上一次写完的文件，最后文件、内存里的配置与每一次
    /// 写入一致。读改写没有串行化时，两个写入各读一份旧文件、各自写回，后写的盖掉先写的。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_config_writes_do_not_lose_updates() {
        let (views, dir) = views();
        let views = Arc::new(views);
        let writes: Vec<(&str, String)> = vec![
            ("/config/filter/ignore_users", "[11, 12]".to_owned()),
            ("/config/filter/ignore_groups", "[21]".to_owned()),
            ("/config/filter/require_mention", "true".to_owned()),
            ("/config/queue/debounce_ms", "1234".to_owned()),
            ("/config/queue/cap_messages", "77".to_owned()),
            ("/config/agent/thinking_level", "\"low\"".to_owned()),
        ];
        // 多轮：每轮所有键一起写，轮与轮之间值不同，增加交错的机会。
        for round in 0..20u64 {
            let tasks: Vec<_> = writes
                .iter()
                .map(|(path, value)| {
                    let views = Arc::clone(&views);
                    let path = (*path).to_owned();
                    let value = if path.ends_with("debounce_ms") {
                        (1000 + round).to_string()
                    } else {
                        value.clone()
                    };
                    tokio::task::spawn_blocking(move || {
                        views.write_config(&target(&path), &value).unwrap();
                    })
                })
                .collect();
            for task in tasks {
                task.await.unwrap();
            }
            let on_disk: SbotConfig =
                toml::from_str(&std::fs::read_to_string(dir.path().join("sbot.toml")).unwrap())
                    .unwrap();
            assert_eq!(on_disk.filter.ignore_users, vec![11, 12], "round {round}");
            assert_eq!(on_disk.filter.ignore_groups, vec![21], "round {round}");
            assert!(on_disk.filter.require_mention, "round {round}");
            assert_eq!(on_disk.queue.debounce_ms, 1000 + round, "round {round}");
            assert_eq!(on_disk.queue.cap_messages, 77, "round {round}");
            assert_eq!(on_disk.agent.thinking_level, "low", "round {round}");
            // 内存里的配置与文件一致。
            let in_memory = views.config_snapshot().unwrap();
            assert_eq!(
                toml::to_string(&in_memory).unwrap(),
                toml::to_string(&on_disk).unwrap(),
                "round {round}"
            );
        }
        // 不留下临时文件。
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[tokio::test]
    async fn receive_scopes_filter_views() {
        let (views, _dir) = views();
        let store = views.messages_store();
        store
            .append(&message(crate::qq::event::ChatId::Group(1), 1, "one"))
            .unwrap();
        store
            .append(&message(crate::qq::event::ChatId::Group(2), 2, "two"))
            .unwrap();

        let scoped = views.with_context(ViewContext {
            receive_scopes: vec!["group:1".to_owned()],
            ..ViewContext::default()
        });
        let listed = scoped
            .list(&target("/messages/groups"))
            .await
            .unwrap()
            .items;
        let paths: Vec<&str> = listed.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(paths, vec!["/messages/groups/1"]);

        let day = format!("{}", chrono::Local::now().format("%Y-%m"));
        assert!(
            scoped
                .read(&target(&format!("/messages/groups/2/{day}/04.md")))
                .await
                .is_err()
        );
        assert!(!scoped.receive_allows(crate::qq::event::ChatId::Group(2)));
        assert!(scoped.receive_allows(crate::qq::event::ChatId::Group(1)));
    }
}
