//! 分段写入二进制节点：边收边加密暂存，最后一次提交。
//!
//! 下载（`API` 的 `local.net.download`）不能把几百 MB 的响应体整块留在内存里，也不能在
//! 网络传输期间一直持有数据库锁。做法：
//! 1. [`VfsStore::stage_blob`] 建一个暂存目录 `blobs/stage-<token>/`；
//! 2. [`BlobStage::push`] 把收到的字节攒满 1 MiB 就加密写成一个暂存分块
//!    （用途域 `vfs-stage`，AAD `stage:<token>:<idx>`），内存里最多留一个分块；
//! 3. [`VfsStore::commit_stage`] 在一个事务里逐块解密、按节点 id 重新加密写入正式位置。
//!
//! 暂存分块同样是密文，磁盘上任何时候都没有明文。`BlobStage` 被丢弃（下载失败、超限）
//! 时删除暂存目录；进程崩溃留下的暂存目录在下次 [`VfsStore::open`] 时清理。

use super::*;

/// 暂存分块的加密用途域；与正式分块分开，暂存密文不能被当成节点内容解开。
const STAGE_PURPOSE: &str = "vfs-stage";

/// 暂存目录名前缀（位于 `blobs/` 下）。
pub(super) const STAGE_PREFIX: &str = "stage-";

/// 一次分段写入。只能由 [`VfsStore::stage_blob`] 创建。
pub struct BlobStage {
    /// 加密暂存分块用的主密钥。
    key: Arc<MasterKey>,
    /// 暂存目录 `blobs/stage-<token>/`。
    dir: PathBuf,
    /// 本次暂存的随机标识，进入 AAD。
    token: String,
    /// 单个二进制节点的大小上限；`None` 表示不限。
    limit: Option<u64>,
    /// 还没凑满一个分块的字节。
    buffer: Vec<u8>,
    /// 已写出的暂存分块数。
    chunks: usize,
    /// 已收到的总字节数（含 `buffer`）。
    len: u64,
    /// 全部内容的 SHA-256，边收边算。
    hasher: Sha256,
}

impl BlobStage {
    /// 追加一段内容；凑满 1 MiB 就加密写出一个暂存分块。
    ///
    /// 超过单文件上限时报错，调用方应当丢弃这次暂存。
    pub fn push(&mut self, mut bytes: &[u8]) -> Result<(), VfsError> {
        let len = self.len.saturating_add(bytes.len() as u64);
        if let Some(limit) = self.limit
            && len > limit
        {
            return Err(VfsError::BlobTooLarge { limit });
        }
        self.len = len;
        self.hasher.update(bytes);
        while !bytes.is_empty() {
            // 填满当前分块还差多少。
            let room = BLOB_CHUNK_BYTES - self.buffer.len();
            let (head, rest) = bytes.split_at(room.min(bytes.len()));
            self.buffer.extend_from_slice(head);
            bytes = rest;
            if self.buffer.len() == BLOB_CHUNK_BYTES {
                self.flush()?;
            }
        }
        Ok(())
    }

    /// 不超过 `max_bytes` 的有效 UTF-8 内容读回成字符串；否则 `None`。`0` 表示不限。
    ///
    /// 用于“响应声称是文本”的下载：小而合法的文本存成文本节点（可 `Read` / `Edit`），
    /// 其余仍按二进制提交。
    pub fn text_within(&self, max_bytes: u64) -> Result<Option<String>, VfsError> {
        if max_bytes != 0 && self.len > max_bytes {
            return Ok(None);
        }
        let mut bytes = Vec::with_capacity(self.len as usize);
        for idx in 0..self.chunks {
            bytes.extend_from_slice(&self.read_chunk(idx)?);
        }
        bytes.extend_from_slice(&self.buffer);
        Ok(String::from_utf8(bytes).ok())
    }

    /// 已收到的字节数。
    pub fn len(&self) -> u64 {
        self.len
    }

    /// 是否还没收到任何字节。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 把 `buffer` 加密写成下一个暂存分块。
    fn flush(&mut self) -> Result<(), VfsError> {
        let aad = format!("stage:{}:{}", self.token, self.chunks);
        let sealed = self
            .key
            .seal(STAGE_PURPOSE, aad.as_bytes(), &self.buffer)
            .map_err(VfsError::from)?;
        std::fs::write(self.dir.join(self.chunks.to_string()), sealed)?;
        self.chunks += 1;
        self.buffer.clear();
        Ok(())
    }

    /// 读回第 `idx` 个暂存分块的明文。
    fn read_chunk(&self, idx: usize) -> Result<Vec<u8>, VfsError> {
        let sealed = std::fs::read(self.dir.join(idx.to_string()))?;
        let aad = format!("stage:{}:{idx}", self.token);
        self.key
            .open(STAGE_PURPOSE, aad.as_bytes(), &sealed)
            .map_err(VfsError::from)
    }
}

