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
    ///
    /// 单个文件或单行的问题（读不出、文件头不完整、行解不开）只记警告并跳过，不让
    /// 整个重建失败：否则一行坏数据会让其余文件永远补不上索引，而启动时的文件数校验
    /// 每次都会再触发一次注定失败的重建。返回新补的 `messages` 行数。
    pub(crate) fn rebuild_index(&self) -> Result<u64> {
        let messages_dir = self.root.join("messages");
        if !messages_dir.exists() {
            return Ok(0);
        }
        let mut repaired = 0u64;
        let mut files: HashMap<String, FileRow> = HashMap::new();
        for entry in walk_files(&messages_dir)? {
            let (message_file, mut row) = match self.read_for_index(&entry) {
                Ok(Some(read)) => read,
                Ok(None) => continue,
                Err(error) => {
                    warn!(file = %entry.display(), %error, "cannot read a message file; skipped");
                    continue;
                }
            };
            repaired += self.index_lines(&message_file, &mut row)?;
            files.insert(message_file.path, row);
        }
        self.sync_file_rows(&files)?;
        Ok(repaired)
    }

    /// 读入一个消息文件并解析文件头。文件头不是 JSON、或缺 `scope` / `peer_id` / `date`
    /// 时记警告并返回 `None`（这种文件不进 `message_files`）；I/O 错误返回 `Err`。
    fn read_for_index(&self, entry: &Path) -> Result<Option<(MessageFile, FileRow)>> {
        let relative = entry
            .strip_prefix(&self.root)?
            .to_string_lossy()
            .replace('\\', "/");
        let lines: Vec<String> = BufReader::new(std::fs::File::open(entry)?)
            .lines()
            .collect::<Result<Vec<_>, _>>()?;
        let header = lines
            .first()
            .and_then(|line| serde_json::from_str::<Value>(line).ok());
        let fields = header.as_ref().and_then(|header| {
            Some((
                header.get("scope").and_then(Value::as_str)?,
                header.get("peer_id").and_then(Value::as_u64)?,
                header.get("date").and_then(Value::as_str)?,
            ))
        });
        let (Some(header), Some((scope, peer, day))) = (&header, fields) else {
            warn!(file = %relative, "message file has no usable header; skipped");
            return Ok(None);
        };
        let row = FileRow {
            scope: scope.to_owned(),
            peer: peer as i64,
            month: day.get(..7).unwrap_or(day).to_owned(),
            day: day.to_owned(),
            count: 0,
            bytes: 0,
        };
        // 行已读进内存，后面的逐行解码复用这份副本，不再按索引重新读文件。
        let message_file = MessageFile {
            path: relative,
            file_id: header
                .get("file_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            lines,
        };
        Ok(Some((message_file, row)))
    }

    /// 逐行补 `messages` 里缺的索引行，同时把文件的消息数与字节数累计进 `row`。
    ///
    /// 字节数按每行长度 + 换行符计（与写入时一致）；不是 JSON、缺会话或 id 的行不计数。
    /// 解不开的行（损坏、篡改、密钥不符）记警告后跳过；数据库错误返回 `Err`。
    fn index_lines(&self, file: &MessageFile, row: &mut FileRow) -> Result<u64> {
        let mut repaired = 0u64;
        for (line_no, line) in file.lines.iter().enumerate().skip(1) {
            row.bytes += line.len() as u64 + 1;
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(chat) = line_chat(&value) else {
                continue;
            };
            let Some(message_id) = value.get("id").and_then(Value::as_i64) else {
                continue;
            };
            row.count += 1;
            let already: bool = self
                .db
                .lock()
                .query_row(
                    "SELECT 1 FROM messages WHERE chat = ?1 AND message_id = ?2",
                    params![chat.to_string(), message_id],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if already {
                continue;
            }
            let message = match self.decode_message_at(file, line_no, Some(message_id)) {
                Ok(Some(message)) => message,
                Ok(None) => continue,
                Err(error) => {
                    warn!(file = %file.path, line = line_no, %error, "cannot decode a message line; skipped");
                    continue;
                }
            };
            let local = message.time.with_timezone(&Local);
            self.db.lock().execute(
                "INSERT INTO messages
                    (chat, message_id, sender, ts, day, file_path, line_no, reply_to, is_self)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    chat.to_string(),
                    message_id,
                    message.sender.label(),
                    message.time.to_rfc3339(),
                    local.format("%Y-%m-%d").to_string(),
                    file.path,
                    line_no as i64,
                    message.reply_to,
                    message.is_self as i32
                ],
            )?;
            repaired += 1;
        }
        Ok(repaired)
    }

    /// 用重建结果覆盖 `message_files`（磁盘上已不存在的文件连同配额一起删掉），再让
    /// `inbox_state` 与 `messages` 一致：缺的补默认状态（自身消息已读），多的删掉。
    fn sync_file_rows(&self, files: &HashMap<String, FileRow>) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self.db.lock();
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
        for (path, row) in files {
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
                    row.scope,
                    row.peer,
                    row.month,
                    row.day,
                    row.count as i64,
                    row.bytes as i64,
                    now
                ],
            )?;
        }
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
        Ok(())
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
                match open_message_file(&self.root.join(&file_path)) {
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

/// 重建中的一个消息文件在 `message_files` 里的一行。
struct FileRow {
    /// 会话类型目录：`groups` / `private`。
    scope: String,
    /// 群号或对端 QQ。
    peer: i64,
    /// `YYYY-MM`，月视图按它分组。
    month: String,
    /// `YYYY-MM-DD`，文件对应的日期。
    day: String,
    /// 可识别的消息行数。
    count: u64,
    /// 消息行的字节数（不含文件头），计入配额。
    bytes: u64,
}

/// 消息行所属的会话；`scope` 不是 `groups` / `private` 或缺 `peer_id` 时返回 `None`。
fn line_chat(value: &Value) -> Option<ChatId> {
    let peer = value.get("peer_id").and_then(Value::as_u64)?;
    match value.get("scope").and_then(Value::as_str)? {
        "groups" => Some(ChatId::Group(peer)),
        "private" => Some(ChatId::Private(peer)),
        _ => None,
    }
}
