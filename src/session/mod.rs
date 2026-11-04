//! Transcript 分段。
//!
//! 设计依据：设计 §8.1。
//!
//! - 整个 QQ 号只有一份逻辑 transcript（`shared`）；文件是分段，不是会话实体。
//! - 文件：`sessions/<YYYYMMDDTHHMMSSZ>-<seq>.jsonl`，名称字典序即时间序。
//! - 首行明文文件头；之后每行一条 `AgentMessage`，AAD 绑定 `session:<file_id>:<line_no>`。
//! - `transcript_files` 只是可重建索引；`/sessions` 视图由内容动态渲染。
//! - 每次压缩成功后轮转新分段（由 runner 调用 [`SessionStore::rotate`]）。

pub mod compaction;
pub mod turn_buffer;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::agent::message::AgentMessage;
use crate::config::EncryptionMode;
use crate::storage::Database;
use crate::storage::crypto::MasterKey;

/// 行格式版本。
const LINE_VERSION: u32 = 1;
/// 检查点摘要的加密域。
const CHECKPOINT_PURPOSE: &str = "compaction-checkpoint";
/// 保留的检查点条数。
const CHECKPOINT_KEEP: u32 = 20;

/// 一个 transcript 分段的元信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFile {
    /// 虚拟路径 `/sessions/<file>` 中的文件名。
    pub file: String,
    pub file_id: String,
    pub prev_file: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub message_count: u64,
    pub bytes: u64,
}

/// 分段存储。
pub struct SessionStore {
    db: Arc<Database>,
    key: Arc<MasterKey>,
    root: PathBuf,
    max_bytes: u64,
    encryption: EncryptionMode,
    /// `None` = 全局视图（列出全部分段）；`Some` = 某个 chat 的隔离链。
    scope: Option<String>,
    /// 按年龄清理已关闭的分段；0 = 只按配额。
    retention_days: u32,
    /// 同一分段的 append/read 串行化，避免行号竞争。
    append_lock: std::sync::Arc<std::sync::Mutex<()>>,
}

impl SessionStore {
    /// 打开并确保 `sessions/` 存在。
    pub fn open(
        db: Arc<Database>,
        key: Arc<MasterKey>,
        root: PathBuf,
        max_bytes: u64,
        encryption: EncryptionMode,
    ) -> Result<Self> {
        std::fs::create_dir_all(root.join("sessions"))
            .with_context(|| format!("cannot create {}", root.join("sessions").display()))?;
        Ok(Self {
            db,
            key,
            root,
            max_bytes,
            encryption,
            scope: None,
            retention_days: 0,
            append_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        })
    }

    /// 为某个 chat 构造隔离的链；共享文件与索引，按 `scope` 过滤。
    pub fn for_scope(&self, scope: &str) -> Self {
        Self {
            db: Arc::clone(&self.db),
            key: Arc::clone(&self.key),
            root: self.root.clone(),
            max_bytes: self.max_bytes,
            encryption: self.encryption,
            scope: Some(scope.to_owned()),
            retention_days: self.retention_days,
            append_lock: std::sync::Arc::clone(&self.append_lock),
        }
    }

    /// 设置按年龄清理的天数（0 = 关闭）。
    pub fn with_retention_days(mut self, days: u32) -> Self {
        self.retention_days = days;
        self
    }

    /// 当前作用域名；全局视图按 `shared` 处理。
    fn scope_name(&self) -> &str {
        self.scope.as_deref().unwrap_or("shared")
    }

