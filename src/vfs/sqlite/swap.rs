//! 替换一个二进制节点的分块目录，并让它与数据库事务同进退。
//!
//! 分块在磁盘上（`blobs/<node_id>/<idx>`），分块的索引与节点的大小、hash、版本在数据库里。
//! 覆盖写要同时换掉两边，而文件系统不参加 SQLite 的事务：先换目录再提交，提交失败时数据库
//! 回到旧版本、磁盘上却已经是新内容；先提交再换目录，换到一半出错时反过来。两种情况读到的
//! 都是“旧的元数据配新的内容”（分块密文的 AAD 只绑定节点 id 与下标，不绑定版本，解密发现
//! 不了）。
//!
//! 做法是提交之前把旧目录留着：
//! 1. 新分块写进 `blobs/<id>.tmp-<pid>/`（[`BlobSwap::begin`]）；
//! 2. 旧目录改名成 `blobs/<id>.old-<旧版本>/`，新目录改名到位（[`BlobSwap::install`]）；
//! 3. 提交事务。成功就删掉旧目录（[`BlobSwap::finish`]）；之前任何一步失败都把旧目录改名
//!    回去（[`BlobSwap::rollback`]），数据库那边由事务自己回滚。
//!
//! 进程在第 2、3 步之间被杀掉时，下次打开由 [`VfsStore::recover_blob_swaps`] 收尾：备份目录
//! 名里的版本号与数据库里节点的版本相同，说明事务没提交，恢复旧目录；不同（或节点已经没了）
//! 说明提交了，删掉备份。
//!
//! 管不到的情况：数据库是 WAL + `synchronous = NORMAL`，断电可能丢掉最后几个已经提交的事务，
//! 那时磁盘上的分块可能比数据库新。进程崩溃（包括 `panic = "abort"`）不属于这种情况。

use super::*;

/// 写到一半的新分块目录的名字里带的标记（`<id>.tmp-<pid>`）。
pub(super) const STAGING_MARK: &str = ".tmp-";
/// 被替换下来的旧分块目录的名字里带的标记（`<id>.old-<旧版本>`）。
const BACKUP_MARK: &str = ".old-";

/// 一次分块目录替换。
pub(super) struct BlobSwap {
    /// 正式位置 `blobs/<id>/`。
    target: PathBuf,
    /// 新分块先写到这里。
    staging: PathBuf,
    /// 旧目录的备份位置；节点是新建的（没有旧版本）时为 `None`。
    backup: Option<PathBuf>,
    /// 新目录是否已经改名到了正式位置。
    installed: bool,
}

impl BlobSwap {
    /// 准备一个空的暂存目录。`old_version` 是被覆盖节点现在的版本，新建节点传 `None`。
    pub(super) fn begin(blobs: &Path, id: i64, old_version: Option<u64>) -> std::io::Result<Self> {
        let staging = blobs.join(format!("{id}{STAGING_MARK}{}", std::process::id()));
        // 同一个进程上一次写这个节点时失败、又没清干净的残留。
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        std::fs::create_dir_all(&staging)?;
        Ok(Self {
            target: blobs.join(id.to_string()),
            staging,
            backup: old_version.map(|version| blobs.join(format!("{id}{BACKUP_MARK}{version}"))),
            installed: false,
        })
    }

    /// 新分块写到哪个目录。
    pub(super) fn staging(&self) -> &Path {
        &self.staging
    }

    /// 把暂存目录换到正式位置，旧目录留作备份。之后必须调用 [`finish`](Self::finish) 或
    /// [`rollback`](Self::rollback)。
    pub(super) fn install(&mut self) -> std::io::Result<()> {
        if self.target.exists() {
            match &self.backup {
                Some(backup) => std::fs::rename(&self.target, backup)?,
                // 新建的节点不该已经有目录：这是同一个 id 以前的主人留下的孤儿（SQLite 会把
                // 回滚掉的、或已删除的最大 id 再发一次；删节点时目录没删干净就会留下），
                // 没有任何节点指向它。
                None => std::fs::remove_dir_all(&self.target)?,
            }
        }
        std::fs::rename(&self.staging, &self.target)?;
        self.installed = true;
        Ok(())
    }

    /// 事务已经提交：旧目录不再需要。删不掉只是多占空间（下次打开时会再清一次），不算失败。
    pub(super) fn finish(self) {
        if let Some(backup) = &self.backup
            && backup.exists()
            && let Err(error) = std::fs::remove_dir_all(backup)
        {
            tracing::warn!(%error, dir = %backup.display(), "cannot remove a replaced blob directory");
        }
    }

