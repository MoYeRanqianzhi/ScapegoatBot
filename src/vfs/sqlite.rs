//! VFS 的 SQLite 后端。
//!
//! 设计依据：设计 §6.4 / §6.5 / §6.9。
//!
//! 存储约定：
//! - 文本内容加密后存在 `vfs_nodes.content_enc`，AAD 绑定 `vfs:<node_id>:<version>`；
//! - 二进制按 1 MiB 分块，密文写到 `blobs/<node_id>/<idx>`，AAD 绑定 `blob:<node_id>:<idx>`；
//! - 索引（路径、大小、hash、时间）明文，供 `LS` / `Glob` / `Grep` 与 SQL 查询使用；
//! - 删除先进回收站；回收站内的 `RM` 是永久删除。
//!
//! 并发：全进程单连接，写路径由 `Database::lock` 串行化；这比按节点加锁更容易验证，
//! 也满足“同一节点串行、不同节点不冲突”的要求（串行是更强的不冲突）。

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::config::StorageConfig;
use crate::storage::Database;
use crate::storage::crypto::MasterKey;

use super::path::{VfsPath, is_dynamic};
use super::quota::{Limits, Quota};
use super::{FileContent, NodeKind, NodeMeta, RemoveOutcome, Usage, Vfs, VfsError, guess_mime};

/// 二进制分块大小：1 MiB。
const BLOB_CHUNK_BYTES: usize = 1024 * 1024;
/// 文本加密的域标签。
const TEXT_PURPOSE: &str = "vfs-text";
/// 二进制加密的域标签。
const BLOB_PURPOSE: &str = "vfs-blob";
/// 回收站保留期（天）。
const TRASH_RETENTION_DAYS: i64 = 30;

/// VFS 存储。
pub struct VfsStore {
    db: Arc<Database>,
    key: Arc<MasterKey>,
    /// `[storage].path` 解析后的宿主目录；blobs 放在其下。
    storage: PathBuf,
    quota: Quota,
}

impl VfsStore {
    /// 打开 VFS：准备 blob 目录并写入种子文件。
    pub fn open(
        db: Arc<Database>,
        key: Arc<MasterKey>,
        storage: PathBuf,
        limits: Limits,
    ) -> Result<Self, VfsError> {
        std::fs::create_dir_all(storage.join("blobs"))?;
        let store = Self {
            db,
            key,
            storage,
            quota: Quota::new(limits),
        };
        store.ensure_seeds()?;
        Ok(store)
    }

    /// 从 `[storage]` 直接构造。
    pub fn from_config(
        db: Arc<Database>,
        key: Arc<MasterKey>,
        storage: PathBuf,
        config: &StorageConfig,
    ) -> Result<Self, VfsError> {
        Self::open(db, key, storage, Limits::from_config(config))
    }

