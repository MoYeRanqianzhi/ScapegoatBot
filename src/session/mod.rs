//! Transcript 分段。
//!
//! 设计依据：设计 §8.1。
//!
//! - 整个 QQ 号只有一份逻辑 transcript（`shared`）；文件是分段，不是会话实体。
//! - 文件：`sessions/<YYYYMMDDTHHMMSSZ>-<seq>.jsonl`，名称字典序即时间序。
//! - 首行明文文件头；之后每行一条 `AgentMessage`，AAD 绑定 `session:<file_id>:<line_no>`。
//! - `transcript_files` 只是可重建索引；`/sessions` 视图由内容动态渲染。
//! - 每次压缩成功后轮转新分段（由 runner 调用 `SessionStore::rotate`）。

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

use crate::config::EncryptionMode;
use crate::storage::Database;
use crate::storage::crypto::MasterKey;
use crate::storage::envelope;
use crate::transcript::AgentMessage;

/// 行格式版本。
const LINE_VERSION: u32 = 1;
/// 单行恢复扫描最多尝试的候选行号数。
///
/// 正常写入时信封行号就是 AAD 行号，不需要扫描；扫描只用于缺字段/位置漂移。
/// 候选先取物理位置 ±64，再退回全范围。若整段都解不开（错 key、file_id 不匹配），
/// 每行最多 128 次 AEAD 尝试，避免旧实现「逐行扫全文件」的 O(n²) 卡顿。
const SCAN_PER_LINE: usize = 128;
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
    /// 按年龄清理已关闭的分段；0 = 只按配额。
    retention_days: u32,
    /// 同一分段的 append/read 串行化，避免行号竞争。
    append_lock: std::sync::Arc<std::sync::Mutex<()>>,
    /// 当前分段解析/创建串行化，避免两个首次写入建出两个 open segment。
    current_lock: std::sync::Arc<std::sync::Mutex<()>>,
}

