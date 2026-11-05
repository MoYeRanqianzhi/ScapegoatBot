//! 树操作：递归删除、复制、改名与回收站。
//!
//! 这些操作整体改动一棵子树，事务边界与递归 CTE 保持和主存储一致；
//! `Vfs` trait 的公开方法仍留在 `sqlite.rs`，这里只提供实现。

use super::*;

/// 回收站保留期（天）。
const TRASH_RETENTION_DAYS: i64 = 30;

impl super::VfsStore {
    /// 回收站根路径。
    pub(super) fn trash_path(&self) -> VfsPath {
        VfsPath::parse("/trash").expect("trash path is valid")
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

    /// 子树占用的字节与节点数（含目录自身）。
    fn subtree_usage(conn: &Connection, root_id: i64) -> Result<(u64, u64), VfsError> {
        let (bytes, nodes): (u64, u64) = conn.query_row(
            "WITH RECURSIVE sub(id, kind, size) AS (
                 SELECT id, kind, size FROM vfs_nodes WHERE id = ?1
                 UNION ALL
                 SELECT n.id, n.kind, n.size FROM vfs_nodes n JOIN sub s ON n.parent_id = s.id
             )
             SELECT COALESCE(SUM(CASE WHEN kind != 'dir' THEN size ELSE 0 END), 0), COUNT(*)
             FROM sub",
            [root_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((bytes, nodes))
    }

    pub(super) fn remove_entry(
        &self,
        path: &VfsPath,
        permanent: bool,
    ) -> Result<RemoveOutcome, VfsError> {
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

    pub(super) fn rename_entry(&self, from: &VfsPath, to: &VfsPath) -> Result<(), VfsError> {
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

    pub(super) fn copy_entry(&self, from: &VfsPath, to: &VfsPath) -> Result<(), VfsError> {
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
        // 复制同样是写入：配额与磁盘水位必须先检查，否则反复复制大文件
        // 可以绕过 `max_vfs_bytes` / 节点上限。
        let (add_bytes, add_nodes) = Self::subtree_usage(&tx, source.id)?;
        let (bytes, nodes) = self.active_usage(&tx)?;
        self.quota
            .check_growth(bytes, nodes, add_bytes, add_nodes)?;
        self.quota.check_disk(&self.storage)?;

        let target_parent = self.ensure_parent(&tx, to, &now())?;
        let target_name = to.file_name().expect("non-root path has a name");
        self.copy_node(&tx, &source, target_parent, target_name, to)?;

        tx.commit()?;
        Ok(())
    }

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
                    let sealed = std::fs::read(self.blob_path(source.id, idx as usize))?;
                    let plain = self.open_blob(source.id, idx as usize, &sealed)?;
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

    pub(super) fn purge_trash_entries(&self) -> Result<u64, VfsError> {
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