impl Drop for BlobStage {
    /// 不论提交与否都删除暂存目录：提交时内容已经重新加密写到正式位置。
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, dir = %self.dir.display(), "cannot remove a blob stage");
        }
    }
}

/// 存储压力；字段为 `None` 表示该项正常。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoragePressure {
    /// VFS 活跃用量（字节或节点数）达到配额的 [`PRESSURE_PERCENT`]% 时的说明。
    pub quota: Option<String>,
    /// 磁盘剩余低于写入下限时的说明。
    pub disk: Option<String>,
}

/// 用量达到配额的这个百分比就算有压力。
pub const PRESSURE_PERCENT: u64 = 90;

impl VfsStore {
    /// 当前存储压力：配额用量与磁盘剩余（`internal:storage` monitor 用）。
    pub fn pressure(&self) -> Result<StoragePressure, VfsError> {
        let usage = self.usage()?;
        let near = |used: u64, limit: u64| {
            limit != 0 && used.saturating_mul(100) >= limit.saturating_mul(PRESSURE_PERCENT)
        };
        let quota = if near(usage.active_bytes, self.quota.max_total_bytes()) {
            Some(format!(
                "VFS uses {} of {} bytes (at least {PRESSURE_PERCENT}%); writes fail once it is full",
                usage.active_bytes,
                self.quota.max_total_bytes()
            ))
        } else if near(usage.nodes, self.quota.max_nodes()) {
            Some(format!(
                "VFS holds {} of {} nodes (at least {PRESSURE_PERCENT}%); writes fail once it is full",
                usage.nodes,
                self.quota.max_nodes()
            ))
        } else {
            None
        };
        let disk = self.quota.disk_shortfall(&self.storage).map(|(free, floor)| {
            format!("free disk space is {free} bytes, below the {floor}-byte write floor; VFS writes are refused")
        });
        Ok(StoragePressure { quota, disk })
    }

    /// 单个文本节点的字节上限（0 = 不限）；分段写入的内容按文本保存前用它判断。
    pub fn text_limit(&self) -> u64 {
        self.quota.max_text_bytes()
    }