    /// 种子布局（设计 §6.7）：空身份文件、记忆目录、`/agents`、`/trash`、`/tmp`。
    fn ensure_seeds(&self) -> Result<(), VfsError> {
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;
        let now = now();

        for dir in [
            "/memory/topics",
            "/memory/notes",
            "/agents",
            "/trash",
            "/tmp",
        ] {
            let path = VfsPath::parse(dir).expect("seed paths are valid");
            self.ensure_dir(&tx, &path, &now)?;
        }
        for file in ["/AGENTS.md", "/MEMORY.md", "/USER.md"] {
            let path = VfsPath::parse(file).expect("seed paths are valid");
            if self.lookup(&tx, &path)?.is_none() {
                let parent_id = self.ensure_parent(&tx, &path, &now)?;
                let name = path.file_name().expect("seed file has a name");
                tx.execute(
                    "INSERT INTO vfs_nodes
                        (parent_id, name, kind, content_enc, size, mime, mode, version, created_at, updated_at, hash)
                     VALUES (?1, ?2, 'text', NULL, 0, 'text/markdown', 420, 1, ?3, ?3, ?4)",
                    params![parent_id, name, now, hash_bytes(b"")],
                )?;
                let id = tx.last_insert_rowid();
                let sealed = self.seal_text(id, 1, "")?;
                tx.execute(
                    "UPDATE vfs_nodes SET content_enc = ?1 WHERE id = ?2",
                    params![sealed, id],
                )?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// 回收站根路径。
    fn trash_path(&self) -> VfsPath {
        VfsPath::parse("/trash").expect("trash path is valid")
    }

    // ---------------------------------------------------------------------
    // 行查询
    // ---------------------------------------------------------------------

    /// 按路径查节点；`/trash` 之下允许命中已回收节点。
    fn lookup(&self, conn: &Connection, path: &VfsPath) -> Result<Option<NodeRow>, VfsError> {
        if path.is_root() {
            return Ok(conn
                .query_row(
                    "SELECT id, parent_id, name, kind, content_enc, size, mime, version, created_at,
                            updated_at, hash, trashed_at, trash_origin
                     FROM vfs_nodes WHERE id = 1",
                    [],
                    NodeRow::from_row,
                )
                .optional()?);
        }

        // 回收站里的节点 trashed_at 非空；只有 /trash 子树允许按路径访问。
        let allow_trashed = path.starts_with(&self.trash_path());
        let mut parent_id = 1i64;
        let segments = path.segments();

        for (index, segment) in segments.iter().enumerate() {
            let found: Option<NodeRow> = conn
                .query_row(
                    "SELECT id, parent_id, name, kind, content_enc, size, mime, version, created_at,
                            updated_at, hash, trashed_at, trash_origin
                     FROM vfs_nodes
                     WHERE parent_id = ?1 AND name = ?2 AND (trashed_at IS NULL OR ?3 = 1)",
                    params![parent_id, segment, allow_trashed as i32],
                    NodeRow::from_row,
                )
                .optional()?;

            let Some(node) = found else {
                return Ok(None);
            };
            // 最后一段就是目标；中间段必须是目录，否则路径不存在。
            if index + 1 == segments.len() {
                return Ok(Some(node));
            }
            if node.kind != NodeKind::Dir {
                return Ok(None);
            }
            parent_id = node.id;
        }
        Ok(None)
    }

    /// 由节点 id 还原虚拟路径。
    fn path_of(&self, conn: &Connection, mut id: i64) -> Result<VfsPath, VfsError> {
        let mut segments = Vec::new();
        while id != 1 {
            let (parent_id, name): (Option<i64>, String) = conn.query_row(
                "SELECT parent_id, name FROM vfs_nodes WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            segments.push(name);
            match parent_id {
                Some(parent) => id = parent,
                None => break,
            }
        }
        segments.reverse();
        if segments.is_empty() {
            return Ok(VfsPath::root());
        }
        let raw = format!("/{}", segments.join("/"));
        Ok(VfsPath::parse(&raw).expect("stored segments form a valid path"))
    }

    /// 建立从根到 `path` 的目录链，返回目标目录 id。
    fn ensure_dir(&self, conn: &Connection, path: &VfsPath, now: &str) -> Result<i64, VfsError> {
        let mut current = VfsPath::root();
        let mut parent_id = 1i64;
        for segment in path.segments() {
            current = current.join(segment)?;
            match self.lookup(conn, &current)? {
                Some(row) => {
                    if row.kind != NodeKind::Dir {
                        return Err(VfsError::NotADirectory(current.to_string()));
                    }
                    parent_id = row.id;
                }
                None => {
                    conn.execute(
                        "INSERT INTO vfs_nodes
                            (parent_id, name, kind, size, mode, version, created_at, updated_at)
                         VALUES (?1, ?2, 'dir', 0, 493, 1, ?3, ?3)",
                        params![parent_id, segment, now],
                    )?;
                    parent_id = conn.last_insert_rowid();
                }
            }
        }
        Ok(parent_id)
    }

    /// 确保父目录存在（缺失则创建）。
    fn ensure_parent(&self, conn: &Connection, path: &VfsPath, now: &str) -> Result<i64, VfsError> {
        let parent = path
            .parent()
            .ok_or_else(|| VfsError::NotFound(path.to_string()))?;
        self.ensure_dir(conn, &parent, now)
    }

    /// 构建元信息。
    fn meta(&self, path: &VfsPath, row: &NodeRow) -> NodeMeta {
        NodeMeta {
            path: path.clone(),
            kind: row.kind,
            size: row.size,
            mime: row.mime.clone(),
            version: row.version,
            created_at: row.created_at.clone(),
            updated_at: row.updated_at.clone(),
            hash: row.hash.clone(),
        }
    }

    // ---------------------------------------------------------------------
    // 加密
    // ---------------------------------------------------------------------

    fn seal_text(&self, id: i64, version: u64, text: &str) -> Result<Vec<u8>, VfsError> {
        let aad = format!("vfs:{id}:{version}");
        self.key
            .seal(TEXT_PURPOSE, aad.as_bytes(), text.as_bytes())
            .map_err(VfsError::from)
    }

    fn open_text(&self, row: &NodeRow) -> Result<String, VfsError> {
        let sealed = row
            .content_enc
            .as_ref()
            .ok_or_else(|| VfsError::Storage(format!("node {} has no content", row.id)))?;
        let aad = format!("vfs:{}:{}", row.id, row.version);
        let plain = self
            .key
            .open(TEXT_PURPOSE, aad.as_bytes(), sealed)
            .map_err(VfsError::from)?;
        String::from_utf8(plain)
            .map_err(|_| VfsError::Storage(format!("node {} is not valid UTF-8", row.id)))
    }

    fn seal_blob(&self, id: i64, idx: usize, plain: &[u8]) -> Result<Vec<u8>, VfsError> {
        let aad = format!("blob:{id}:{idx}");
        self.key
            .seal(BLOB_PURPOSE, aad.as_bytes(), plain)
            .map_err(VfsError::from)
    }

    /// 读取某个二进制分块（供 Phase 4 的 `Download` / 发送文件流式使用）。
    #[allow(dead_code)]
    pub fn read_blob_chunk(&self, path: &VfsPath, idx: u64) -> Result<Vec<u8>, VfsError> {
        let conn = self.db.lock();
        let row = self
            .lookup(&conn, path)?
            .ok_or_else(|| VfsError::NotFound(path.to_string()))?;
        if !row.kind.is_file() {
            return Err(VfsError::IsADirectory(path.to_string()));
        }
        let aad = format!("blob:{}:{idx}", row.id);
        let sealed = std::fs::read(self.blob_path(row.id, idx as usize))?;
        self.key
            .open(BLOB_PURPOSE, aad.as_bytes(), &sealed)
            .map_err(VfsError::from)
    }

    fn blob_path(&self, id: i64, idx: usize) -> PathBuf {
        self.storage
            .join("blobs")
            .join(id.to_string())
            .join(idx.to_string())
    }

    // ---------------------------------------------------------------------
    // Read-before-Edit
    // ---------------------------------------------------------------------

    /// 记录一次读取。
    fn record_read(
        &self,
        conn: &Connection,
        id: i64,
        version: u64,
        hash: Option<&str>,
        is_partial: bool,
    ) -> Result<(), VfsError> {
        conn.execute(
            "INSERT INTO vfs_reads (node_id, version, content_hash, is_partial, read_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(node_id) DO UPDATE SET
                version = excluded.version,
                content_hash = excluded.content_hash,
                is_partial = excluded.is_partial,
                read_at = excluded.read_at",
            params![id, version, hash, is_partial as i32, now()],
        )?;
        Ok(())
    }

    /// 写前检查（设计 §6.5 的 1–4 步）。
    fn check_read_before_edit(&self, conn: &Connection, row: &NodeRow) -> Result<(), VfsError> {
        let record: Option<(u64, Option<String>, bool)> = conn
            .query_row(
                "SELECT version, content_hash, is_partial FROM vfs_reads WHERE node_id = ?1",
                params![row.id],
                |result| {
                    Ok((
                        result.get(0)?,
                        result.get(1)?,
                        result.get::<_, i32>(2)? != 0,
                    ))
                },
            )
            .optional()?;

        match record {
            None => Err(VfsError::NotReadYet),
            Some((_, _, true)) => Err(VfsError::PartialRead),
            Some((version, hash, false)) => {
                if version != row.version || hash.as_deref() != row.hash.as_deref() {
                    Err(VfsError::ModifiedSinceRead)
                } else {
                    Ok(())
                }
            }
        }
    }

    // ---------------------------------------------------------------------
    // 写入
    // ---------------------------------------------------------------------

    /// 活跃用量。
    fn active_usage(&self, conn: &Connection) -> Result<(u64, u64), VfsError> {
        let (bytes, nodes): (u64, u64) = conn.query_row(
            "SELECT COALESCE(SUM(CASE WHEN kind != 'dir' THEN size ELSE 0 END), 0), COUNT(*)
             FROM vfs_nodes WHERE trashed_at IS NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((bytes, nodes))
    }

    /// 文本写入的核心逻辑，要求调用方已持有连接锁与事务。
    fn write_text_tx(
        &self,
        conn: &Connection,
        path: &VfsPath,
        content: &str,
        origin_chat: Option<&str>,
    ) -> Result<NodeMeta, VfsError> {
        let len = content.len() as u64;
        self.quota.check_text(len)?;
        self.quota.check_disk(&self.storage)?;

        let existing = self.lookup(conn, path)?;
        let parent_id = self.ensure_parent(conn, path, &now())?;
        let now = now();
        let name = path.file_name().expect("non-root path has a name");
        let mime = guess_mime(name).unwrap_or("text/plain");
        let hash = hash_bytes(content.as_bytes());

        let (id, version) = match existing {
            None => {
                let (bytes, nodes) = self.active_usage(conn)?;
                self.quota.check_growth(bytes, nodes, len, 1)?;
                conn.execute(
                    "INSERT INTO vfs_nodes
                        (parent_id, name, kind, content_enc, size, mime, mode, version,
                         origin_chat, created_at, updated_at, hash)
                     VALUES (?1, ?2, 'text', NULL, ?3, ?4, 420, 1, ?5, ?6, ?6, ?7)",
                    params![parent_id, name, len, mime, origin_chat, now, hash],
                )?;
                let id = conn.last_insert_rowid();
                let sealed = self.seal_text(id, 1, content)?;
                conn.execute(
                    "UPDATE vfs_nodes SET content_enc = ?1 WHERE id = ?2",
                    params![sealed, id],
                )?;
                (id, 1)
            }
            Some(row) => {
                if row.kind == NodeKind::Dir {
                    return Err(VfsError::IsADirectory(path.to_string()));
                }
                self.check_read_before_edit(conn, &row)?;
                let version = row.version + 1;
                let (bytes, nodes) = self.active_usage(conn)?;
                let delta = len.saturating_sub(row.size);
                self.quota.check_growth(bytes, nodes, delta, 0)?;
                let sealed = self.seal_text(row.id, version, content)?;
                conn.execute(
                    "UPDATE vfs_nodes
                     SET content_enc = ?1, size = ?2, mime = ?3, version = ?4,
                         updated_at = ?5, hash = ?6, kind = 'text'
                     WHERE id = ?7",
                    params![sealed, len, mime, version, now, hash, row.id],
                )?;
                (row.id, version)
            }
        };

        // 本次写入视为已读：后续编辑不必再读一遍。
        self.record_read(conn, id, version, Some(&hash), false)?;

        let row = self
            .lookup(conn, path)?
            .ok_or_else(|| VfsError::Storage("just-written node disappeared".to_owned()))?;
        Ok(self.meta(path, &row))
    }

    /// 递归删除子树，返回删除的节点 id（用于清理 blob 文件）。
    fn delete_subtree(&self, conn: &Connection, id: i64) -> Result<Vec<i64>, VfsError> {
        let mut statement = conn.prepare(
            "WITH RECURSIVE sub(id) AS (
                 SELECT id FROM vfs_nodes WHERE id = ?1
                 UNION ALL
                 SELECT n.id FROM vfs_nodes n JOIN sub s ON n.parent_id = s.id
             )
             SELECT id FROM sub",
        )?;
        let ids: Vec<i64> = statement
            .query_map(params![id], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        drop(statement);

        for node_id in &ids {
            // 目录也要清理，blobs 目录可能不存在，忽略 NotFound。
            let dir = self.storage.join("blobs").join(node_id.to_string());
            if dir.exists() {
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
        conn.execute(
            "WITH RECURSIVE sub(id) AS (
                 SELECT id FROM vfs_nodes WHERE id = ?1
                 UNION ALL
                 SELECT n.id FROM vfs_nodes n JOIN sub s ON n.parent_id = s.id
             )
             DELETE FROM vfs_nodes WHERE id IN (SELECT id FROM sub)",
            params![id],
        )?;
        Ok(ids)
    }
}

// ---------------------------------------------------------------------------
// 数据行
// ---------------------------------------------------------------------------

/// `vfs_nodes` 的一行。
#[derive(Debug, Clone)]
struct NodeRow {
    id: i64,
    parent_id: Option<i64>,
    name: String,
    kind: NodeKind,
    content_enc: Option<Vec<u8>>,
    size: u64,
    mime: Option<String>,
    version: u64,
    created_at: String,
    updated_at: String,
    hash: Option<String>,
    trashed_at: Option<String>,
    trash_origin: Option<String>,
}

impl NodeRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let kind: String = row.get(3)?;
        let kind = NodeKind::parse(&kind).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                format!("unknown node kind {kind:?}").into(),
            )
        })?;
        Ok(Self {
            id: row.get(0)?,
            parent_id: row.get(1)?,
            name: row.get(2)?,
            kind,
            content_enc: row.get(4)?,
            size: row.get(5)?,
            mime: row.get(6)?,
            version: row.get(7)?,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
            hash: row.get(10)?,
            trashed_at: row.get(11)?,
            trash_origin: row.get(12)?,
        })
    }
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

/// SHA-256 十六进制摘要。
fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

// ---------------------------------------------------------------------------
// Vfs 实现
// ---------------------------------------------------------------------------

impl Vfs for VfsStore {
    fn stat(&self, path: &VfsPath) -> Result<NodeMeta, VfsError> {
        let conn = self.db.lock();
        let row = self
            .lookup(&conn, path)?
            .ok_or_else(|| VfsError::NotFound(path.to_string()))?;
        Ok(self.meta(path, &row))
    }

    fn list(&self, path: &VfsPath) -> Result<Vec<NodeMeta>, VfsError> {
        let conn = self.db.lock();
        let row = self
            .lookup(&conn, path)?
            .ok_or_else(|| VfsError::NotFound(path.to_string()))?;
        if row.kind != NodeKind::Dir {
            return Err(VfsError::NotADirectory(path.to_string()));
        }

        let under_trash = path.starts_with(&self.trash_path());
        let mut statement = conn.prepare(
            "SELECT id, parent_id, name, kind, content_enc, size, mime, version, created_at,
                    updated_at, hash, trashed_at, trash_origin
             FROM vfs_nodes
             WHERE parent_id = ?1 AND (trashed_at IS NULL OR ?2 = 1)
             ORDER BY name ASC",
        )?;
        let rows = statement
            .query_map(params![row.id, under_trash as i32], NodeRow::from_row)?
            .collect::<Result<Vec<_>, _>>()?;

        let mut result = Vec::with_capacity(rows.len());
        for child in rows {
            let child_path = path.join(&child.name)?;
            result.push(self.meta(&child_path, &child));
        }
        Ok(result)
    }

    fn read(
        &self,
        path: &VfsPath,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<FileContent, VfsError> {
        let conn = self.db.lock();
        let row = self
            .lookup(&conn, path)?
            .ok_or_else(|| VfsError::NotFound(path.to_string()))?;
        if row.kind == NodeKind::Dir {
            return Err(VfsError::IsADirectory(path.to_string()));
        }

        if row.kind == NodeKind::Text {
            let text = self.open_text(&row)?;
            let total = text.len() as u64;
            let full = offset.is_none() && limit.is_none();
            let (sliced, is_partial) = if full {
                (text.clone(), false)
            } else {
                let start = offset.unwrap_or(0) as usize;
                let end = match limit {
                    Some(limit) => start.saturating_add(limit as usize),
                    None => usize::MAX,
                };
                let chars: Vec<char> = text.chars().collect();
                if start > chars.len() {
                    return Err(VfsError::RangeOutOfBounds);
                }
                let end = end.min(chars.len());
                (chars[start..end].iter().collect(), true)
            };

            self.record_read(&conn, row.id, row.version, row.hash.as_deref(), is_partial)?;
            Ok(FileContent::Text {
                text: sliced,
                total_bytes: total,
                is_partial,
            })
        } else {
            // 二进制只返回元信息；读取元信息视为完整读取，允许后续覆盖。
            self.record_read(&conn, row.id, row.version, row.hash.as_deref(), false)?;
            Ok(FileContent::Metadata(self.meta(path, &row)))
        }
    }

    fn write_text(
        &self,
        path: &VfsPath,
        content: &str,
        origin_chat: Option<&str>,
    ) -> Result<NodeMeta, VfsError> {
        if is_dynamic(path) {
            return Err(VfsError::Storage(format!(
                "{path} is a read-only dynamic view"
            )));
        }
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;
        let meta = self.write_text_tx(&tx, path, content, origin_chat)?;
        tx.commit()?;
        Ok(meta)
    }

    fn edit_text(
        &self,
        path: &VfsPath,
        old_text: &str,
        new_text: &str,
        replace_all: bool,
    ) -> Result<NodeMeta, VfsError> {
        if old_text.is_empty() {
            return Err(VfsError::EditNotFound);
        }
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;

        let row = self
            .lookup(&tx, path)?
            .ok_or_else(|| VfsError::NotFound(path.to_string()))?;
        if row.kind != NodeKind::Text {
            return Err(VfsError::Storage(format!("{path} is not a text file")));
        }
        self.check_read_before_edit(&tx, &row)?;

        let current = self.open_text(&row)?;
        let count = current.matches(old_text).count();
        if count == 0 {
            return Err(VfsError::EditNotFound);
        }
        if count > 1 && !replace_all {
            return Err(VfsError::EditNotUnique { count });
        }
        let updated = if replace_all {
            current.replace(old_text, new_text)
        } else {
            current.replacen(old_text, new_text, 1)
        };

        let meta = self.write_text_tx(&tx, path, &updated, None)?;
        tx.commit()?;
        Ok(meta)
    }

    fn write_binary(
        &self,
        path: &VfsPath,
        data: &[u8],
        kind: NodeKind,
    ) -> Result<NodeMeta, VfsError> {
        if kind == NodeKind::Dir {
            return Err(VfsError::IsADirectory(path.to_string()));
        }
        if is_dynamic(path) {
            return Err(VfsError::Storage(format!(
                "{path} is a read-only dynamic view"
            )));
        }
        let len = data.len() as u64;
        self.quota.check_blob(len)?;
        self.quota.check_disk(&self.storage)?;

        let mut conn = self.db.lock();
        let tx = conn.transaction()?;

        let existing = self.lookup(&tx, path)?;
        if let Some(row) = &existing {
            if row.kind == NodeKind::Dir {
                return Err(VfsError::IsADirectory(path.to_string()));
            }
            self.check_read_before_edit(&tx, row)?;
        }
        let parent_id = self.ensure_parent(&tx, path, &now())?;
        let name = path.file_name().expect("non-root path has a name");
        let now_value = now();
        let hash = hash_bytes(data);
        let mime = guess_mime(name).unwrap_or("application/octet-stream");

        let (bytes, nodes) = self.active_usage(&tx)?;
        let delta = len.saturating_sub(existing.as_ref().map_or(0, |row| row.size));
        self.quota
            .check_growth(bytes, nodes, delta, u64::from(existing.is_none()))?;

        let (id, version) = match &existing {
            None => {
                tx.execute(
                    "INSERT INTO vfs_nodes
                        (parent_id, name, kind, size, mime, mode, version, created_at, updated_at, hash)
                     VALUES (?1, ?2, ?3, ?4, ?5, 420, 1, ?6, ?6, ?7)",
                    params![parent_id, name, kind.as_str(), len, mime, now_value, hash],
                )?;
                (tx.last_insert_rowid(), 1u64)
            }
            Some(row) => (row.id, row.version + 1),
        };

        // 先把新分块写进临时目录，再整体替换旧目录，避免中途失败留下半份数据。
        let blobs = self.storage.join("blobs");
        let target = blobs.join(id.to_string());
        let staging = blobs.join(format!("{id}.tmp-{}", std::process::id()));
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        std::fs::create_dir_all(&staging)?;

        tx.execute("DELETE FROM vfs_chunks WHERE node_id = ?1", params![id])?;
        let mut offset = 0u64;
        for (index, chunk) in data.chunks(BLOB_CHUNK_BYTES).enumerate() {
            let sealed = self.seal_blob(id, index, chunk)?;
            std::fs::write(staging.join(index.to_string()), sealed)?;
            tx.execute(
                "INSERT INTO vfs_chunks (node_id, idx, offset, length, hash)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id,
                    index as i64,
                    offset as i64,
                    chunk.len() as i64,
                    hash_bytes(chunk)
                ],
            )?;
            offset += chunk.len() as u64;
        }

        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        std::fs::rename(&staging, &target)?;

        tx.execute(
            "UPDATE vfs_nodes
             SET size = ?1, mime = ?2, version = ?3, updated_at = ?4, hash = ?5, kind = ?6
             WHERE id = ?7",
            params![len, mime, version, now_value, hash, kind.as_str(), id],
        )?;
        self.record_read(&tx, id, version, Some(&hash), false)?;
        tx.commit()?;
        // 先释放连接锁再重新读取，否则会自锁。
        drop(conn);

        let conn = self.db.lock();
        let row = self
            .lookup(&conn, path)?
            .ok_or_else(|| VfsError::Storage("just-written binary disappeared".to_owned()))?;
        Ok(self.meta(path, &row))
    }

    fn mkdir_all(&self, path: &VfsPath) -> Result<(), VfsError> {
        if is_dynamic(path) {
            return Err(VfsError::Storage(format!(
                "{path} is a read-only dynamic view"
            )));
        }
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;
        let now = now();
        self.ensure_dir(&tx, path, &now)?;
        tx.commit()?;
        Ok(())
    }

    fn remove(&self, path: &VfsPath, permanent: bool) -> Result<RemoveOutcome, VfsError> {
        if path.is_root() {
            return Err(VfsError::Storage("cannot remove the VFS root".to_owned()));
        }
        let trash = self.trash_path();
        if path == &trash {
            return Err(VfsError::Storage("cannot remove /trash itself".to_owned()));
        }

        let mut conn = self.db.lock();
        let tx = conn.transaction()?;

        let row = self
            .lookup(&tx, path)?
            .ok_or_else(|| VfsError::NotFound(path.to_string()))?;
        let in_trash = path.starts_with(&trash);

        if permanent || in_trash {
            let removed = self.delete_subtree(&tx, row.id)?.len() as u64;
            tx.commit()?;
            return Ok(RemoveOutcome {
                trashed_to: None,
                recursive: row.kind == NodeKind::Dir,
                removed_nodes: removed,
            });
        }

        // 移入回收站：名字带上时间戳，并保证 /trash 下不重名。
        let trash_root = self
            .lookup(&tx, &trash)?
            .ok_or_else(|| VfsError::Storage("/trash is missing".to_owned()))?;
        let stamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
        let base = format!("{stamp}-{}", row.name);
        let mut candidate = base.clone();
        let mut suffix = 2;
        while tx
            .query_row(
                "SELECT 1 FROM vfs_nodes WHERE parent_id = ?1 AND name = ?2",
                params![trash_root.id, candidate],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            candidate = format!("{base}-{suffix}");
            suffix += 1;
        }

        let origin = path.to_string();
        let at = now();
        tx.execute(
            "UPDATE vfs_nodes
             SET parent_id = ?1, name = ?2, trashed_at = ?3, trash_origin = ?4
             WHERE id = ?5",
            params![trash_root.id, candidate, at, origin, row.id],
        )?;
        tx.execute(
            "WITH RECURSIVE sub(id) AS (
                 SELECT id FROM vfs_nodes WHERE id = ?1
                 UNION ALL
                 SELECT n.id FROM vfs_nodes n JOIN sub s ON n.parent_id = s.id
             )
             UPDATE vfs_nodes SET trashed_at = ?2 WHERE id IN (SELECT id FROM sub)",
            params![row.id, at],
        )?;

        tx.commit()?;
        Ok(RemoveOutcome {
            trashed_to: Some(trash.join(&candidate)?),
            recursive: row.kind == NodeKind::Dir,
            removed_nodes: 0,
        })
    }

    fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<(), VfsError> {
        if from.is_root() || to.is_root() {
            return Err(VfsError::Storage("cannot rename the VFS root".to_owned()));
        }
        if to.starts_with(from) && from != to {
            return Err(VfsError::MoveIntoItself);
        }
        let trash = self.trash_path();
        if from == &trash || to == &trash {
            return Err(VfsError::Storage("cannot rename /trash itself".to_owned()));
        }

        let mut conn = self.db.lock();
        let tx = conn.transaction()?;

        let row = self
            .lookup(&tx, from)?
            .ok_or_else(|| VfsError::NotFound(from.to_string()))?;
        if self.lookup(&tx, to)?.is_some() {
            return Err(VfsError::AlreadyExists(to.to_string()));
        }
        let target_parent = self.ensure_parent(&tx, to, &now())?;
        let target_name = to.file_name().expect("non-root path has a name");

        let was_trashed = row.trashed_at.is_some();
        let going_to_trash = to.starts_with(&trash);
        let trashed_at = match (was_trashed, going_to_trash) {
            (_, true) => Some(now()),
            (true, false) => None,
            (false, false) => None,
        };

        tx.execute(
            "UPDATE vfs_nodes
             SET parent_id = ?1, name = ?2, trashed_at = ?3, trash_origin = ?4, updated_at = ?5
             WHERE id = ?6",
            params![
                target_parent,
                target_name,
                trashed_at,
                if going_to_trash && !was_trashed {
                    Some(from.to_string())
                } else {
                    row.trash_origin.clone()
                },
                now(),
                row.id
            ],
        )?;

        // 整棵子树的回收状态跟随根节点：移出回收站即恢复，移入即全部标记。
        tx.execute(
            "WITH RECURSIVE sub(id) AS (
                 SELECT id FROM vfs_nodes WHERE id = ?1
                 UNION ALL
                 SELECT n.id FROM vfs_nodes n JOIN sub s ON n.parent_id = s.id
             )
             UPDATE vfs_nodes SET trashed_at = ?2 WHERE id IN (SELECT id FROM sub)",
            params![row.id, trashed_at],
        )?;

        tx.commit()?;
        Ok(())
    }

