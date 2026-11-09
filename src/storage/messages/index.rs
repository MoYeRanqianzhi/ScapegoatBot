use super::codec::{MessageFile, open_message_file};
use super::*;

impl MessageStore {
    /// 用量：字节数与消息条数。
    pub(crate) fn usage(&self) -> Result<(u64, u64)> {
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
    pub(crate) fn index_file_counts(&self) -> Result<(u64, u64)> {
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
    pub(crate) fn rebuild_index(&self) -> Result<u64> {
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
            let file_id = header
                .get("file_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let encryption = match header.get("encryption").and_then(Value::as_str) {
                Some("none") => EncryptionMode::None,
                Some("xchacha20poly1305") => EncryptionMode::XChaCha20Poly1305,
                _ => self.encryption,
            };
            // 行已读进内存，后面的逐行解码复用这份副本，不再按索引重新读文件。
            let message_file = MessageFile {
                path: relative.clone(),
                file_id,
                encryption,
                lines,
            };
            let mut count = 0u64;
            let mut bytes = 0u64;
            for (line_no, line) in message_file.lines.iter().enumerate().skip(1) {
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
                let Some(message) =
                    self.decode_message_at(&message_file, line_no, Some(message_id))?
                else {
                    continue;
                };
                let local = message.time.with_timezone(&Local);
                let conn = self.db.lock();
                conn.execute(
                    "INSERT INTO messages
                        (chat, message_id, sender, ts, day, file_path, line_no, reply_to, is_self)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        chat.to_string(),
                        message_id,
                        message.sender.label(),
                        message.time.to_rfc3339(),
                        local.format("%Y-%m-%d").to_string(),
                        relative,
                        line_no as i64,
                        message.reply_to,
                        message.is_self as i32
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

    /// 从索引行读取消息；同一文件的索引行复用一次文件读取。
    pub(super) fn load_indexed(
        &self,
        sql: &str,
        params: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<QqMessage>> {
        let rows: Vec<(String, i64, i64)> = {
            let conn = self.db.lock();
            let mut statement = conn.prepare(sql)?;
            statement
                .query_map(params, |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut messages = Vec::with_capacity(rows.len());
        let mut files: HashMap<String, Option<MessageFile>> = HashMap::new();
        for (file_path, line_no, message_id) in rows {
            let file = files.entry(file_path.clone()).or_insert_with(|| {
                match open_message_file(&self.root.join(&file_path), self.encryption) {
                    Ok(file) => file,
                    Err(error) => {
                        warn!(%error, file = %file_path, "cannot open message file");
                        None
                    }
                }
            });
            let Some(file) = file.as_ref() else {
                continue;
            };
            match self.decode_message_at(file, line_no as usize, Some(message_id)) {
                Ok(Some(message)) => messages.push(message),
                Ok(None) => {}
                Err(error) => {
                    warn!(%error, file = %file_path, line = line_no, "cannot read message");
                }
            }
        }
        Ok(messages)
    }
}