    /// 开始一次分段写入；先检查磁盘水位。
    pub fn stage_blob(&self) -> Result<BlobStage, VfsError> {
        self.quota.check_disk(&self.storage)?;
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| VfsError::Storage(format!("cannot generate a stage id: {error}")))?;
        let token = hex::encode(random);
        let dir = self
            .storage
            .join("blobs")
            .join(format!("{STAGE_PREFIX}{token}"));
        // `create_dir`（而非 `create_dir_all`）：目录已存在说明标识撞了，宁可报错。
        std::fs::create_dir(&dir)?;
        Ok(BlobStage {
            key: Arc::clone(&self.key),
            dir,
            token,
            limit: self.quota.max_blob_bytes(),
            buffer: Vec::with_capacity(BLOB_CHUNK_BYTES),
            chunks: 0,
            len: 0,
            hasher: Sha256::new(),
        })
    }

    /// 把暂存的内容提交成 `path` 处的二进制节点（覆盖写同样要求先读过）。
    ///
    /// 在一个事务里完成；期间持有数据库锁，调用方应在阻塞线程里调用。
    pub fn commit_stage(
        &self,
        mut stage: BlobStage,
        path: &VfsPath,
        kind: NodeKind,
    ) -> Result<NodeMeta, VfsError> {
        // 不满一个分块的尾巴也要写出。
        if !stage.buffer.is_empty() {
            stage.flush()?;
        }
        let hash = hex::encode(stage.hasher.clone().finalize());
        let mut chunks = (0..stage.chunks).map(|idx| stage.read_chunk(idx).map(Cow::Owned));
        self.write_binary_chunks(path, kind, stage.len, &hash, &mut chunks)
    }

    /// 删除崩溃遗留的暂存目录：分段写入的 `stage-*` 与整块写入的 `<id>.tmp-*`。
    ///
    /// 只在打开时调用：生产环境唯一的打开点 `app::assembly::open_stores` 在拿到实例锁
    /// （`InstanceLock`）之后，不会有另一个进程正在往这些目录里写。
    pub(super) fn remove_stale_stages(&self) {
        let Ok(entries) = std::fs::read_dir(self.storage.join("blobs")) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if (name.starts_with(STAGE_PREFIX) || name.contains(".tmp-"))
                && let Err(error) = std::fs::remove_dir_all(entry.path())
            {
                tracing::warn!(%error, dir = %name, "cannot remove a stale blob stage");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (VfsStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = VfsStore::open(
            Arc::new(Database::open_in_memory().unwrap()),
            Arc::new(MasterKey::from_bytes([9u8; 32])),
            dir.path().to_path_buf(),
            Limits {
                max_blob_bytes: 3 * BLOB_CHUNK_BYTES as u64,
                ..Limits::default()
            },
        )
        .unwrap();
        (store, dir)
    }

    fn stage_dirs(dir: &tempfile::TempDir) -> usize {
        std::fs::read_dir(dir.path().join("blobs"))
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(STAGE_PREFIX)
            })
            .count()
    }

    #[test]
    fn staged_content_matches_a_direct_write() {
        let (store, dir) = store();
        // 跨分块边界、分多段送入。
        let data: Vec<u8> = (0..BLOB_CHUNK_BYTES * 2 + 77)
            .map(|index| (index % 251) as u8)
            .collect();
        let mut stage = store.stage_blob().unwrap();
        for part in data.chunks(300_001) {
            stage.push(part).unwrap();
        }
        assert_eq!(stage.len(), data.len() as u64);
        let path = VfsPath::parse("/tmp/staged.bin").unwrap();
        let staged = store.commit_stage(stage, &path, NodeKind::Binary).unwrap();
        assert_eq!(store.read_bytes(&path).unwrap(), data);
        assert_eq!(stage_dirs(&dir), 0, "the stage is removed after commit");

        let direct = VfsPath::parse("/tmp/direct.bin").unwrap();
        let written = store
            .write_binary(&direct, &data, NodeKind::Binary)
            .unwrap();
        assert_eq!(staged.hash, written.hash);
        assert_eq!(staged.size, written.size);

        // 复制多分块的二进制：逐块重新加密，内容与 hash 不变。
        let copied = VfsPath::parse("/tmp/copied.bin").unwrap();
        store.copy(&path, &copied).unwrap();
        assert_eq!(store.read_bytes(&copied).unwrap(), data);
        assert_eq!(store.stat(&copied).unwrap().hash, staged.hash);

        // 文本读回：超限或不是 UTF-8 时为 None；0 表示不限。
        let mut text = store.stage_blob().unwrap();
        text.push("你好，".as_bytes()).unwrap();
        text.push(b"world").unwrap();
        assert_eq!(
            text.text_within(64).unwrap().as_deref(),
            Some("你好，world")
        );
        assert_eq!(text.text_within(0).unwrap().as_deref(), Some("你好，world"));
        assert_eq!(text.text_within(4).unwrap(), None);
        text.push(&[0xff]).unwrap();
        assert_eq!(text.text_within(64).unwrap(), None);

        // 空内容也能提交。
        let empty = store.stage_blob().unwrap();
        assert!(empty.is_empty());
        let path = VfsPath::parse("/tmp/empty.bin").unwrap();
        store.commit_stage(empty, &path, NodeKind::Binary).unwrap();
        assert!(store.read_bytes(&path).unwrap().is_empty());
    }

    #[test]
    fn pressure_reports_quota_usage_near_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = VfsStore::open(
            Arc::new(Database::open_in_memory().unwrap()),
            Arc::new(MasterKey::from_bytes([9u8; 32])),
            dir.path().to_path_buf(),
            Limits {
                max_total_bytes: 1_000,
                min_free_bytes: 0,
                ..Limits::default()
            },
        )
        .unwrap();
        assert_eq!(store.pressure().unwrap(), StoragePressure::default());
        store
            .write_binary(
                &VfsPath::parse("/tmp/fill.bin").unwrap(),
                &[0u8; 950],
                NodeKind::Binary,
            )
            .unwrap();
        let pressure = store.pressure().unwrap();
        assert!(pressure.quota.unwrap().contains("950 of 1000"));
        assert!(pressure.disk.is_none());
    }

    #[test]
    fn oversized_or_abandoned_stages_leave_nothing_behind() {
        let (store, dir) = store();
        let mut stage = store.stage_blob().unwrap();
        let error = stage
            .push(&vec![0u8; 3 * BLOB_CHUNK_BYTES + 1])
            .unwrap_err();
        assert!(matches!(error, VfsError::BlobTooLarge { .. }), "{error}");
        assert_eq!(stage_dirs(&dir), 1);
        drop(stage);
        assert_eq!(stage_dirs(&dir), 0);

        // 崩溃遗留的暂存目录在下次打开时清理。
        std::fs::create_dir_all(dir.path().join("blobs").join("stage-dead")).unwrap();
        std::fs::create_dir_all(dir.path().join("blobs").join("7.tmp-123")).unwrap();
        std::fs::create_dir_all(dir.path().join("blobs").join("8")).unwrap();
        store.remove_stale_stages();
        let left: Vec<String> = std::fs::read_dir(dir.path().join("blobs"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["8".to_owned()]);
    }
}