    /// 数据库中已有的作用域（用于 per_chat 崩溃恢复）。
    pub fn scopes(&self) -> Result<Vec<String>> {
        let conn = self.db.lock();
        let mut statement = conn.prepare(
            "SELECT DISTINCT COALESCE(scope, 'shared') FROM transcript_files ORDER BY 1",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// 当前（未关闭）分段；不存在时创建一个。
    pub fn current(&self) -> Result<SessionFile> {
        let existing: Option<String> = {
            let conn = self.db.lock();
            conn.query_row(
                "SELECT file FROM transcript_files
                 WHERE ended_at IS NULL AND COALESCE(scope, 'shared') = ?1
                 ORDER BY started_at DESC LIMIT 1",
                params![self.scope_name()],
                |row| row.get(0),
            )
            .optional()?
        };
        match existing {
            Some(file) => self.info(&file)?.ok_or_else(|| {
                anyhow::anyhow!("current transcript file {file} is missing from the index")
            }),
            None => {
                let prev = self.latest_closed()?;
                self.create(prev.as_deref())
            }
        }
    }

    /// 当前分段的虚拟路径，供 system prompt 的 `current_session` 使用。
    pub fn current_path(&self) -> Result<String> {
        Ok(format!("/sessions/{}", self.current()?.file))
    }

    /// 追加消息到当前分段。
    pub fn append(&self, messages: &[AgentMessage]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let _guard = self
            .append_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = self.current()?;
        let path = self.root.join("sessions").join(&current.file);
        let mut count = current.message_count;
        let mut added_bytes = 0u64;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;

        for message in messages {
            let line_no = count as usize + 1; // 第 0 行是文件头
            let line = self.encode_line(message, &current.file_id, line_no)?;
            file.write_all(line.as_bytes())?;
            added_bytes += line.len() as u64;
            count += 1;
        }
        file.flush()?;

        let conn = self.db.lock();
        conn.execute(
            "UPDATE transcript_files
             SET message_count = ?1, bytes = bytes + ?2
             WHERE file = ?3",
            params![count as i64, added_bytes as i64, current.file],
        )?;
        Ok(())
    }

    /// 读取整个分段。
    pub fn read_segment(&self, file: &str) -> Result<Vec<AgentMessage>> {
        let _guard = self
            .append_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let file_name = file;
        let path = self.root.join("sessions").join(file);
        let handle = std::fs::File::open(&path)
            .with_context(|| format!("cannot open transcript {}", path.display()))?;
        let lines: Vec<String> = BufReader::new(handle)
            .lines()
            .collect::<Result<Vec<_>, _>>()?;
        let Some(header) = lines.first() else {
            return Ok(Vec::new());
        };
        let header: Value = serde_json::from_str(header)?;
        let file_id = header
            .get("file_id")
            .and_then(Value::as_str)
            .context("transcript header has no file_id")?
            .to_owned();

        let mut messages = Vec::with_capacity(lines.len().saturating_sub(1));
        for (line_no, line) in lines.iter().enumerate().skip(1) {
            match self.decode_line(line, &file_id, line_no) {
                Ok(message) => messages.push(message),
                Err(error) => {
                    // 单个坏行不等于整个分段不可用；记录并跳过。
                    warn!(%error, file = %file_name, line_no, "skipping unreadable transcript line");
                }
            }
        }
        Ok(messages)
    }

    /// 列出分段（从新到旧）；全局视图列出全部，隔离视图只列自己的链。
    pub fn list(&self) -> Result<Vec<SessionFile>> {
        let conn = self.db.lock();
        let mut rows = match &self.scope {
            None => {
                let mut statement = conn.prepare(
                    "SELECT file, file_id, prev_file, started_at, ended_at, message_count, bytes
                     FROM transcript_files ORDER BY started_at DESC",
                )?;
                statement
                    .query_map([], row_to_file)?
                    .collect::<Result<Vec<_>, _>>()?
            }
            Some(scope) => {
                let mut statement = conn.prepare(
                    "SELECT file, file_id, prev_file, started_at, ended_at, message_count, bytes
                     FROM transcript_files
                     WHERE COALESCE(scope, 'shared') = ?1
                     ORDER BY started_at DESC",
                )?;
                statement
                    .query_map(params![scope], row_to_file)?
                    .collect::<Result<Vec<_>, _>>()?
            }
        };
        rows.shrink_to_fit();
        Ok(rows)
    }

    /// 轮转：关闭当前分段并创建新分段（压缩成功后调用）。
    pub fn rotate(&self) -> Result<SessionFile> {
        let current = self.current()?;
        {
            let conn = self.db.lock();
            conn.execute(
                "UPDATE transcript_files SET ended_at = ?1 WHERE file = ?2",
                params![Utc::now().to_rfc3339(), current.file],
            )?;
        }
        let next = self.create(Some(&current.file))?;
        // 轮转后顺手清理过期的已关闭分段；当前分段不会被删除。
        if self.retention_days > 0
            && let Err(error) = self.enforce_retention(self.retention_days)
        {
            warn!(%error, "cannot enforce transcript retention");
        }
        Ok(next)
    }

    /// 记录一次压缩检查点（加密存储）。
    ///
    /// 设计 §8.1：`compaction_checkpoints` 用于崩溃恢复与审计；
    /// 分段首行的摘要仍是权威，这里的行只是可查询的缓存。
    /// 只保留最近 [`CHECKPOINT_KEEP`] 条，避免无限增长。
    pub fn record_checkpoint(&self, from_file: &str, summary: &str) -> Result<()> {
        let sealed = match self.encryption {
            EncryptionMode::XChaCha20Poly1305 => {
                self.key
                    .seal(CHECKPOINT_PURPOSE, from_file.as_bytes(), summary.as_bytes())?
            }
            EncryptionMode::None => summary.as_bytes().to_vec(),
        };
        let conn = self.db.lock();
        conn.execute(
            "INSERT INTO compaction_checkpoints (from_file, summary_enc, created_at)
             VALUES (?1, ?2, ?3)",
            params![from_file, sealed, Utc::now().to_rfc3339()],
        )?;
        conn.execute(
            "DELETE FROM compaction_checkpoints
             WHERE id NOT IN (SELECT id FROM compaction_checkpoints ORDER BY id DESC LIMIT ?1)",
            params![CHECKPOINT_KEEP],
        )?;
        Ok(())
    }

    /// 最近的检查点：`(from_file, summary, created_at)`，最新在前。
    pub fn checkpoints(&self, limit: usize) -> Result<Vec<(String, String, String)>> {
        let conn = self.db.lock();
        let mut statement = conn.prepare(
            "SELECT from_file, summary_enc, created_at FROM compaction_checkpoints
             ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map(params![limit as u32], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (from_file, sealed, created_at) = row?;
            let summary = match self.encryption {
                EncryptionMode::XChaCha20Poly1305 => {
                    let plain = self
                        .key
                        .open(CHECKPOINT_PURPOSE, from_file.as_bytes(), &sealed)?;
                    String::from_utf8(plain).context("checkpoint summary is not valid UTF-8")?
                }
                EncryptionMode::None => {
                    String::from_utf8(sealed).context("checkpoint summary is not valid UTF-8")?
                }
            };
            out.push((from_file, summary, created_at));
        }
        Ok(out)
    }

    /// 用量：字节数与消息条数；隔离视图只统计本链。
    pub fn usage(&self) -> Result<(u64, u64)> {
        let conn = self.db.lock();
        let (bytes, count) = match &self.scope {
            None => conn.query_row(
                "SELECT COALESCE(SUM(bytes), 0), COALESCE(SUM(message_count), 0)
                 FROM transcript_files WHERE pruned_at IS NULL",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?,
            Some(scope) => conn.query_row(
                "SELECT COALESCE(SUM(bytes), 0), COALESCE(SUM(message_count), 0)
                 FROM transcript_files
                 WHERE pruned_at IS NULL AND COALESCE(scope, 'shared') = ?1",
                params![scope],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?,
        };
        Ok((bytes, count))
    }

    /// 按年龄与配额删除最旧的**已关闭**分段，绝不删除当前分段。
    pub fn enforce_retention(&self, retention_days: u32) -> Result<u64> {
        let mut removed = 0u64;
        loop {
            let (bytes, _) = self.usage()?;
            let over = self.max_bytes != 0 && bytes > self.max_bytes;
            let cutoff = if retention_days == 0 {
                None
            } else {
                Some(Utc::now() - chrono::Duration::days(i64::from(retention_days)))
            };

            let oldest: Option<(String, String, u64)> = {
                let conn = self.db.lock();
                conn.query_row(
                    "SELECT file, started_at, bytes FROM transcript_files
                     WHERE ended_at IS NOT NULL AND pruned_at IS NULL
                       AND COALESCE(scope, 'shared') = ?1
                     ORDER BY started_at ASC LIMIT 1",
                    params![self.scope_name()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
            };
            let Some((file, started_at, file_bytes)) = oldest else {
                break;
            };
            let expired = cutoff.as_ref().is_some_and(|cutoff| {
                DateTime::parse_from_rfc3339(&started_at)
                    .map(|time| time.with_timezone(&Utc) < *cutoff)
                    .unwrap_or(false)
            });
            if !over && !expired {
                break;
            }
            let path = self.root.join("sessions").join(&file);
            if let Err(error) = std::fs::remove_file(&path) {
                warn!(%error, path = %path.display(), "cannot remove transcript segment");
            }
            let conn = self.db.lock();
            conn.execute(
                "UPDATE transcript_files SET pruned_at = ?1 WHERE file = ?2",
                params![Utc::now().to_rfc3339(), file],
            )?;
            removed += file_bytes;
        }
        Ok(removed)
    }

    /// 分段的索引信息。
    pub fn info(&self, file: &str) -> Result<Option<SessionFile>> {
        let conn = self.db.lock();
        Ok(conn
            .query_row(
                "SELECT file, file_id, prev_file, started_at, ended_at, message_count, bytes
                 FROM transcript_files WHERE file = ?1",
                params![file],
                row_to_file,
            )
            .optional()?)
    }

    /// 最新的已关闭分段，用于链式 `prev_file`。
    fn latest_closed(&self) -> Result<Option<String>> {
        let conn = self.db.lock();
        Ok(conn
            .query_row(
                "SELECT file FROM transcript_files
                 WHERE ended_at IS NOT NULL
                   AND COALESCE(scope, 'shared') = ?1
                 ORDER BY started_at DESC LIMIT 1",
                params![self.scope_name()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// 创建新分段。
    fn create(&self, prev_file: Option<&str>) -> Result<SessionFile> {
        let started_at = Utc::now();
        let prefix = self.scope.as_ref().map(|scope| {
            let mut escaped = scope.replace([':', '/', '\\'], "-");
            escaped.push('-');
            escaped
        });
        let prefix = prefix.unwrap_or_default();
        // `create_new` 是原子占位；冲突时换随机后缀重试，避免 exists()+create 的 TOCTOU。
        for attempt in 0..4 {
            let file_id = new_file_id();
            let suffix = if attempt == 0 {
                (std::process::id() % 1000).to_string()
            } else {
                hex::encode(&file_id.as_bytes()[file_id.len().saturating_sub(4)..])
            };
            let file = format!(
                "{prefix}{}-{suffix}.jsonl",
                started_at.format("%Y%m%dT%H%M%SZ")
            );
            match self.create_named(file, file_id, prev_file, started_at) {
                Ok(info) => return Ok(info),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        anyhow::bail!("cannot allocate a unique transcript file")
    }

    fn create_named(
        &self,
        file: String,
        file_id: String,
        prev_file: Option<&str>,
        started_at: DateTime<Utc>,
    ) -> Result<SessionFile> {
        let path = self.root.join("sessions").join(&file);
        let header = json!({
            "v": LINE_VERSION,
            "kind": "header",
            "file_id": file_id,
            "prev_file": prev_file,
            "created_at": started_at.to_rfc3339(),
            "encryption": match self.encryption {
                EncryptionMode::XChaCha20Poly1305 => "xchacha20poly1305",
                EncryptionMode::None => "none",
            },
        });
        let mut handle = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        handle.write_all(format!("{header}\n").as_bytes())?;
        handle.flush()?;

        let info = SessionFile {
            file,
            file_id,
            prev_file: prev_file.map(str::to_owned),
            started_at: started_at.to_rfc3339(),
            ended_at: None,
            message_count: 0,
            bytes: 0,
        };
        let conn = self.db.lock();
        conn.execute(
            "INSERT INTO transcript_files (file, file_id, prev_file, started_at, scope)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                info.file,
                info.file_id,
                info.prev_file,
                info.started_at,
                self.scope_name()
            ],
        )?;
        Ok(info)
    }

    fn encode_line(&self, message: &AgentMessage, file_id: &str, line_no: usize) -> Result<String> {
        let payload = serde_json::to_vec(message)?;
        let (nonce, ct) = match self.encryption {
            EncryptionMode::XChaCha20Poly1305 => {
                let aad = format!("session:{file_id}:{line_no}");
                let sealed = self.key.seal("session", aad.as_bytes(), &payload)?;
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
        Ok(format!(
            "{}\n",
            json!({"v": LINE_VERSION, "nonce": nonce, "ct": ct})
        ))
    }

    fn decode_line(&self, line: &str, file_id: &str, line_no: usize) -> Result<AgentMessage> {
        let value: Value = serde_json::from_str(line)?;
        let ct = base64::engine::general_purpose::STANDARD
            .decode(value.get("ct").and_then(Value::as_str).unwrap_or_default())?;
        let line_no = value
            .get("line")
            .and_then(Value::as_u64)
            .map(|line| line as usize)
            .unwrap_or(line_no);
        let plain = match self.encryption {
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
                let mut plain = None;
                for candidate in std::iter::once(line_no).chain(1..=4096) {
                    let aad = format!("session:{file_id}:{candidate}");
                    if let Ok(value) = self.key.open("session", aad.as_bytes(), &sealed) {
                        plain = Some(value);
                        break;
                    }
                }
                plain.ok_or_else(|| {
                    anyhow::anyhow!(
                        "cannot decrypt transcript line {line_no} (tried physical and shifted line numbers)"
                    )
                })?
            }
            EncryptionMode::None => ct,
        };
        Ok(serde_json::from_slice(&plain)?)
    }
}

fn row_to_file(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionFile> {
    Ok(SessionFile {
        file: row.get(0)?,
        file_id: row.get(1)?,
        prev_file: row.get(2)?,
        started_at: row.get(3)?,
        ended_at: row.get(4)?,
        message_count: row.get(5)?,
        bytes: row.get(6)?,
    })
}

fn new_file_id() -> String {
    let mut suffix = [0u8; 4];
    let _ = getrandom::fill(&mut suffix);
    format!(
        "{}-{}",
        Utc::now().format("%Y%m%dT%H%M%S%.fZ"),
        hex::encode(suffix)
    )
}

/// 分段文件的宿主路径（测试与 `doctor` 使用）。
pub fn segment_path(root: &Path, file: &str) -> PathBuf {
    root.join("sessions").join(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::message::{AssistantMessage, StopReason, Usage};

    fn store() -> (SessionStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([9u8; 32]));
        let store = SessionStore::open(
            db,
            key,
            dir.path().to_path_buf(),
            0,
            EncryptionMode::XChaCha20Poly1305,
        )
        .unwrap();
        (store, dir)
    }

    fn assistant(text: &str) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![crate::agent::message::AssistantBlock::Text {
                text: text.to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            refusal: None,
        })
    }

    #[test]
    fn append_and_read_roundtrip() {
        let (store, _dir) = store();
        let current = store.current().unwrap();
        assert!(current.file.ends_with(".jsonl"));
        assert_eq!(current.message_count, 0);

        store
            .append(&[AgentMessage::user("hi"), assistant("hello")])
            .unwrap();
        let messages = store.read_segment(&current.file).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], AgentMessage::user("hi"));
        match &messages[1] {
            AgentMessage::Assistant(message) => assert_eq!(message.text(), "hello"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn checkpoints_are_encrypted_and_readable() {
        let (store, _dir) = store();
        let previous = store.current().unwrap();
        store
            .record_checkpoint(&previous.file, "## Facts\nremembered")
            .unwrap();

        // 磁盘上的摘要不是明文。
        let sealed: Vec<u8> = {
            let conn = store.db.lock();
            conn.query_row(
                "SELECT summary_enc FROM compaction_checkpoints ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert!(!sealed.windows(11).any(|window| window == b"remembered"));

        let checkpoints = store.checkpoints(10).unwrap();
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].0, previous.file);
        assert!(checkpoints[0].1.contains("remembered"));
    }

    #[test]
    fn rotate_closes_current_and_chains_prev() {
        let (store, _dir) = store();
        store.append(&[AgentMessage::user("one")]).unwrap();
        let first = store.current().unwrap();
        let second = store.rotate().unwrap();
        assert_eq!(second.prev_file.as_deref(), Some(first.file.as_str()));

        let first_info = store.info(&first.file).unwrap().unwrap();
        assert!(first_info.ended_at.is_some());
        assert_eq!(store.current().unwrap().file, second.file);

        store.append(&[AgentMessage::user("two")]).unwrap();
        assert_eq!(store.read_segment(&second.file).unwrap().len(), 1);
    }

    #[test]
    fn segments_are_encrypted_at_rest() {
        let (store, dir) = store();
        store
            .append(&[AgentMessage::user("secret-marker")])
            .unwrap();
        let current = store.current().unwrap();
        let text = std::fs::read_to_string(segment_path(dir.path(), &current.file)).unwrap();
        assert!(text.contains("\"kind\":\"header\""));
        assert!(!text.contains("secret-marker"));
    }

    #[test]
    fn retention_never_removes_current_segment() {
        let (store, _dir) = store();
        store.append(&[AgentMessage::user("one")]).unwrap();
        let _ = store.rotate().unwrap();

        let removed = store.enforce_retention(0).unwrap();
        // 没有大小压力也没有年龄压力：什么都不删。
        assert_eq!(removed, 0);

        // 把旧分段标成 100 天前，才会被年龄清理。
        {
            let conn = store.db.lock();
            conn.execute(
                "UPDATE transcript_files SET started_at = ?1, ended_at = ?1 WHERE ended_at IS NOT NULL",
                params![(Utc::now() - chrono::Duration::days(100)).to_rfc3339()],
            )
            .unwrap();
        }
        let removed = store.enforce_retention(30).unwrap();
        assert!(removed > 0);
        // 当前分段仍在。
        let current = store.current().unwrap();
        assert!(store.info(&current.file).unwrap().is_some());
    }

    #[test]
    fn list_is_newest_first() {
        let (store, _dir) = store();
        store.append(&[AgentMessage::user("one")]).unwrap();
        let second = store.rotate().unwrap();
        let list = store.list().unwrap();
        assert_eq!(list[0].file, second.file);
    }

    #[test]
    fn per_chat_scopes_have_separate_chains() {
        let (store, _dir) = store();
        let group = store.for_scope("group-1");
        let private = store.for_scope("private-2");
        group
            .append(&[AgentMessage::user("group message")])
            .unwrap();
        private
            .append(&[AgentMessage::user("private message")])
            .unwrap();

        let group_current = group.current().unwrap();
        let private_current = private.current().unwrap();
        assert_ne!(group_current.file, private_current.file);
        assert!(group_current.file.starts_with("group-1-"));
        assert!(private_current.file.starts_with("private-2-"));
        assert_eq!(group.list().unwrap().len(), 1);
        assert_eq!(private.list().unwrap().len(), 1);
        assert_eq!(store.list().unwrap().len(), 2);
        let scopes = store.scopes().unwrap();
        assert!(scopes.contains(&"group-1".to_owned()));
        assert!(scopes.contains(&"private-2".to_owned()));

        let group_messages = group.read_segment(&group_current.file).unwrap();
        assert_eq!(group_messages.len(), 1);
        assert!(
            matches!(&group_messages[0], AgentMessage::User { text, .. } if text.contains("group message"))
        );
    }
}