    /// 事务没有提交：把磁盘恢复成开始之前的样子。恢复不了的部分记日志，留给下次打开时的
    /// [`VfsStore::recover_blob_swaps`]。
    pub(super) fn rollback(self) {
        let remove = |dir: &Path| {
            if dir.exists()
                && let Err(error) = std::fs::remove_dir_all(dir)
            {
                tracing::warn!(%error, dir = %dir.display(), "cannot remove an abandoned blob directory");
            }
        };
        remove(&self.staging);
        if self.installed {
            remove(&self.target);
        }
        if let Some(backup) = &self.backup
            && backup.exists()
            && let Err(error) = std::fs::rename(backup, &self.target)
        {
            tracing::warn!(%error, dir = %backup.display(), "cannot restore a blob directory");
        }
    }
}

impl VfsStore {
    /// 收拾上次运行留下的半截替换（见模块说明）：恢复或删除备份目录，删除没装上的暂存目录。
    ///
    /// 只在打开时调用：生产环境唯一的打开点 `app::assembly::open_stores` 在拿到实例锁
    /// （`InstanceLock`）之后，不会有另一个进程正在往这些目录里写。
    pub(super) fn recover_blob_swaps(&self) {
        let blobs = self.storage.join("blobs");
        let Ok(entries) = std::fs::read_dir(&blobs) else {
            return;
        };
        let conn = self.db.lock();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let path = entry.path();
            let outcome = if let Some((id, version)) = parse_backup(&name) {
                // 备份目录：数据库里的版本没变，说明那次事务没有提交。
                let current: rusqlite::Result<Option<u64>> = conn
                    .query_row(
                        "SELECT version FROM vfs_nodes WHERE id = ?1",
                        params![id],
                        |row| row.get(0),
                    )
                    .optional();
                match current {
                    Ok(Some(current)) if current == version => {
                        let target = blobs.join(id.to_string());
                        let cleared = if target.exists() {
                            std::fs::remove_dir_all(&target)
                        } else {
                            Ok(())
                        };
                        cleared.and_then(|()| std::fs::rename(&path, &target))
                    }
                    Ok(_) => std::fs::remove_dir_all(&path),
                    // 查不出来就不动它：删错了是丢数据，留着只是多占空间。
                    Err(error) => {
                        tracing::warn!(%error, dir = %name, "cannot decide what to do with a blob backup");
                        continue;
                    }
                }
            } else if name.contains(STAGING_MARK) {
                std::fs::remove_dir_all(&path)
            } else {
                continue;
            };
            if let Err(error) = outcome {
                tracing::warn!(%error, dir = %name, "cannot clean up an interrupted blob write");
            }
        }
    }
}

