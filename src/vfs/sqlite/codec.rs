//! VFS 的加密与 blob 编解码 helper。
//!
//! AAD 与用途域必须和 `sqlite.rs` 顶部记录的一致：文本绑定
//! `vfs:<id>:<version>`，分块绑定 `blob:<id>:<idx>`；改动这里会让已有数据无法解密。

use super::*;

impl super::VfsStore {
    pub(super) fn seal_text(&self, id: i64, version: u64, text: &str) -> Result<Vec<u8>, VfsError> {
        let aad = format!("vfs:{id}:{version}");
        self.key
            .seal(TEXT_PURPOSE, aad.as_bytes(), text.as_bytes())
            .map_err(VfsError::from)
    }

    pub(super) fn open_text(&self, row: &NodeRow) -> Result<String, VfsError> {
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

    pub(super) fn seal_blob(&self, id: i64, idx: usize, plain: &[u8]) -> Result<Vec<u8>, VfsError> {
        let aad = format!("blob:{id}:{idx}");
        self.key
            .seal(BLOB_PURPOSE, aad.as_bytes(), plain)
            .map_err(VfsError::from)
    }

    /// 解密一个二进制分块；AAD 绑定节点 id 与分块下标。
    pub(super) fn open_blob(
        &self,
        id: i64,
        idx: usize,
        sealed: &[u8],
    ) -> Result<Vec<u8>, VfsError> {
        let aad = format!("blob:{id}:{idx}");
        self.key
            .open(BLOB_PURPOSE, aad.as_bytes(), sealed)
            .map_err(VfsError::from)
    }

    pub(super) fn blob_path(&self, id: i64, idx: usize) -> PathBuf {
        self.storage
            .join("blobs")
            .join(id.to_string())
            .join(idx.to_string())
    }
}