impl SessionStore {
    /// 打开并确保 `sessions/` 存在。
    pub(crate) fn open(
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
            retention_days: 0,
            append_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            current_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        })
    }

    /// 设置按年龄清理的天数（0 = 关闭）。
    pub(crate) fn with_retention_days(mut self, days: u32) -> Self {
        self.retention_days = days;
        self
    }

    /// 当前（未关闭）分段；不存在时创建一个。
    pub(crate) fn current(&self) -> Result<SessionFile> {
        let _guard = self
            .current_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.current_unlocked()
    }

    fn current_unlocked(&self) -> Result<SessionFile> {
        let existing: Option<String> = {
            let conn = self.db.lock();
            conn.query_row(
                "SELECT file FROM transcript_files
                 WHERE ended_at IS NULL AND COALESCE(scope, 'shared') = 'shared'
                 ORDER BY started_at DESC LIMIT 1",
                [],
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
                self.create(prev.as_deref(), &[], None)
            }
        }
    }

    /// 当前分段的虚拟路径，供 system prompt 的 `current_session` 使用。
    pub(crate) fn current_path(&self) -> Result<String> {
        Ok(format!("/sessions/{}", self.current()?.file))
    }

    /// 追加消息到当前分段。
    pub(crate) fn append(&self, messages: &[AgentMessage]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let _guard = self
            .append_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = self.current_unlocked()?;
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
    pub(crate) fn read_segment(&self, file: &str) -> Result<Vec<AgentMessage>> {
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

        // AAD 行号按写入时的 message_count 生成（append 用 count+1）；删除行后物理
        // 行数会小于最大行号。扫描上界取两者较大值，避免删掉 ≥2 行后仍漏掉后半段。
        let recorded_count: usize = self
            .db
            .lock()
            .query_row(
                "SELECT message_count FROM transcript_files WHERE file = ?1",
                [file_name],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            .max(0) as usize;
        let mut messages = Vec::with_capacity(lines.len().saturating_sub(1));
        let max_line = lines.len().max(recorded_count);
        for (line_no, line) in lines.iter().enumerate().skip(1) {
            match self.decode_line(line, &file_id, line_no, max_line) {
                Ok(message) => messages.push(message),
                Err(error) => {
                    // 单个坏行不等于整个分段不可用；记录并跳过。
                    warn!(%error, file = %file_name, line_no, "skipping unreadable transcript line");
                }
            }
        }
        Ok(messages)
    }

    /// 列出全部分段（从新到旧）。
    pub(crate) fn list(&self) -> Result<Vec<SessionFile>> {
        let conn = self.db.lock();
        let mut statement = conn.prepare(
            "SELECT file, file_id, prev_file, started_at, ended_at, message_count, bytes
             FROM transcript_files ORDER BY started_at DESC",
        )?;
        let mut rows = statement
            .query_map([], row_to_file)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.shrink_to_fit();
        Ok(rows)
    }

    /// 轮转：关闭当前分段并创建一个空的新分段（清空上下文时调用）。
    pub(crate) fn rotate(&self) -> Result<SessionFile> {
        self.rotate_with(&[])
    }

    /// 轮转，并把 `first` 作为新分段的开头（压缩时是摘要与保留的消息）。
    ///
    /// 要么全部发生，要么什么都没变：新分段的文件连同 `first` 先写好，然后在一个数据库事务
    /// 里关闭旧分段、登记新分段；任何一步失败都删掉新文件，当前分段还是原来那个。分两步做
    /// （先轮转、再追加）的话，第二步失败会留下一个空的当前分段：进程重启时上下文从它恢复，
    /// 之前的对话连同摘要一起没了。
    pub(crate) fn rotate_with(&self, first: &[AgentMessage]) -> Result<SessionFile> {
        let _guard = self
            .append_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _current_guard = self
            .current_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = self.current_unlocked()?;
        let next = self.create(Some(&current.file), first, Some(&current.file))?;
        // 轮转后顺手清理：超过保留期或总大小超过 `max_session_bytes` 的已关闭分段。
        // 两个条件独立——只设了大小上限（默认 2 GiB、保留期 0）时同样要清理。
        if self.limits_retention()
            && let Err(error) = self.enforce_retention(self.retention_days)
        {
            warn!(%error, "cannot enforce transcript retention");
        }
        Ok(next)
    }

    /// 是否设了保留期或总大小上限（任一非零就需要清理）。
    pub(crate) fn limits_retention(&self) -> bool {
        self.retention_days > 0 || self.max_bytes != 0
    }

    /// 记录一次压缩检查点（加密存储）。
    ///
    /// 设计 §8.1：`compaction_checkpoints` 用于崩溃恢复与审计；
    /// 分段首行的摘要仍是权威，这里的行只是可查询的缓存。
    /// 只保留最近 [`CHECKPOINT_KEEP`] 条，避免无限增长。
    pub(crate) fn record_checkpoint(&self, from_file: &str, summary: &str) -> Result<()> {
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
    #[cfg(test)]
    pub(crate) fn checkpoints(&self, limit: usize) -> Result<Vec<(String, String, String)>> {
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

    /// 用量：字节数与消息条数。
    pub(crate) fn usage(&self) -> Result<(u64, u64)> {
        let conn = self.db.lock();
        let (bytes, count) = conn.query_row(
            "SELECT COALESCE(SUM(bytes), 0), COALESCE(SUM(message_count), 0)
             FROM transcript_files WHERE pruned_at IS NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((bytes, count))
    }

    /// 按年龄与配额删除最旧的**已关闭**分段，绝不删除当前分段。
    pub(crate) fn enforce_retention(&self, retention_days: u32) -> Result<u64> {
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
                       AND COALESCE(scope, 'shared') = 'shared'
                     ORDER BY started_at ASC LIMIT 1",
                    [],
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
    pub(crate) fn info(&self, file: &str) -> Result<Option<SessionFile>> {
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
                   AND COALESCE(scope, 'shared') = 'shared'
                 ORDER BY started_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// 创建新分段，内容是 `first`；`close` 是要在同一个事务里关闭的分段（轮转时的旧分段）。
    fn create(
        &self,
        prev_file: Option<&str>,
        first: &[AgentMessage],
        close: Option<&str>,
    ) -> Result<SessionFile> {
        let started_at = Utc::now();
        let prefix = "";
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
            match self.create_named(file, file_id, prev_file, started_at, first, close) {
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

    /// [`create`](Self::create) 的一次尝试：用给定的文件名。文件已存在时返回
    /// `AlreadyExists`（调用方换名字重试），不碰那个文件。
    fn create_named(
        &self,
        file: String,
        file_id: String,
        prev_file: Option<&str>,
        started_at: DateTime<Utc>,
        first: &[AgentMessage],
        close: Option<&str>,
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
        // `create_new` 失败（文件已存在）时直接返回：那不是这次调用建的文件，不能删。
        let mut handle = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("cannot create {}", path.display()))?;

        // 从这里起文件是这次调用建的：后面任何一步失败都删掉它，不留下没登记的分段。
        let registered = (|| -> Result<SessionFile> {
            handle.write_all(format!("{header}\n").as_bytes())?;
            // 与 `append` 相同的记法：行号从 1 起（第 0 行是文件头），`bytes` 不含文件头。
            let mut bytes = 0u64;
            for (index, message) in first.iter().enumerate() {
                let line = self.encode_line(message, &file_id, index + 1)?;
                handle.write_all(line.as_bytes())?;
                bytes += line.len() as u64;
            }
            handle.flush()?;

            let info = SessionFile {
                file: file.clone(),
                file_id: file_id.clone(),
                prev_file: prev_file.map(str::to_owned),
                started_at: started_at.to_rfc3339(),
                ended_at: None,
                message_count: first.len() as u64,
                bytes,
            };
            let mut conn = self.db.lock();
            let tx = conn.transaction()?;
            if let Some(close) = close {
                tx.execute(
                    "UPDATE transcript_files SET ended_at = ?1 WHERE file = ?2",
                    params![Utc::now().to_rfc3339(), close],
                )?;
            }
            tx.execute(
                "INSERT INTO transcript_files
                    (file, file_id, prev_file, started_at, scope, message_count, bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    info.file,
                    info.file_id,
                    info.prev_file,
                    info.started_at,
                    "shared",
                    info.message_count,
                    info.bytes
                ],
            )?;
            tx.commit()?;
            Ok(info)
        })();
        if registered.is_err() {
            drop(handle);
            if let Err(error) = std::fs::remove_file(&path) {
                warn!(%error, file = %path.display(), "cannot remove an unregistered transcript segment");
            }
        }
        registered
    }

    fn encode_line(&self, message: &AgentMessage, file_id: &str, line_no: usize) -> Result<String> {
        let payload = serde_json::to_vec(message)?;
        let (nonce, ct) = envelope::seal_as(
            self.encryption,
            &self.key,
            "session",
            &format!("session:{file_id}:{line_no}"),
            &payload,
        )?;
        Ok(format!(
            "{}\n",
            json!({"v": LINE_VERSION, "line": line_no, "nonce": nonce, "ct": ct})
        ))
    }

    /// 解密一行；`max_line` 是所在分段的总行数，用于限定恢复扫描范围。
    ///
    /// AAD 是 `session:<file_id>:<line_no>`，行号在写入时决定；信封里的 `line`
    /// 只是提示。正常情况下信封行号即 AAD 行号，先按它解；缺失或漂移时才
    /// 在分段行数内枚举 AAD 行号，把还能解开的行恢复出来。
    fn decode_line(
        &self,
        line: &str,
        file_id: &str,
        line_no: usize,
        max_line: usize,
    ) -> Result<AgentMessage> {
        let value: Value = serde_json::from_str(line)?;
        let ct_b64 = value.get("ct").and_then(Value::as_str).unwrap_or_default();
        let envelope_line = value
            .get("line")
            .and_then(Value::as_u64)
            .map(|line| line as usize);
        let plain = match self.encryption {
            EncryptionMode::XChaCha20Poly1305 => {
                let nonce_b64 = value
                    .get("nonce")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let sealed = envelope::join(nonce_b64, ct_b64)?;
                // 信封行号优先（正常写入时它就是 AAD 行号）；失败后再试物理行号，
                // 最后才扫描。
                //
                // 扫描存在的原因：AAD 绑定写入时的行号，而分段被重写、拼接，
                // 或前后插入/删除行之后，物理位置会漂移，信封也可能缺字段，
                // 此时只能按「能否解开」反推行号。
                //
                // 上界用分段总行数而不是固定值：扫描范围随分段增长，长分段靠后的行
                // 不会在固定值处被截断；候选上限也与读取方实际掌握的文件长度一致。
                //
                // 已知取舍：扫描会接受位置被移动但可解的行，所以它恢复的是可读性，
                // 不是位置完整性；位置完整性仍待签名或链式摘要方案。
                let first = envelope_line.unwrap_or(line_no);
                // 位置漂移通常很小（删/插几行）。先试信封与物理行号，再试物理位置
                // 附近的候选，最后才退回全范围；长分段末尾的行不必从 1 扫起。
                let mut candidates = Vec::new();
                candidates.push(first);
                if line_no != first {
                    candidates.push(line_no);
                }
                for delta in 1..=64usize {
                    if line_no + delta <= max_line {
                        candidates.push(line_no + delta);
                    }
                    if line_no > delta {
                        candidates.push(line_no - delta);
                    }
                }
                candidates.extend(
                    (1..=max_line).filter(|candidate| *candidate != first && *candidate != line_no),
                );
                let mut plain = None;
                for (scanned, candidate) in candidates.into_iter().enumerate() {
                    if scanned >= SCAN_PER_LINE {
                        break;
                    }
                    let aad = format!("session:{file_id}:{candidate}");
                    if let Ok(value) = self.key.open("session", aad.as_bytes(), &sealed) {
                        plain = Some(value);
                        break;
                    }
                }
                plain.ok_or_else(|| {
                    anyhow::anyhow!(
                        "cannot decrypt transcript line at physical position {line_no} \
                         (envelope line: {envelope_line:?}; scanned sealed line numbers \
                         1..={max_line})"
                    )
                })?
            }
            EncryptionMode::None => base64::engine::general_purpose::STANDARD.decode(ct_b64)?,
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
    use crate::transcript::{AssistantMessage, StopReason, Usage};

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
            content: vec![crate::transcript::AssistantBlock::Text {
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

    /// `sessions/` 下的文件名，排好序。
    fn segment_files(dir: &tempfile::TempDir) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.path().join("sessions"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// 带着开头消息轮转：新分段一建好就有这些消息，索引里的条数与字节数和 `append` 写进去
    /// 的记法一致（之后再追加，行号接着往下排，整段都读得出来）。
    #[test]
    fn rotate_with_starts_the_new_segment_with_its_first_messages() {
        let (store, dir) = store();
        store.append(&[AgentMessage::user("one")]).unwrap();
        let first = store.current().unwrap();

        let opening = [AgentMessage::user("summary"), assistant("kept")];
        let second = store.rotate_with(&opening).unwrap();
        assert_eq!(second.prev_file.as_deref(), Some(first.file.as_str()));
        assert_eq!(second.message_count, 2);
        assert!(store.info(&first.file).unwrap().unwrap().ended_at.is_some());

        // 返回值与索引里的行一致；`bytes` 是文件头之后的全部字节。
        let indexed = store.current().unwrap();
        assert_eq!(indexed.file, second.file);
        assert_eq!(indexed.message_count, 2);
        assert_eq!(indexed.bytes, second.bytes);
        let raw = std::fs::read(dir.path().join("sessions").join(&second.file)).unwrap();
        let header_len = raw.iter().position(|byte| *byte == b'\n').unwrap() + 1;
        assert_eq!(second.bytes, (raw.len() - header_len) as u64);

        let messages = store.read_segment(&second.file).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], AgentMessage::user("summary"));

        store.append(&[AgentMessage::user("next")]).unwrap();
        let messages = store.read_segment(&second.file).unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2], AgentMessage::user("next"));
        assert_eq!(store.current().unwrap().message_count, 3);
    }

    /// 轮转在登记新分段时失败：旧分段没有被关闭、仍是当前分段，磁盘上不留下没登记的文件，
    /// 之后的追加照常落在旧分段里。以前关闭旧分段与登记新分段是两条独立的语句。
    #[test]
    fn a_failed_rotation_changes_nothing() {
        let (store, dir) = store();
        store.append(&[AgentMessage::user("one")]).unwrap();
        let first = store.current().unwrap();
        let files_before = segment_files(&dir);

        // 让登记新分段的 INSERT 失败：它在关闭旧分段的 UPDATE 之后。
        store
            .db
            .lock()
            .execute_batch(
                "CREATE TRIGGER fail_insert BEFORE INSERT ON transcript_files
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        let error = store
            .rotate_with(&[AgentMessage::user("summary")])
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("injected failure"),
            "{error:#}"
        );

        let current = store.current().unwrap();
        assert_eq!(current.file, first.file);
        assert!(current.ended_at.is_none());
        assert_eq!(segment_files(&dir), files_before);
        store.append(&[AgentMessage::user("two")]).unwrap();
        assert_eq!(store.read_segment(&first.file).unwrap().len(), 2);

        // 故障排除后同一次轮转成功。
        store
            .db
            .lock()
            .execute_batch("DROP TRIGGER fail_insert")
            .unwrap();
        let second = store.rotate_with(&[AgentMessage::user("summary")]).unwrap();
        assert_eq!(store.current().unwrap().file, second.file);
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
    fn size_cap_applies_without_a_retention_period() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([3u8; 32]));
        // 保留期 0（默认），只有 1 字节的大小上限：轮转时旧分段必须被清理。
        let store =
            SessionStore::open(db, key, dir.path().to_path_buf(), 1, EncryptionMode::None).unwrap();
        assert!(store.limits_retention());
        store.append(&[AgentMessage::user("old segment")]).unwrap();
        let first = store.current().unwrap();
        store.rotate().unwrap();
        let pruned: Option<String> = {
            let conn = store.db.lock();
            conn.query_row(
                "SELECT pruned_at FROM transcript_files WHERE file = ?1",
                params![first.file],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert!(pruned.is_some(), "the size cap was not enforced on rotate");
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
    fn recovery_scan_covers_segments_longer_than_4096_lines() {
        let (store, dir) = store();
        let current = store.current().unwrap();
        let messages: Vec<AgentMessage> = (1..=4100)
            .map(|index| AgentMessage::user(format!("message-{index}")))
            .collect();
        store.append(&messages).unwrap();

        let path = segment_path(dir.path(), &current.file);
        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        // 删掉首条消息：其后所有物理行号前移一位，而 AAD 仍是写入时的行号。
        lines.remove(1);
        // 目标行原本物理行 4100、AAD 4100；现在物理行 4099 且信封没有
        // `line`。固定上限 1..=4096 够不到 4100，只有按文件行数扫描才能恢复。
        let target = lines.last_mut().unwrap();
        let mut value: Value = serde_json::from_str(target).unwrap();
        value.as_object_mut().unwrap().remove("line");
        *target = value.to_string();
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

        let read = store.read_segment(&current.file).unwrap();
        assert_eq!(read.len(), 4099);
        assert_eq!(read.first(), Some(&AgentMessage::user("message-2")));
        assert!(
            matches!(read.last(), Some(AgentMessage::User { text, .. }) if text == "message-4100"),
            "recovery must reach the shifted line beyond 4096"
        );
    }

    #[test]
    fn concurrent_append_read_rotate_keeps_segments_consistent() {
        let (store, dir) = store();
        let store = Arc::new(store);
        let mut handles = Vec::new();

        for worker in 0..2 {
            let store = Arc::clone(&store);
            handles.push(std::thread::spawn(move || {
                for round in 0..40 {
                    store
                        .append(&[AgentMessage::user(format!("w{worker}-r{round}"))])
                        .unwrap();
                }
            }));
        }

        {
            let store = Arc::clone(&store);
            handles.push(std::thread::spawn(move || {
                for _ in 0..20 {
                    for file in store.list().unwrap() {
                        let messages = store.read_segment(&file.file).unwrap();
                        for message in messages {
                            match message {
                                AgentMessage::User { text, .. } => {
                                    assert!(text.starts_with('w'), "unexpected {text}");
                                }
                                other => panic!("unexpected {other:?}"),
                            }
                        }
                    }
                }
            }));
        }

        {
            let store = Arc::clone(&store);
            handles.push(std::thread::spawn(move || {
                for _ in 0..8 {
                    store.rotate().unwrap();
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let mut texts = Vec::new();
        for file in store.list().unwrap() {
            let info = store.info(&file.file).unwrap().unwrap();
            let raw_lines = std::fs::read_to_string(segment_path(dir.path(), &file.file))
                .unwrap()
                .lines()
                .count();
            // 每条消息恰好一行：既没有半截行，也没有重复写入。
            assert_eq!(raw_lines as u64, info.message_count + 1);
            for message in store.read_segment(&file.file).unwrap() {
                match message {
                    AgentMessage::User { text, .. } => texts.push(text),
                    other => panic!("unexpected {other:?}"),
                }
            }
        }
        assert_eq!(texts.len(), 80);
        let unique: std::collections::HashSet<_> = texts.iter().collect();
        assert_eq!(unique.len(), 80);
    }

    #[test]
    fn wrong_master_key_reports_error_and_read_segment_skips_lines() {
        let (store, dir) = store();
        store.append(&[AgentMessage::user("secret")]).unwrap();
        let current = store.current().unwrap();
        let wrong = SessionStore::open(
            Arc::clone(&store.db),
            Arc::new(MasterKey::from_bytes([1u8; 32])),
            dir.path().to_path_buf(),
            0,
            EncryptionMode::XChaCha20Poly1305,
        )
        .unwrap();

        let file = std::fs::read_to_string(segment_path(dir.path(), &current.file)).unwrap();
        let line = file.lines().nth(1).unwrap();
        let error = wrong.decode_line(line, &current.file_id, 1, 2).unwrap_err();
        assert!(
            error.to_string().contains("1..=2"),
            "error must name the scanned range: {error}"
        );

        // 坏行只记日志并跳过，整段读取仍返回成功而不是 panic。
        assert!(wrong.read_segment(&current.file).unwrap().is_empty());
    }
}
