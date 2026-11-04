//! 消息归档：按群/私聊、月、日写入加密 JSONL，SQLite 只做可重建索引。
//!
//! 设计依据：设计 §8.2 / §8.3。
//!
//! 物理布局：
//! ```text
//! messages/groups/<gid>/<YYYY-MM>/<DD>.jsonl
//! messages/private/<qq>/<YYYY-MM>/<DD>.jsonl
//! ```
//! 首行是明文文件头：`{v, kind:"header", scope, peer_id, date, encryption, created_at, file_id}`。
//! 之后每行是一条消息：
//! ```text
//! {"v":1,"kind":"message","id":55555,"ts":"...","scope":"groups","peer_id":123,
//!  "sender":"10001","nonce":"<base64>","ct":"<base64>"}
//! ```
//! AAD 绑定 `message:<scope>:<peer>:<id>:<file_id>:<line_no>`，
//! 因此行被移动、复制到别的文件或改行号都会解密失败；`id` 在明文里，
//! 索引可以在没有数据库时从 JSONL 完整重建。
//!
//! 过滤器是准入门：被拒消息根本不会走到这里。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use base64::Engine;
use chrono::{Datelike, Local, Utc};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::config::EncryptionMode;
use crate::qq::event::{ChatId, QqMessage};
use crate::storage::Database;
use crate::storage::crypto::MasterKey;

/// 行格式版本。
const LINE_VERSION: u32 = 1;

/// 收件箱状态（设计 §8.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxState {
    Unread,
    Read,
    Dismissed,
}

impl InboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unread => "unread",
            Self::Read => "read",
            Self::Dismissed => "dismissed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "unread" => Self::Unread,
            "read" => Self::Read,
            "dismissed" => Self::Dismissed,
            _ => return None,
        })
    }
}

/// 清理报告。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub removed_files: u64,
    pub removed_messages: u64,
    pub freed_bytes: u64,
}

/// 消息存储。
pub struct MessageStore {
    db: Arc<Database>,
    key: Arc<MasterKey>,
    /// 数据根目录（`[storage].path`），其下是 `messages/`。
    root: PathBuf,
    /// `max_message_bytes`；0 = 不限制。
    max_bytes: u64,
    /// 与 `[storage].encryption` 一致；`none` 只用于调试。
    encryption: EncryptionMode,
    /// 串行化写入：行号 = 已提交行数，取号与写入必须在同一个临界区，
    /// 否则两个并发 append 会拿到同一个行号（AAD 相同，读到错误的消息）。
    append_lock: Mutex<()>,
    /// 按年龄清理旧月份；0 = 只按配额。
    retention_days: u32,
}

impl MessageStore {
    /// 打开存储并确保目录存在。
    pub fn open(
        db: Arc<Database>,
        key: Arc<MasterKey>,
        root: PathBuf,
        max_bytes: u64,
        encryption: EncryptionMode,
    ) -> Result<Self> {
        std::fs::create_dir_all(root.join("messages"))
            .with_context(|| format!("cannot create {}", root.join("messages").display()))?;
        Ok(Self {
            db,
            key,
            root,
            max_bytes,
            encryption,
            append_lock: Mutex::new(()),
            retention_days: 0,
        })
    }

    /// 设置按年龄清理的天数（0 = 关闭）；启动时也可调用 [`Self::enforce_retention`]。
    pub fn with_retention_days(mut self, days: u32) -> Self {
        self.retention_days = days;
        self
    }