    fn copy(&self, from: &VfsPath, to: &VfsPath) -> Result<(), VfsError> {
        if from == to || to.starts_with(from) {
            return Err(VfsError::MoveIntoItself);
        }
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;

        let source = self
            .lookup(&tx, from)?
            .ok_or_else(|| VfsError::NotFound(from.to_string()))?;
        if self.lookup(&tx, to)?.is_some() {
            return Err(VfsError::AlreadyExists(to.to_string()));
        }
        let target_parent = self.ensure_parent(&tx, to, &now())?;
        let target_name = to.file_name().expect("non-root path has a name");
        self.copy_node(&tx, &source, target_parent, target_name, to)?;

        tx.commit()?;
        Ok(())
    }

    fn usage(&self) -> Result<Usage, VfsError> {
        let conn = self.db.lock();
        let (active_bytes, nodes): (u64, u64) = conn.query_row(
            "SELECT COALESCE(SUM(CASE WHEN kind != 'dir' THEN size ELSE 0 END), 0), COUNT(*)
             FROM vfs_nodes WHERE trashed_at IS NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let (trash_bytes, trash_nodes): (u64, u64) = conn.query_row(
            "SELECT COALESCE(SUM(CASE WHEN kind != 'dir' THEN size ELSE 0 END), 0), COUNT(*)
             FROM vfs_nodes WHERE trashed_at IS NOT NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(Usage {
            active_bytes,
            trash_bytes,
            nodes,
            trash_nodes,
        })
    }

    fn purge_trash(&self) -> Result<u64, VfsError> {
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;

        let trash = self.trash_path();
        let Some(trash_root) = self.lookup(&tx, &trash)? else {
            return Ok(0);
        };

        let entries = {
            let mut statement = tx.prepare(
                "SELECT id, trashed_at FROM vfs_nodes
                 WHERE parent_id = ?1 AND trashed_at IS NOT NULL
                 ORDER BY trashed_at ASC",
            )?;
            statement
                .query_map(params![trash_root.id], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };

        let (trash_bytes, _) = {
            let value: (u64, u64) = tx.query_row(
                "SELECT COALESCE(SUM(CASE WHEN kind != 'dir' THEN size ELSE 0 END), 0), COUNT(*)
                 FROM vfs_nodes WHERE trashed_at IS NOT NULL",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            value
        };

        // 回收站上限：总量的 10%（总量为 0 时不按比例清理）。
        let cap = match self.quota.max_total_bytes() {
            0 => 0,
            total => total / 10,
        };
        let cutoff = Utc::now() - chrono::Duration::days(TRASH_RETENTION_DAYS);

        let mut deleted = 0u64;
        let mut remaining = trash_bytes;
        for (id, trashed_at) in entries {
            let expired = trashed_at
                .as_deref()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .is_some_and(|time| time < cutoff);
            let over_cap = cap != 0 && remaining > cap;
            if expired || over_cap {
                let size: u64 = tx.query_row(
                    "WITH RECURSIVE sub(id) AS (
                         SELECT id FROM vfs_nodes WHERE id = ?1
                         UNION ALL
                         SELECT n.id FROM vfs_nodes n JOIN sub s ON n.parent_id = s.id
                     )
                     SELECT COALESCE(SUM(CASE WHEN kind != 'dir' THEN size ELSE 0 END), 0)
                     FROM vfs_nodes WHERE id IN (SELECT id FROM sub)",
                    params![id],
                    |row| row.get(0),
                )?;
                let removed = self.delete_subtree(&tx, id)?.len() as u64;
                deleted += removed;
                remaining = remaining.saturating_sub(size);
            }
        }

        tx.commit()?;
        Ok(deleted)
    }
}

impl VfsStore {
    /// 递归复制节点。
    fn copy_node(
        &self,
        conn: &Connection,
        source: &NodeRow,
        parent_id: i64,
        name: &str,
        path: &VfsPath,
    ) -> Result<i64, VfsError> {
        let now_value = now();
        match source.kind {
            NodeKind::Dir => {
                conn.execute(
                    "INSERT INTO vfs_nodes
                        (parent_id, name, kind, size, mode, version, created_at, updated_at)
                     VALUES (?1, ?2, 'dir', 0, 493, 1, ?3, ?3)",
                    params![parent_id, name, now_value],
                )?;
                let new_id = conn.last_insert_rowid();

                let children = {
                    let mut statement = conn.prepare(
                        "SELECT id, parent_id, name, kind, content_enc, size, mime, version, created_at,
                                updated_at, hash, trashed_at, trash_origin
                         FROM vfs_nodes WHERE parent_id = ?1 ORDER BY name ASC",
                    )?;
                    statement
                        .query_map(params![source.id], NodeRow::from_row)?
                        .collect::<Result<Vec<_>, _>>()?
                };
                for child in children {
                    let child_path = path.join(&child.name)?;
                    self.copy_node(conn, &child, new_id, &child.name, &child_path)?;
                }
                Ok(new_id)
            }
            NodeKind::Text => {
                let text = self.open_text(source)?;
                // 新节点 id 要等插入后才知道，先插入占位再回填密文。
                conn.execute(
                    "INSERT INTO vfs_nodes
                        (parent_id, name, kind, content_enc, size, mime, mode, version,
                         created_at, updated_at, hash)
                     VALUES (?1, ?2, 'text', NULL, ?3, ?4, 420, 1, ?5, ?5, ?6)",
                    params![
                        parent_id,
                        name,
                        source.size,
                        source.mime,
                        now_value,
                        source.hash
                    ],
                )?;
                let new_id = conn.last_insert_rowid();
                let sealed = self.seal_text(new_id, 1, &text)?;
                conn.execute(
                    "UPDATE vfs_nodes SET content_enc = ?1 WHERE id = ?2",
                    params![sealed, new_id],
                )?;
                Ok(new_id)
            }
            _ => {
                // 二进制：读出全部明文再按新 id 重新分块加密。
                let mut data = Vec::with_capacity(source.size as usize);
                let chunks: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM vfs_chunks WHERE node_id = ?1",
                    params![source.id],
                    |row| row.get(0),
                )?;
                for idx in 0..chunks {
                    let aad = format!("blob:{}:{idx}", source.id);
                    let sealed = std::fs::read(self.blob_path(source.id, idx as usize))?;
                    let plain = self
                        .key
                        .open(BLOB_PURPOSE, aad.as_bytes(), &sealed)
                        .map_err(VfsError::from)?;
                    data.extend_from_slice(&plain);
                }

                conn.execute(
                    "INSERT INTO vfs_nodes
                        (parent_id, name, kind, size, mime, mode, version, created_at, updated_at, hash)
                     VALUES (?1, ?2, ?3, ?4, ?5, 420, 1, ?6, ?6, ?7)",
                    params![
                        parent_id,
                        name,
                        source.kind.as_str(),
                        source.size,
                        source.mime,
                        now_value,
                        source.hash
                    ],
                )?;
                let new_id = conn.last_insert_rowid();

                let target = self.storage.join("blobs").join(new_id.to_string());
                std::fs::create_dir_all(&target)?;
                let mut offset = 0u64;
                for (index, chunk) in data.chunks(BLOB_CHUNK_BYTES).enumerate() {
                    let sealed = self.seal_blob(new_id, index, chunk)?;
                    std::fs::write(target.join(index.to_string()), sealed)?;
                    conn.execute(
                        "INSERT INTO vfs_chunks (node_id, idx, offset, length, hash)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            new_id,
                            index as i64,
                            offset as i64,
                            chunk.len() as i64,
                            hash_bytes(chunk)
                        ],
                    )?;
                    offset += chunk.len() as u64;
                }
                Ok(new_id)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(limits: Limits) -> (VfsStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([7u8; 32]));
        let store = VfsStore::open(db, key, dir.path().to_path_buf(), limits).unwrap();
        (store, dir)
    }

    fn store() -> (VfsStore, tempfile::TempDir) {
        store_with(Limits::default())
    }

    fn path(raw: &str) -> VfsPath {
        VfsPath::parse(raw).unwrap()
    }

    #[test]
    fn seeds_exist() {
        let (store, _dir) = store();
        for file in ["/AGENTS.md", "/MEMORY.md", "/USER.md"] {
            let meta = store.stat(&path(file)).unwrap();
            assert_eq!(meta.kind, NodeKind::Text);
            assert_eq!(meta.size, 0);
        }
        for dir in [
            "/memory/topics",
            "/memory/notes",
            "/agents",
            "/trash",
            "/tmp",
        ] {
            assert_eq!(store.stat(&path(dir)).unwrap().kind, NodeKind::Dir);
        }
    }

    #[test]
    fn write_then_read_roundtrip() {
        let (store, _dir) = store();
        let meta = store
            .write_text(&path("/notes/a.md"), "hello 世界", None)
            .unwrap();
        assert_eq!(meta.size, "hello 世界".len() as u64);
        assert_eq!(meta.version, 1);

        let content = store.read(&path("/notes/a.md"), None, None).unwrap();
        match content {
            FileContent::Text {
                text,
                total_bytes,
                is_partial,
            } => {
                assert_eq!(text, "hello 世界");
                assert_eq!(total_bytes, "hello 世界".len() as u64);
                assert!(!is_partial);
            }
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn text_content_is_encrypted_at_rest() {
        let (store, _dir) = store();
        store
            .write_text(&path("/secret.md"), "plaintext-marker", None)
            .unwrap();
        let conn = store.db.lock();
        let sealed: Vec<u8> = conn
            .query_row(
                "SELECT content_enc FROM vfs_nodes WHERE name = 'secret.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !sealed
                .windows(16)
                .any(|window| window == b"plaintext-marker")
        );
    }

    #[test]
    fn overwrite_requires_a_full_read() {
        let (store, _dir) = store();
        store.write_text(&path("/a.md"), "one", None).unwrap();

        // 写入本身会记录“已读”；清除记录模拟外部已存在、本进程没读过的文件。
        {
            let conn = store.db.lock();
            conn.execute("DELETE FROM vfs_reads", []).unwrap();
        }
        let error = store.write_text(&path("/a.md"), "two", None).unwrap_err();
        assert_eq!(error, VfsError::NotReadYet);

        store.read(&path("/a.md"), None, None).unwrap();
        let meta = store.write_text(&path("/a.md"), "two", None).unwrap();
        assert_eq!(meta.version, 2);
    }

    #[test]
    fn partial_read_does_not_satisfy_edit() {
        let (store, _dir) = store();
        store
            .write_text(&path("/a.md"), "0123456789", None)
            .unwrap();
        let content = store.read(&path("/a.md"), Some(0), Some(4)).unwrap();
        match content {
            FileContent::Text { is_partial, .. } => assert!(is_partial),
            _ => panic!("expected text"),
        }
        assert_eq!(
            store.write_text(&path("/a.md"), "x", None).unwrap_err(),
            VfsError::PartialRead
        );
    }

    #[test]
    fn stale_read_is_rejected() {
        let (store, _dir) = store();
        store.write_text(&path("/a.md"), "one", None).unwrap();
        store.read(&path("/a.md"), None, None).unwrap();

        // 模拟外部修改：直接改版本号，读取记录保持不变。
        {
            let conn = store.db.lock();
            conn.execute(
                "UPDATE vfs_nodes SET version = version + 1 WHERE name = 'a.md'",
                [],
            )
            .unwrap();
        }
        assert_eq!(
            store.write_text(&path("/a.md"), "two", None).unwrap_err(),
            VfsError::ModifiedSinceRead
        );
    }

    #[test]
    fn edit_requires_unique_match_unless_replace_all() {
        let (store, _dir) = store();
        store.write_text(&path("/a.md"), "foo foo", None).unwrap();
        store.read(&path("/a.md"), None, None).unwrap();

        assert_eq!(
            store
                .edit_text(&path("/a.md"), "foo", "bar", false)
                .unwrap_err(),
            VfsError::EditNotUnique { count: 2 }
        );
        let meta = store.edit_text(&path("/a.md"), "foo", "bar", true).unwrap();
        assert_eq!(meta.version, 2);

        let content = store.read(&path("/a.md"), None, None).unwrap();
        match content {
            FileContent::Text { text, .. } => assert_eq!(text, "bar bar"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn edit_missing_text_reports_not_found() {
        let (store, _dir) = store();
        store.write_text(&path("/a.md"), "abc", None).unwrap();
        store.read(&path("/a.md"), None, None).unwrap();
        assert_eq!(
            store
                .edit_text(&path("/a.md"), "zzz", "y", false)
                .unwrap_err(),
            VfsError::EditNotFound
        );
    }

    #[test]
    fn mkdir_list_and_stat() {
        let (store, _dir) = store();
        store.mkdir_all(&path("/projects/alpha")).unwrap();
        store
            .write_text(&path("/projects/alpha/README.md"), "hi", None)
            .unwrap();

        let listing = store.list(&path("/projects/alpha")).unwrap();
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].path, path("/projects/alpha/README.md"));

        assert_eq!(
            store.list(&path("/projects/alpha/README.md")).unwrap_err(),
            VfsError::NotADirectory("/projects/alpha/README.md".to_owned())
        );
    }

    #[test]
    fn remove_moves_to_trash_and_restores() {
        let (store, _dir) = store();
        store
            .write_text(&path("/notes/a.md"), "keep me", None)
            .unwrap();

        let outcome = store.remove(&path("/notes/a.md"), false).unwrap();
        let trashed = outcome.trashed_to.unwrap();
        assert!(trashed.to_string().starts_with("/trash/"));
        assert!(store.stat(&path("/notes/a.md")).is_err());
        assert!(store.stat(&trashed).is_ok());

        store.rename(&trashed, &path("/notes/a.md")).unwrap();
        assert!(store.stat(&path("/notes/a.md")).is_ok());
        assert!(store.list(&path("/trash")).unwrap().is_empty());
    }

    #[test]
    fn permanent_remove_deletes_subtree() {
        let (store, _dir) = store();
        store.mkdir_all(&path("/dir")).unwrap();
        store.write_text(&path("/dir/a.md"), "a", None).unwrap();
        store.write_text(&path("/dir/b.md"), "b", None).unwrap();

        let outcome = store.remove(&path("/dir"), true).unwrap();
        assert!(outcome.recursive);
        assert_eq!(outcome.removed_nodes, 3);
        assert!(store.stat(&path("/dir")).is_err());
    }

    #[test]
    fn binary_chunks_roundtrip_and_encrypted() {
        let (store, _dir) = store();
        let data = vec![7u8; BLOB_CHUNK_BYTES + 123];
        let meta = store
            .write_binary(&path("/media/big.bin"), &data, NodeKind::File)
            .unwrap();
        assert_eq!(meta.size, data.len() as u64);

        let first = store.read_blob_chunk(&path("/media/big.bin"), 0).unwrap();
        let second = store.read_blob_chunk(&path("/media/big.bin"), 1).unwrap();
        assert_eq!(first.len(), BLOB_CHUNK_BYTES);
        assert_eq!(second.len(), 123);
        assert!(first.iter().all(|byte| *byte == 7));

        // 直接扫描 blob 目录，确认落盘内容是密文。
        let raw = std::fs::read_dir(_dir.path().join("blobs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let chunk_file = std::fs::read_dir(raw.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let bytes = std::fs::read(chunk_file.path()).unwrap();
        assert!(
            !bytes.iter().all(|byte| *byte == 7),
            "chunk must be encrypted"
        );
    }

    #[test]
    fn binary_overwrite_requires_read() {
        let (store, _dir) = store();
        store
            .write_binary(&path("/x.bin"), b"one", NodeKind::File)
            .unwrap();
        {
            let conn = store.db.lock();
            conn.execute("DELETE FROM vfs_reads", []).unwrap();
        }
        assert_eq!(
            store
                .write_binary(&path("/x.bin"), b"two", NodeKind::File)
                .unwrap_err(),
            VfsError::NotReadYet
        );
        store.read(&path("/x.bin"), None, None).unwrap();
        let meta = store
            .write_binary(&path("/x.bin"), b"two", NodeKind::File)
            .unwrap();
        assert_eq!(meta.version, 2);
    }

    #[test]
    fn copy_copies_text_and_binary() {
        let (store, _dir) = store();
        store.write_text(&path("/a.md"), "text", None).unwrap();
        store
            .write_binary(&path("/b.bin"), b"bytes", NodeKind::File)
            .unwrap();
        store.mkdir_all(&path("/dst")).unwrap();

        store.copy(&path("/a.md"), &path("/dst/a.md")).unwrap();
        store.copy(&path("/b.bin"), &path("/dst/b.bin")).unwrap();

        match store.read(&path("/dst/a.md"), None, None).unwrap() {
            FileContent::Text { text, .. } => assert_eq!(text, "text"),
            _ => panic!("expected text"),
        }
        assert_eq!(
            store.read_blob_chunk(&path("/dst/b.bin"), 0).unwrap(),
            b"bytes"
        );
    }

    #[test]
    fn rename_rejects_move_into_self() {
        let (store, _dir) = store();
        store.mkdir_all(&path("/a")).unwrap();
        assert_eq!(
            store.rename(&path("/a"), &path("/a/b")).unwrap_err(),
            VfsError::MoveIntoItself
        );
    }

    #[test]
    fn usage_separates_active_and_trash() {
        let (store, _dir) = store();
        store.write_text(&path("/a.md"), "1234", None).unwrap();
        store.write_text(&path("/b.md"), "5678", None).unwrap();
        store.remove(&path("/b.md"), false).unwrap();

        let usage = store.usage().unwrap();
        assert_eq!(usage.active_bytes, 4);
        assert_eq!(usage.trash_bytes, 4);
        assert_eq!(usage.trash_nodes, 1);
    }

    #[test]
    fn quota_is_enforced() {
        let (store, _dir) = store_with(Limits {
            max_text_bytes: 4,
            max_blob_bytes: 4,
            max_total_bytes: 4,
            max_nodes: 100,
            min_free_bytes: 0,
        });
        assert!(store.write_text(&path("/a.md"), "12345", None).is_err());
        assert!(
            store
                .write_binary(&path("/a.bin"), b"12345", NodeKind::File)
                .is_err()
        );
        assert!(store.write_text(&path("/a.md"), "1234", None).is_ok());
        assert!(store.write_text(&path("/b.md"), "x", None).is_err());
    }

    #[test]
    fn dynamic_views_are_not_writable() {
        let (store, _dir) = store();
        assert!(
            store
                .write_text(&path("/messages/x.md"), "x", None)
                .is_err()
        );
        assert!(store.mkdir_all(&path("/sessions/x")).is_err());
    }

    #[test]
    fn trash_purge_removes_old_entries() {
        let (store, _dir) = store();
        store.write_text(&path("/a.md"), "x", None).unwrap();
        let trashed = store
            .remove(&path("/a.md"), false)
            .unwrap()
            .trashed_to
            .unwrap();

        // 手动把 trashed_at 改成 31 天前，触发保留期清理。
        {
            let conn = store.db.lock();
            let old = (Utc::now() - chrono::Duration::days(31)).to_rfc3339();
            conn.execute(
                "UPDATE vfs_nodes SET trashed_at = ?1 WHERE name = ?2",
                params![old, trashed.file_name().unwrap()],
            )
            .unwrap();
        }
        let removed = store.purge_trash().unwrap();
        assert_eq!(removed, 1);
        assert!(store.stat(&trashed).is_err());
    }
}