/// 解析备份目录名 `<id>.old-<版本>`。
fn parse_backup(name: &str) -> Option<(i64, u64)> {
    let (id, version) = name.split_once(BACKUP_MARK)?;
    Some((id.parse().ok()?, version.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (VfsStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = VfsStore::open(
            Arc::new(Database::open_in_memory().unwrap()),
            Arc::new(MasterKey::from_bytes([5u8; 32])),
            dir.path().to_path_buf(),
            Limits::default(),
        )
        .unwrap();
        (store, dir)
    }

    fn path(raw: &str) -> VfsPath {
        VfsPath::parse(raw).unwrap()
    }

    /// 跨两个分块的内容，每个字节都是 `fill`。
    fn content(fill: u8) -> Vec<u8> {
        vec![fill; BLOB_CHUNK_BYTES + 100]
    }

    /// `blobs/` 下的条目名，排好序。
    fn blob_entries(dir: &tempfile::TempDir) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.path().join("blobs"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// 节点的 `(id, version)`。
    fn node(store: &VfsStore, raw: &str) -> (i64, u64) {
        let conn = store.db.lock();
        let row = store.lookup(&conn, &path(raw)).unwrap().unwrap();
        (row.id, row.version)
    }

    /// 覆盖写在“新目录已经换到位、事务还没提交”时失败：节点必须还是旧内容、旧版本，磁盘上
    /// 不留暂存或备份目录。以前旧目录在提交之前就被删了，数据库回滚后读到的是旧的大小与
    /// hash 配新的分块。
    #[test]
    fn a_failed_overwrite_keeps_the_old_content() {
        let (store, dir) = store();
        store
            .write_binary(&path("/tmp/a.bin"), &content(1), NodeKind::Binary)
            .unwrap();
        let (id, version) = node(&store, "/tmp/a.bin");

        // 让事务里最后那条 UPDATE 失败：它在目录替换之后。
        store
            .db
            .lock()
            .execute_batch(
                "CREATE TRIGGER fail_update BEFORE UPDATE ON vfs_nodes
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        let error = store
            .write_binary(&path("/tmp/a.bin"), &content(2), NodeKind::Binary)
            .unwrap_err();
        assert!(error.to_string().contains("injected failure"), "{error}");

        assert_eq!(store.read_bytes(&path("/tmp/a.bin")).unwrap(), content(1));
        assert_eq!(node(&store, "/tmp/a.bin"), (id, version));
        assert_eq!(blob_entries(&dir), vec![id.to_string()]);

        // 故障排除后同一次写入成功，旧目录的备份被删掉。
        store
            .db
            .lock()
            .execute_batch("DROP TRIGGER fail_update")
            .unwrap();
        store
            .write_binary(&path("/tmp/a.bin"), &content(2), NodeKind::Binary)
            .unwrap();
        assert_eq!(store.read_bytes(&path("/tmp/a.bin")).unwrap(), content(2));
        assert_eq!(node(&store, "/tmp/a.bin"), (id, version + 1));
        assert_eq!(blob_entries(&dir), vec![id.to_string()]);
    }

    /// 分块写到一半出错（下载中断、暂存分块读不出）：覆盖的节点不变；新建的节点不存在，
    /// 也不留下目录。
    #[test]
    fn a_write_that_fails_midway_leaves_nothing_behind() {
        let (store, dir) = store();
        store
            .write_binary(&path("/tmp/a.bin"), &content(1), NodeKind::Binary)
            .unwrap();
        let (id, _) = node(&store, "/tmp/a.bin");

        for target in ["/tmp/a.bin", "/tmp/new.bin"] {
            let mut chunks = [
                Ok(Cow::Owned(vec![9u8; BLOB_CHUNK_BYTES])),
                Err(VfsError::Storage("source went away".to_owned())),
            ]
            .into_iter();
            let error = store
                .write_binary_chunks(
                    &path(target),
                    NodeKind::Binary,
                    2 * BLOB_CHUNK_BYTES as u64,
                    "irrelevant",
                    &mut chunks,
                )
                .unwrap_err();
            assert!(error.to_string().contains("source went away"), "{error}");
        }

        assert_eq!(store.read_bytes(&path("/tmp/a.bin")).unwrap(), content(1));
        assert!(store.stat(&path("/tmp/new.bin")).is_err());
        assert_eq!(blob_entries(&dir), vec![id.to_string()]);
    }

    /// 进程死在“目录换了、事务没提交”之间：下次打开时按版本号判断，恢复旧目录。
    #[test]
    fn an_uncommitted_swap_is_undone_on_open() {
        let (store, dir) = store();
        store
            .write_binary(&path("/tmp/a.bin"), &content(1), NodeKind::Binary)
            .unwrap();
        let (id, version) = node(&store, "/tmp/a.bin");
        let blobs = dir.path().join("blobs");

        // 摆出崩溃现场：旧目录成了备份，正式位置是没提交的新内容，另有一个没装上的暂存目录。
        std::fs::rename(
            blobs.join(id.to_string()),
            blobs.join(format!("{id}.old-{version}")),
        )
        .unwrap();
        std::fs::create_dir(blobs.join(id.to_string())).unwrap();
        std::fs::write(blobs.join(id.to_string()).join("0"), b"uncommitted").unwrap();
        std::fs::create_dir(blobs.join(format!("{id}.tmp-4242"))).unwrap();

        store.recover_blob_swaps();
        assert_eq!(blob_entries(&dir), vec![id.to_string()]);
        assert_eq!(store.read_bytes(&path("/tmp/a.bin")).unwrap(), content(1));
    }

    /// 进程死在“事务提交了、备份还没删”之间：备份的版本号比数据库旧，删掉；节点已经不存在
    /// 的备份同样删掉。当前内容不受影响。
    #[test]
    fn a_committed_swap_only_loses_its_backup_on_open() {
        let (store, dir) = store();
        store
            .write_binary(&path("/tmp/a.bin"), &content(1), NodeKind::Binary)
            .unwrap();
        store
            .write_binary(&path("/tmp/a.bin"), &content(2), NodeKind::Binary)
            .unwrap();
        let (id, version) = node(&store, "/tmp/a.bin");
        let blobs = dir.path().join("blobs");

        std::fs::create_dir(blobs.join(format!("{id}.old-{}", version - 1))).unwrap();
        std::fs::create_dir(blobs.join("987654.old-3")).unwrap();
        // 名字不合约定的目录不是这里管的，原样留着。
        std::fs::create_dir(blobs.join("notes.old-x")).unwrap();

        store.recover_blob_swaps();
        assert_eq!(
            blob_entries(&dir),
            vec![id.to_string(), "notes.old-x".to_owned()]
        );
        assert_eq!(store.read_bytes(&path("/tmp/a.bin")).unwrap(), content(2));
    }
}