    /// 追加一条消息；已存在（同 chat + message_id）时返回 `false`。
    ///
    /// 返回 `true` 表示新写入，调用方才应触发去抖与回合。
    pub fn append(&self, message: &QqMessage) -> Result<bool> {
        let _guard = self
            .append_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let conn = self.db.lock();
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM messages WHERE chat = ?1 AND message_id = ?2",
                params![message.chat.to_string(), message.message_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if exists {
            return Ok(false);
        }
        drop(conn);

        // 超配额时先清理最旧的完整月份，再写入。
        self.make_room()?;

        let (scope, peer) = scope_peer(message.chat);
        let local = message.time.with_timezone(&Local);
        let month = local.format("%Y-%m").to_string();
        let day_number = local.format("%d").to_string();
        let date = local.format("%Y-%m-%d").to_string();
        let relative = format!("messages/{scope}/{peer}/{month}/{day_number}.jsonl");
        let path = self.root.join(&relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file_id = self.ensure_header(&path, scope, peer, &date)?;
        let line_no: i64 = {
            let conn = self.db.lock();
            // 文件头占第 0 行，消息从第 1 行开始。
            1 + conn
                .query_row(
                    "SELECT COALESCE(SUM(count), 0) FROM message_files WHERE path = ?1",
                    params![relative],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
        };

        let line = self.encode_line(message, scope, peer, &file_id, line_no as usize)?;

        {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("cannot open {}", path.display()))?;
            file.write_all(line.as_bytes())?;
            file.flush()?;
        }

        // 索引写入放在文件之后：即使进程在两者之间崩溃，`rebuild_index` 能补回来。
        let now = Utc::now().to_rfc3339();
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO messages
                (chat, message_id, sender, ts, day, file_path, line_no, reply_to, is_self, backfilled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(chat, message_id) DO NOTHING",
            params![
                message.chat.to_string(),
                message.message_id,
                message.sender.label(),
                message.time.to_rfc3339(),
                date,
                relative,
                line_no,
                message.reply_to,
                message.is_self as i32,
                message.backfilled as i32
            ],
        )?;
        // 自身消息不进收件箱；其它消息默认未读。
        tx.execute(
            "INSERT INTO inbox_state (chat, message_id, state, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(chat, message_id) DO NOTHING",
            params![
                message.chat.to_string(),
                message.message_id,
                if message.is_self { "read" } else { "unread" },
                now
            ],
        )?;
        tx.execute(
            "INSERT INTO message_files (path, scope, peer, month, day, count, bytes, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)
             ON CONFLICT(path) DO UPDATE SET
                count = message_files.count + 1,
                bytes = message_files.bytes + ?6,
                updated_at = excluded.updated_at",
            params![
                relative,
                scope,
                peer as i64,
                month,
                date,
                line.len() as i64,
                now
            ],
        )?;
        let seen_at = Utc::now().timestamp();
        for origin in message.media_origins() {
            tx.execute(
                "INSERT INTO media_origins (origin, chat, seen_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(origin) DO NOTHING",
                params![origin, message.chat.to_string(), seen_at],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// 按 message_id 找它所属的 chat；`get_msg` 没有 chat 参数时用它做范围校验。
    pub fn chat_for_message(&self, message_id: i64) -> Result<Option<ChatId>> {
        let conn = self.db.lock();
        let chat: Option<String> = conn
            .query_row(
                "SELECT chat FROM messages WHERE message_id = ?1 LIMIT 1",
                params![message_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(chat.and_then(|chat| ChatId::parse(&chat)))
    }

    /// 按 chat + message_id 读取消息。
    pub fn load(&self, chat: ChatId, message_id: i64) -> Result<Option<QqMessage>> {
        let row: Option<(String, i64)> = {
            let conn = self.db.lock();
            conn.query_row(
                "SELECT file_path, line_no FROM messages WHERE chat = ?1 AND message_id = ?2",
                params![chat.to_string(), message_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
        };
        let Some((file_path, line_no)) = row else {
            return Ok(None);
        };
        self.read_line(
            &self.root.join(file_path),
            line_no as usize,
            Some(message_id),
        )
    }

    /// 某天某 chat 的全部消息（按时间升序）。
    pub fn messages_for_day(&self, chat: ChatId, day: &str) -> Result<Vec<QqMessage>> {
        self.load_indexed(
            "SELECT file_path, line_no, message_id FROM messages
             WHERE chat = ?1 AND day = ?2 ORDER BY ts ASC",
            params![chat.to_string(), day],
        )
    }

    /// 最近 N 条（按时间升序返回）。
    pub fn recent(&self, chat: ChatId, limit: u32) -> Result<Vec<QqMessage>> {
        let mut messages = self.load_indexed(
            "SELECT file_path, line_no, message_id FROM messages
             WHERE chat = ?1 ORDER BY ts DESC LIMIT ?2",
            params![chat.to_string(), limit],
        )?;
        messages.reverse();
        Ok(messages)
    }

    /// 全部未读（可按 chat 过滤），按时间升序。
    pub fn unread(&self, chat: Option<ChatId>) -> Result<Vec<QqMessage>> {
        match chat {
            Some(chat) => self.load_indexed(
                "SELECT m.file_path, m.line_no, m.message_id FROM messages m
                 JOIN inbox_state s ON s.chat = m.chat AND s.message_id = m.message_id
                 WHERE m.chat = ?1 AND s.state = 'unread'
                 ORDER BY m.ts ASC",
                params![chat.to_string()],
            ),
            None => self.load_indexed(
                "SELECT m.file_path, m.line_no, m.message_id FROM messages m
                 JOIN inbox_state s ON s.chat = m.chat AND s.message_id = m.message_id
                 WHERE s.state = 'unread'
                 ORDER BY m.ts ASC",
                params![],
            ),
        }
    }

    /// 每个来源的未读数。
    pub fn unread_counts(&self) -> Result<Vec<(ChatId, u64)>> {
        let conn = self.db.lock();
        let mut statement = conn.prepare(
            "SELECT chat, COUNT(*) FROM inbox_state WHERE state = 'unread'
             GROUP BY chat ORDER BY chat ASC",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(chat, count)| ChatId::parse(&chat).map(|chat| (chat, count)))
            .collect())
    }

    /// 查询某条消息的收件箱状态。
    pub fn state(&self, chat: ChatId, message_id: i64) -> Result<Option<InboxState>> {
        let conn = self.db.lock();
        let raw: Option<String> = conn
            .query_row(
                "SELECT state FROM inbox_state WHERE chat = ?1 AND message_id = ?2",
                params![chat.to_string(), message_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(raw.as_deref().and_then(InboxState::parse))
    }

    /// 设置状态；`dismissed` 不会被后续 `read` 覆盖。
    pub fn set_state(&self, chat: ChatId, message_id: i64, state: InboxState) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "INSERT INTO inbox_state (chat, message_id, state, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(chat, message_id) DO UPDATE SET
                state = CASE
                    WHEN inbox_state.state = 'dismissed' AND excluded.state = 'read' THEN 'dismissed'
                    ELSE excluded.state
                END,
                updated_at = excluded.updated_at",
            params![
                chat.to_string(),
                message_id,
                state.as_str(),
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// 批量标记已读；“内容进入上下文就算读过”。
    pub fn mark_read(&self, chat: ChatId, ids: &[i64]) -> Result<usize> {
        let mut marked = 0;
        for id in ids {
            if self.state(chat, *id)? == Some(InboxState::Unread) {
                self.set_state(chat, *id, InboxState::Read)?;
                marked += 1;
            }
        }
        Ok(marked)
    }

    /// 批量忽略；返回实际忽略的条数。
    pub fn dismiss(&self, chat: ChatId, ids: &[i64]) -> Result<usize> {
        let mut count = 0;
        for id in ids {
            self.set_state(chat, *id, InboxState::Dismissed)?;
            count += 1;
        }
        Ok(count)
    }

    /// 读取断线游标。
    pub fn cursor(&self, chat: ChatId) -> Result<Option<(i64, String)>> {
        let conn = self.db.lock();
        Ok(conn
            .query_row(
                "SELECT last_message_id, last_ts FROM chat_cursors WHERE chat = ?1",
                params![chat.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// 更新断线游标。
    pub fn set_cursor(&self, chat: ChatId, message_id: i64, ts: &str) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "INSERT INTO chat_cursors (chat, last_message_id, last_ts, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(chat) DO UPDATE SET
                last_message_id = excluded.last_message_id,
                last_ts = excluded.last_ts,
                updated_at = excluded.updated_at",
            params![chat.to_string(), message_id, ts, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// 已知的最近活跃 chat（供断线重连补消息）。
    pub fn known_chats(&self) -> Result<Vec<ChatId>> {
        let conn = self.db.lock();
        let mut statement = conn.prepare(
            "SELECT chat FROM chat_cursors UNION SELECT chat FROM messages GROUP BY chat",
        )?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.iter().filter_map(|chat| ChatId::parse(chat)).collect())
    }

    /// 某 chat 有数据的月份（升序）。
    pub fn months(&self, chat: ChatId) -> Result<Vec<String>> {
        let (scope, peer) = scope_peer(chat);
        let conn = self.db.lock();
        let mut statement = conn.prepare(
            "SELECT DISTINCT month FROM message_files
             WHERE scope = ?1 AND peer = ?2 ORDER BY month ASC",
        )?;
        Ok(statement
            .query_map(params![scope, peer as i64], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// 某 chat 某月有数据的日子（`DD`，升序）。
    pub fn days(&self, chat: ChatId, month: &str) -> Result<Vec<String>> {
        let (scope, peer) = scope_peer(chat);
        let conn = self.db.lock();
        let mut statement = conn.prepare(
            "SELECT DISTINCT day FROM message_files
             WHERE scope = ?1 AND peer = ?2 AND month = ?3 ORDER BY day ASC",
        )?;
        Ok(statement
            .query_map(params![scope, peer as i64, month], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// 某个 scope（groups / private）下出现过的 peer。
    pub fn peers(&self, scope: &str) -> Result<Vec<u64>> {
        let conn = self.db.lock();
        let mut statement = conn.prepare(
            "SELECT DISTINCT peer FROM message_files WHERE scope = ?1 ORDER BY peer ASC",
        )?;
        Ok(statement
            .query_map(params![scope], |row| row.get::<_, u64>(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// 读取某天某 chat 的收件箱条目（带状态）。
    pub fn inbox_for_chat(&self, chat: ChatId, state: InboxState) -> Result<Vec<QqMessage>> {
        self.load_indexed(
            "SELECT m.file_path, m.line_no, m.message_id FROM messages m
             JOIN inbox_state s ON s.chat = m.chat AND s.message_id = m.message_id
             WHERE m.chat = ?1 AND s.state = ?2 ORDER BY m.ts ASC",
            params![chat.to_string(), state.as_str()],
        )
    }

    /// 发送账本：登记意图（发送前）。
    pub fn begin_send(&self, id: &str, chat: ChatId, intent: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self.db.lock();
        conn.execute(
            "INSERT INTO outbound_intents (id, chat, intent, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'pending', ?4, ?4)
             ON CONFLICT(id) DO NOTHING",
            params![id, chat.to_string(), intent, now],
        )?;
        Ok(())
    }

    /// 发送账本：记录结果。
    pub fn finish_send(&self, id: &str, status: &str, message_id: Option<i64>) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "UPDATE outbound_intents
             SET status = ?1, message_id = ?2, updated_at = ?3
             WHERE id = ?4",
            params![status, message_id, Utc::now().to_rfc3339(), id],
        )?;
        Ok(())
    }

    /// 启动核对：超过 `age_secs` 仍是 `pending` 的发送标记为 `unknown`。
    ///
    /// 崩溃/超时可能发生在“对方已收到、本地没记下 message_id”之间；不能当作
    /// 失败自动重试，先标记为不确定，交给 agent 查历史后决定。
    pub fn mark_stale_intents(&self, age_secs: u64) -> Result<u64> {
        let cutoff = (Utc::now() - chrono::Duration::seconds(age_secs as i64)).to_rfc3339();
        let conn = self.db.lock();
        let changed = conn.execute(
            "UPDATE outbound_intents SET status = 'unknown', updated_at = ?1
             WHERE status = 'pending' AND created_at < ?2",
            params![Utc::now().to_rfc3339(), cutoff],
        )?;
        Ok(changed as u64)
    }

    /// 未确认的发送（崩溃恢复时先查它们）。
    pub fn unconfirmed_sends(&self) -> Result<Vec<(String, ChatId, String)>> {
        let conn = self.db.lock();
        let mut statement = conn.prepare(
            "SELECT id, chat, intent FROM outbound_intents
             WHERE status IN ('pending', 'unknown') ORDER BY created_at ASC",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, chat, intent)| ChatId::parse(&chat).map(|chat| (id, chat, intent)))
            .collect())
    }

    /// 用量：字节数与消息条数。
    pub fn usage(&self) -> Result<(u64, u64)> {
        let conn = self.db.lock();
        let (bytes, count) = conn.query_row(
            "SELECT COALESCE(SUM(bytes), 0), COALESCE(SUM(count), 0) FROM message_files",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((bytes, count))
    }

    /// 磁盘 JSONL 文件数与索引里的文件数。
    ///
    /// 启动时用于决定是否需要重建索引（设计 §8.2：索引只是可重建缓存）。
    /// 只遍历文件名，不解析内容，百万级消息下也是 O(文件数)。
    pub fn index_file_counts(&self) -> Result<(u64, u64)> {
        let messages_dir = self.root.join("messages");
        let disk = if messages_dir.exists() {
            walk_files(&messages_dir)?.len() as u64
        } else {
            0
        };
        let indexed: u64 =
            self.db
                .lock()
                .query_row("SELECT COUNT(*) FROM message_files", [], |row| row.get(0))?;
        Ok((disk, indexed))
    }

    /// 从 JSONL 重建索引（设计 §8.2：索引是可重建缓存）。
    ///
    /// 补 `messages`、`message_files` 与 `inbox_state` 三张表：前两张决定配额、
    /// 保留与月视图，最后一张决定未读数；只补 `messages` 会让其余视图静默失准。
    pub fn rebuild_index(&self) -> Result<u64> {
        let messages_dir = self.root.join("messages");
        if !messages_dir.exists() {
            return Ok(0);
        }
        let mut repaired = 0u64;
        // path → (scope, peer, month, day, count, bytes)
        let mut files: HashMap<String, (String, i64, String, String, u64, u64)> = HashMap::new();
        for entry in walk_files(&messages_dir)? {
            let relative = entry
                .strip_prefix(&self.root)?
                .to_string_lossy()
                .replace('\\', "/");
            let file = std::fs::File::open(&entry)?;
            let lines: Vec<String> = BufReader::new(file)
                .lines()
                .collect::<Result<Vec<_>, _>>()?;
            let Some(header) = lines
                .first()
                .and_then(|line| serde_json::from_str::<Value>(line).ok())
            else {
                continue;
            };
            let Some(scope) = header.get("scope").and_then(Value::as_str) else {
                continue;
            };
            let Some(peer) = header.get("peer_id").and_then(Value::as_u64) else {
                continue;
            };
            let Some(day) = header.get("date").and_then(Value::as_str) else {
                continue;
            };
            let month = day.get(..7).unwrap_or(day).to_owned();
            let mut count = 0u64;
            let mut bytes = 0u64;
            for (line_no, line) in lines.iter().enumerate().skip(1) {
                bytes += line.len() as u64 + 1;
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                let Some(chat) = value
                    .get("scope")
                    .and_then(Value::as_str)
                    .and_then(|scope| {
                        let peer = value.get("peer_id").and_then(Value::as_u64)?;
                        match scope {
                            "groups" => Some(ChatId::Group(peer)),
                            "private" => Some(ChatId::Private(peer)),
                            _ => None,
                        }
                    })
                else {
                    continue;
                };
                let Some(message_id) = value.get("id").and_then(Value::as_i64) else {
                    continue;
                };
                count += 1;
                let already: bool = {
                    let conn = self.db.lock();
                    conn.query_row(
                        "SELECT 1 FROM messages WHERE chat = ?1 AND message_id = ?2",
                        params![chat.to_string(), message_id],
                        |_| Ok(true),
                    )
                    .optional()?
                    .unwrap_or(false)
                };
                if already {
                    continue;
                }
                let Some(message) = self.read_line(&entry, line_no, Some(message_id))? else {
                    continue;
                };
                let local = message.time.with_timezone(&Local);
                let conn = self.db.lock();
                conn.execute(
                    "INSERT INTO messages
                        (chat, message_id, sender, ts, day, file_path, line_no, reply_to, is_self, backfilled)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        chat.to_string(),
                        message_id,
                        message.sender.label(),
                        message.time.to_rfc3339(),
                        local.format("%Y-%m-%d").to_string(),
                        relative,
                        line_no as i64,
                        message.reply_to,
                        message.is_self as i32,
                        message.backfilled as i32
                    ],
                )?;
                repaired += 1;
            }
            files.insert(
                relative,
                (
                    scope.to_owned(),
                    peer as i64,
                    month,
                    day.to_owned(),
                    count,
                    bytes,
                ),
            );
        }

        let now = Utc::now().to_rfc3339();
        {
            let conn = self.db.lock();
            // 磁盘上已不存在的文件，其索引行与配额一起清掉。
            let indexed: Vec<String> = {
                let mut statement = conn.prepare("SELECT path FROM message_files")?;
                statement
                    .query_map([], |row| row.get(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };
            for path in indexed {
                if !files.contains_key(&path) {
                    conn.execute("DELETE FROM message_files WHERE path = ?1", params![path])?;
                }
            }
            for (path, (scope, peer, month, day, count, bytes)) in &files {
                conn.execute(
                    "INSERT INTO message_files
                        (path, scope, peer, month, day, count, bytes, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(path) DO UPDATE SET
                        count = excluded.count,
                        bytes = excluded.bytes,
                        updated_at = excluded.updated_at",
                    params![
                        path,
                        scope,
                        peer,
                        month,
                        day,
                        *count as i64,
                        *bytes as i64,
                        now
                    ],
                )?;
            }
            // 没有收件箱行的消息补回默认状态；自身消息默认已读。
            conn.execute(
                "INSERT OR IGNORE INTO inbox_state (chat, message_id, state, updated_at)
                 SELECT chat, message_id,
                        CASE WHEN is_self = 1 THEN 'read' ELSE 'unread' END,
                        ?1
                 FROM messages",
                params![now],
            )?;
            conn.execute(
                "DELETE FROM inbox_state WHERE NOT EXISTS (
                     SELECT 1 FROM messages m
                     WHERE m.chat = inbox_state.chat AND m.message_id = inbox_state.message_id
                 )",
                [],
            )?;
        }
        Ok(repaired)
    }

    /// 按年龄与配额清理最旧的完整月份，绝不删除当前月份。
    pub fn enforce_retention(&self, retention_days: u32) -> Result<CleanupReport> {
        let mut report = CleanupReport::default();
        let current_month = Local::now().format("%Y-%m").to_string();
        let cutoff_month = if retention_days == 0 {
            None
        } else {
            let cutoff = Local::now() - chrono::Duration::days(i64::from(retention_days));
            Some(cutoff.format("%Y-%m").to_string())
        };

        loop {
            let (total_bytes, _) = self.usage()?;
            let over = self.max_bytes != 0 && total_bytes > self.max_bytes;

            let month: Option<(String, u64, u64)> = {
                let conn = self.db.lock();
                conn.query_row(
                    "SELECT month, SUM(bytes), SUM(count) FROM message_files
                     WHERE month != ?1
                     GROUP BY month ORDER BY month ASC LIMIT 1",
                    params![current_month],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
            };

            let Some((month, bytes, count)) = month else {
                break;
            };
            let expired = cutoff_month
                .as_ref()
                .is_some_and(|cutoff| month.as_str() < cutoff.as_str());
            if !over && !expired {
                break;
            }

            let files: Vec<String> = {
                let conn = self.db.lock();
                let mut statement =
                    conn.prepare("SELECT path FROM message_files WHERE month = ?1")?;
                statement
                    .query_map(params![month], |row| row.get(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };
            for file in &files {
                let path = self.root.join(file);
                if let Err(error) = std::fs::remove_file(&path) {
                    warn!(%error, path = %path.display(), "cannot remove expired message file");
                }
            }
            let conn = self.db.lock();
            conn.execute(
                "DELETE FROM messages WHERE day LIKE ?1",
                params![format!("{month}-%")],
            )?;
            conn.execute("DELETE FROM message_files WHERE month = ?1", params![month])?;
            // 消息行已删，收件箱状态不能留下孤儿，否则未读数会永久虚高。
            conn.execute(
                "DELETE FROM inbox_state WHERE NOT EXISTS (
                     SELECT 1 FROM messages m
                     WHERE m.chat = inbox_state.chat AND m.message_id = inbox_state.message_id
                 )",
                [],
            )?;
            report.removed_files += files.len() as u64;
            report.removed_messages += count;
            report.freed_bytes += bytes;
        }
        Ok(report)
    }

    /// 从索引行读取消息；任何一条失败都跳过而不是让整个视图失败。
    fn load_indexed(&self, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Result<Vec<QqMessage>> {
        let rows: Vec<(String, i64, i64)> = {
            let conn = self.db.lock();
            let mut statement = conn.prepare(sql)?;
            statement
                .query_map(params, |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut messages = Vec::with_capacity(rows.len());
        for (file_path, line_no, message_id) in rows {
            match self.read_line(
                &self.root.join(&file_path),
                line_no as usize,
                Some(message_id),
            ) {
                Ok(Some(message)) => messages.push(message),
                Ok(None) => {}
                Err(error) => {
                    warn!(%error, file = %file_path, line = line_no, "cannot read message");
                }
            }
        }
        Ok(messages)
    }

    /// 确保文件头存在，返回 `file_id`。
    fn ensure_header(&self, path: &Path, scope: &str, peer: u64, date: &str) -> Result<String> {
        if path.exists()
            && let Some(file_id) = read_file_id(path)?
        {
            return Ok(file_id);
        }
        // 空文件或残行：补写文件头。

        let file_id = new_file_id();
        let header = json!({
            "v": LINE_VERSION,
            "kind": "header",
            "scope": scope,
            "peer_id": peer,
            "date": date,
            "encryption": match self.encryption {
                EncryptionMode::XChaCha20Poly1305 => "xchacha20poly1305",
                EncryptionMode::None => "none",
            },
            "created_at": Utc::now().to_rfc3339(),
            "file_id": file_id,
        });
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        file.write_all(format!("{header}\n").as_bytes())?;
        file.flush()?;
        Ok(file_id)
    }

    /// 编码一行。
    fn encode_line(
        &self,
        message: &QqMessage,
        scope: &str,
        peer: u64,
        file_id: &str,
        line_no: usize,
    ) -> Result<String> {
        let payload = serde_json::to_vec(message)?;
        let (nonce, ciphertext) = match self.encryption {
            EncryptionMode::XChaCha20Poly1305 => {
                let aad = format!(
                    "message:{scope}:{peer}:{}:{file_id}:{line_no}",
                    message.message_id
                );
                let sealed = self.key.seal("message", aad.as_bytes(), &payload)?;
                let (nonce, ct) = sealed.split_at(24);
                (
                    base64::engine::general_purpose::STANDARD.encode(nonce),
                    base64::engine::general_purpose::STANDARD.encode(ct),
                )
            }
            EncryptionMode::None => (
                String::new(),
                base64::engine::general_purpose::STANDARD.encode(payload),
            ),
        };

        let line = json!({
            "v": LINE_VERSION,
            "kind": "message",
            "id": message.message_id,
            "ts": message.time.to_rfc3339(),
            "scope": scope,
            "peer_id": peer,
            "sender": message.sender.label(),
            "nonce": nonce,
            "ct": ciphertext,
        });
        Ok(format!("{line}\n"))
    }

    /// 读取某文件的某一行并解密。
    ///
    /// `expected_id` 用于校验索引行号与文件内容一致：索引可能因崩溃或并发写入
    /// 落后/错位，解密本身不会发现这种事（AAD 绑定的是行内 id），必须在返回前比对。
    fn read_line(
        &self,
        path: &Path,
        line_no: usize,
        expected_id: Option<i64>,
    ) -> Result<Option<QqMessage>> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let lines: Vec<String> = BufReader::new(file)
            .lines()
            .collect::<Result<Vec<_>, _>>()?;
        let Some(header) = lines.first() else {
            return Ok(None);
        };
        let header: Value = serde_json::from_str(header)?;
        let file_id = header
            .get("file_id")
            .and_then(Value::as_str)
            .context("message file has no file_id")?
            .to_owned();
        // 按文件头记录的算法解密：切换 `[storage].encryption` 后旧文件仍然可读。
        let encryption = match header.get("encryption").and_then(Value::as_str) {
            Some("none") => EncryptionMode::None,
            Some("xchacha20poly1305") => EncryptionMode::XChaCha20Poly1305,
            // 旧文件没有该字段时退回当前配置。
            _ => self.encryption,
        };
        let Some(line) = lines.get(line_no) else {
            return Ok(None);
        };
        let value: Value = serde_json::from_str(line)?;
        let scope = value
            .get("scope")
            .and_then(Value::as_str)
            .context("message line has no scope")?;
        let peer = value
            .get("peer_id")
            .and_then(Value::as_u64)
            .context("message line has no peer_id")?;
        let message_id = value
            .get("id")
            .and_then(Value::as_i64)
            .context("message line has no id")?;
        if let Some(expected) = expected_id
            && expected != message_id
        {
            warn!(
                file = %path.display(),
                line = line_no,
                expected,
                actual = message_id,
                "message index line points at a different message"
            );
            return Ok(None);
        }
        let ct = base64::engine::general_purpose::STANDARD
            .decode(value.get("ct").and_then(Value::as_str).unwrap_or_default())?;

        let plain = match encryption {
            EncryptionMode::XChaCha20Poly1305 => {
                let nonce = base64::engine::general_purpose::STANDARD.decode(
                    value
                        .get("nonce")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )?;
                let mut sealed = Vec::with_capacity(nonce.len() + ct.len());
                sealed.extend_from_slice(&nonce);
                sealed.extend_from_slice(&ct);
                let aad = format!("message:{scope}:{peer}:{message_id}:{file_id}:{line_no}");
                self.key.open("message", aad.as_bytes(), &sealed)?
            }
            EncryptionMode::None => ct,
        };
        Ok(Some(serde_json::from_slice(&plain)?))
    }

    /// 写入前腾出空间。
    fn make_room(&self) -> Result<()> {
        // 先按年龄清理；`retention_days = 0` 时不做任何事。
        if self.retention_days > 0 {
            self.enforce_retention(self.retention_days)?;
        }
        if self.max_bytes == 0 {
            return Ok(());
        }
        let (bytes, _) = self.usage()?;
        if bytes <= self.max_bytes {
            return Ok(());
        }
        let report = self.enforce_retention(self.retention_days)?;
        let (bytes, _) = self.usage()?;
        if bytes > self.max_bytes {
            warn!(
                bytes,
                limit = self.max_bytes,
                removed = report.removed_files,
                "message store still over quota after cleanup"
            );
        }
        Ok(())
    }
}

/// 路径中的 scope 与 peer（与设计 §8.2 的目录一致）。
fn scope_peer(chat: ChatId) -> (&'static str, u64) {
    match chat {
        ChatId::Group(id) => ("groups", id),
        ChatId::Private(id) => ("private", id),
    }
}

/// 读取文件头里的 `file_id`。
fn read_file_id(path: &Path) -> Result<Option<String>> {
    let file = std::fs::File::open(path)?;
    let Some(first) = BufReader::new(file).lines().next() else {
        return Ok(None);
    };
    let first = first?;
    let Ok(value) = serde_json::from_str::<Value>(&first) else {
        return Ok(None);
    };
    Ok(value
        .get("file_id")
        .and_then(Value::as_str)
        .map(str::to_owned))
}

/// 生成文件 id：UTC 时间 + 随机后缀，字典序即时间序。
fn new_file_id() -> String {
    let mut suffix = [0u8; 4];
    let _ = getrandom::fill(&mut suffix);
    format!(
        "{}-{}",
        Utc::now().format("%Y%m%dT%H%M%S%.fZ"),
        hex::encode(suffix)
    )
}

/// 递归列出目录下的文件。
fn walk_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// 当前本地月份，供测试与状态展示。
pub fn current_month() -> String {
    let now = Local::now();
    format!("{:04}-{:02}", now.year(), now.month())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qq::event::Sender;
    use chrono::TimeZone;

    fn store() -> (MessageStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([5u8; 32]));
        let store = MessageStore::open(
            db,
            key,
            dir.path().to_path_buf(),
            0,
            EncryptionMode::XChaCha20Poly1305,
        )
        .unwrap();
        (store, dir)
    }

    fn message(chat: ChatId, id: i64, text: &str) -> QqMessage {
        QqMessage {
            message_id: id,
            chat,
            sender: Sender {
                qq: Some(10001),
                nickname: "Alice".to_owned(),
                card: String::new(),
                role: "member".to_owned(),
            },
            time: Local
                .with_ymd_and_hms(2026, 10, 29, 21, 4, 3)
                .unwrap()
                .with_timezone(&Utc),
            segments: vec![crate::qq::event::Segment::Text {
                text: text.to_owned(),
            }],
            reply_to: None,
            is_self: false,
            backfilled: false,
            raw: Value::Null,
        }
    }

    #[test]
    fn append_deduplicates_and_roundtrips() {
        let (store, dir) = store();
        let chat = ChatId::Group(123);
        assert!(store.append(&message(chat, 1, "hello")).unwrap());
        assert!(!store.append(&message(chat, 1, "hello")).unwrap());

        let loaded = store.load(chat, 1).unwrap().unwrap();
        assert_eq!(loaded.plain_text(), "hello");

        // 文件存在、头行明文、正文不可见。
        let path = dir.path().join("messages/groups/123/2026-10/29.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let header: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(header["kind"], "header");
        assert_eq!(header["encryption"], "xchacha20poly1305");
        assert!(!text.contains("hello"));
        assert!(text.contains("\"id\":1"));
    }

    #[test]
    fn recent_and_day_views_are_ordered() {
        let (store, _dir) = store();
        let chat = ChatId::Private(42);
        for id in 1..=3 {
            let mut message = message(chat, id, &format!("m{id}"));
            message.time = Utc
                .with_ymd_and_hms(2026, 10, 29, 20, id as u32, 0)
                .unwrap();
            store.append(&message).unwrap();
        }
        let recent = store.recent(chat, 2).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].plain_text(), "m2");
        assert_eq!(recent[1].plain_text(), "m3");

        // 分区按本地日期；测试机时区不一定是 UTC，按实际本地日查询。
        let local_day = recent[0]
            .time
            .with_timezone(&Local)
            .format("%Y-%m-%d")
            .to_string();
        let day = store.messages_for_day(chat, &local_day).unwrap();
        assert_eq!(day.len(), 3);
    }

    #[test]
    fn unread_mark_read_and_dismiss() {
        let (store, _dir) = store();
        let chat = ChatId::Group(123);
        store.append(&message(chat, 1, "a")).unwrap();
        store.append(&message(chat, 2, "b")).unwrap();
        assert_eq!(store.unread_counts().unwrap(), vec![(chat, 2)]);
        assert_eq!(store.unread(None).unwrap().len(), 2);

        assert_eq!(store.mark_read(chat, &[1]).unwrap(), 1);
        assert_eq!(store.state(chat, 1).unwrap(), Some(InboxState::Read));
        assert_eq!(store.unread_counts().unwrap(), vec![(chat, 1)]);

        store.dismiss(chat, &[2]).unwrap();
        assert_eq!(store.state(chat, 2).unwrap(), Some(InboxState::Dismissed));
        // dismissed 不会被 read 覆盖。
        store.set_state(chat, 2, InboxState::Read).unwrap();
        assert_eq!(store.state(chat, 2).unwrap(), Some(InboxState::Dismissed));
        assert!(store.unread_counts().unwrap().is_empty());
    }

    #[test]
    fn self_messages_never_enter_inbox() {
        let (store, _dir) = store();
        let mut message = message(ChatId::Group(123), 9, "self");
        message.is_self = true;
        message.sender.qq = None;
        store.append(&message).unwrap();
        assert!(store.unread_counts().unwrap().is_empty());
        assert_eq!(
            store.state(ChatId::Group(123), 9).unwrap(),
            Some(InboxState::Read)
        );
    }

    #[test]
    fn cursor_roundtrip() {
        let (store, _dir) = store();
        let chat = ChatId::Group(1);
        assert_eq!(store.cursor(chat).unwrap(), None);
        store.set_cursor(chat, 7, "2026-10-29T00:00:00Z").unwrap();
        assert_eq!(
            store.cursor(chat).unwrap(),
            Some((7, "2026-10-29T00:00:00Z".to_owned()))
        );
        assert_eq!(store.known_chats().unwrap(), vec![chat]);
    }

    #[test]
    fn outbound_ledger_tracks_unconfirmed() {
        let (store, _dir) = store();
        let chat = ChatId::Private(42);
        store.begin_send("i1", chat, "hello").unwrap();
        assert_eq!(store.unconfirmed_sends().unwrap().len(), 1);
        store.finish_send("i1", "sent", Some(100)).unwrap();
        assert!(store.unconfirmed_sends().unwrap().is_empty());
    }

    #[test]
    fn rebuild_index_recovers_missing_rows() {
        let (store, _dir) = store();
        let chat = ChatId::Group(123);
        store.append(&message(chat, 1, "a")).unwrap();
        store.append(&message(chat, 2, "b")).unwrap();

        {
            let conn = store.db.lock();
            conn.execute("DELETE FROM messages", []).unwrap();
        }
        assert!(store.recent(chat, 10).unwrap().is_empty());
        let repaired = store.rebuild_index().unwrap();
        assert_eq!(repaired, 2);
        assert_eq!(store.recent(chat, 10).unwrap().len(), 2);
    }

    #[test]
    fn concurrent_appends_keep_distinct_line_numbers() {
        let (store, dir) = store();
        let store = std::sync::Arc::new(store);
        let chat = ChatId::Group(1);
        std::thread::scope(|scope| {
            for id in 1..=16i64 {
                let store = std::sync::Arc::clone(&store);
                scope.spawn(move || {
                    store
                        .append(&message(chat, id, &format!("body-{id}")))
                        .unwrap();
                });
            }
        });

        for id in 1..=16i64 {
            let loaded = store.load(chat, id).unwrap();
            assert!(loaded.is_some(), "message {id} disappeared");
            let loaded = loaded.unwrap();
            assert_eq!(loaded.message_id, id);
            match &loaded.segments[0] {
                crate::qq::event::Segment::Text { text } => assert_eq!(text, &format!("body-{id}")),
                other => panic!("unexpected {other:?}"),
            }
        }

        // 文件里除表头外恰好 16 行，没有因为并发写重行号而丢行。
        let path = dir.path().join("messages/groups/1/2026-10/29.jsonl");
        let lines = std::fs::read_to_string(path).unwrap().lines().count();
        assert_eq!(lines, 17);
    }

    #[test]
    fn index_drift_does_not_return_the_wrong_message() {
        let (store, _dir) = store();
        let chat = ChatId::Group(1);
        store.append(&message(chat, 1, "first")).unwrap();
        store.append(&message(chat, 2, "second")).unwrap();

        {
            // 模拟崩溃后索引错位：id 2 指向 id 1 的行。
            let conn = store.db.lock();
            conn.execute(
                "UPDATE messages SET line_no = 1 WHERE chat = ?1 AND message_id = 2",
                params![chat.to_string()],
            )
            .unwrap();
        }

        assert!(store.load(chat, 2).unwrap().is_none());
        assert!(
            store
                .recent(chat, 10)
                .unwrap()
                .iter()
                .all(|m| m.message_id != 2)
        );
    }

    #[test]
    fn reads_follow_the_file_header_not_the_current_config() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([6u8; 32]));
        let chat = ChatId::Group(1);
        let root = dir.path().to_path_buf();
        {
            let store = MessageStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                root.clone(),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap();
            store.append(&message(chat, 1, "ciphertext")).unwrap();
        }
        // 同一个数据目录按“明文”配置重新打开：文件头写着 xchacha，读取必须照此解密。
        let reopened = MessageStore::open(db, key, root, 0, EncryptionMode::None).unwrap();
        let loaded = reopened.load(chat, 1).unwrap().unwrap();
        match &loaded.segments[0] {
            crate::qq::event::Segment::Text { text } => assert_eq!(text, "ciphertext"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rebuild_restores_file_counts_and_inbox_state() {
        let (store, _dir) = store();
        let chat = ChatId::Group(1);
        store.append(&message(chat, 1, "a")).unwrap();
        store.append(&message(chat, 2, "b")).unwrap();
        {
            let conn = store.db.lock();
            conn.execute("DELETE FROM message_files", []).unwrap();
            conn.execute("DELETE FROM inbox_state", []).unwrap();
            conn.execute("DELETE FROM messages", []).unwrap();
        }
        assert!(store.unread(None).unwrap().is_empty());

        assert_eq!(store.rebuild_index().unwrap(), 2);
        assert_eq!(store.load(chat, 1).unwrap().unwrap().message_id, 1);
        assert_eq!(store.unread_counts().unwrap(), vec![(chat, 2)]);
        let (disk, indexed) = store.index_file_counts().unwrap();
        assert_eq!(disk, indexed);
        assert_eq!(disk, 1);
    }

    #[test]
    fn configured_retention_days_clean_on_append() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([13u8; 32]));
        let store = MessageStore::open(
            db,
            key,
            dir.path().to_path_buf(),
            0,
            EncryptionMode::XChaCha20Poly1305,
        )
        .unwrap()
        .with_retention_days(30);
        let chat = ChatId::Group(1);
        let mut old = message(chat, 1, "old");
        old.time = Utc::now() - chrono::Duration::days(90);
        store.append(&old).unwrap();

        let mut current = message(chat, 2, "new");
        current.time = Utc::now();
        store.append(&current).unwrap();

        assert!(store.load(chat, 1).unwrap().is_none());
        assert!(store.load(chat, 2).unwrap().is_some());
    }

    #[test]
    fn retention_removes_orphan_inbox_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([7u8; 32]));
        let store = MessageStore::open(
            db,
            key,
            dir.path().to_path_buf(),
            1,
            EncryptionMode::XChaCha20Poly1305,
        )
        .unwrap();
        let mut old = message(ChatId::Group(1), 1, "old");
        old.time = Utc.with_ymd_and_hms(2026, 1, 5, 0, 0, 0).unwrap();
        store.append(&old).unwrap();
        assert_eq!(store.unread_counts().unwrap().len(), 1);

        store.enforce_retention(0).unwrap();
        assert!(store.unread_counts().unwrap().is_empty());
    }

    #[test]
    fn tampered_line_fails_to_decrypt() {
        let (store, dir) = store();
        let chat = ChatId::Group(1);
        store.append(&message(chat, 1, "a")).unwrap();
        store.append(&message(chat, 2, "b")).unwrap();
        let path = dir.path().join("messages/groups/1/2026-10/29.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        lines.swap(1, 2); // 交换两行：AAD 里的 line_no 不匹配。
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        // 行内 id 与索引不一致：宁可当作缺失，也不能返回错误的消息。
        assert!(store.load(chat, 1).unwrap().is_none());
        assert!(store.load(chat, 2).unwrap().is_none());
    }

    #[test]
    fn stale_pending_sends_become_unknown() {
        let (store, _dir) = store();
        let chat = ChatId::Group(1);
        store.begin_send("i1", chat, "send_group_msg").unwrap();
        {
            let conn = store.db.lock();
            conn.execute(
                "UPDATE outbound_intents SET created_at = '2020-01-01T00:00:00+00:00' WHERE id = 'i1'",
                [],
            )
            .unwrap();
        }
        assert_eq!(store.mark_stale_intents(60).unwrap(), 1);
        // 不是 pending 了，但仍然是“未确认”，不会被误当作可安全重试。
        let unconfirmed = store.unconfirmed_sends().unwrap();
        assert_eq!(unconfirmed.len(), 1);
        assert_eq!(unconfirmed[0].0, "i1");
    }

    #[test]
    fn retention_never_removes_current_month() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([5u8; 32]));
        let store = MessageStore::open(
            db,
            key,
            dir.path().to_path_buf(),
            1,
            EncryptionMode::XChaCha20Poly1305,
        )
        .unwrap();

        // 用相对时间，避免测试在跨月后失败：旧消息至少早 40 天，必然落在别的月份。
        let mut old = message(ChatId::Group(1), 1, "old");
        old.time = Utc::now() - chrono::Duration::days(40);
        store.append(&old).unwrap();

        let mut current = message(ChatId::Group(1), 2, "now");
        current.time = Utc::now();
        store.append(&current).unwrap();

        // `append` 在写入前已经按配额清掉了旧月份；这里再跑一次应无事可做，
        // 关键是当前月份的消息始终保留。
        let report = store.enforce_retention(0).unwrap();
        assert_eq!(report.removed_files, 0);
        assert!(store.load(ChatId::Group(1), 2).unwrap().is_some());
        assert!(store.load(ChatId::Group(1), 1).unwrap().is_none());
    }

    #[test]
    fn age_retention_removes_old_months() {
        let (store, _dir) = store();
        let mut old = message(ChatId::Group(1), 1, "old");
        old.time = Utc.with_ymd_and_hms(2020, 1, 5, 0, 0, 0).unwrap();
        store.append(&old).unwrap();
        let current = message(ChatId::Group(1), 2, "now");
        store.append(&current).unwrap();

        let report = store.enforce_retention(30).unwrap();
        assert_eq!(report.removed_messages, 1);
        assert!(store.load(ChatId::Group(1), 1).unwrap().is_none());
        assert!(store.load(ChatId::Group(1), 2).unwrap().is_some());
    }

    #[test]
    fn plaintext_mode_is_readable_and_marked() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([5u8; 32]));
        let store =
            MessageStore::open(db, key, dir.path().to_path_buf(), 0, EncryptionMode::None).unwrap();
        let chat = ChatId::Group(1);
        store.append(&message(chat, 1, "visible")).unwrap();
        let path = dir.path().join("messages/groups/1/2026-10/29.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"encryption\":\"none\""));
        assert_eq!(
            store.load(chat, 1).unwrap().unwrap().plain_text(),
            "visible"
        );
    }
}
