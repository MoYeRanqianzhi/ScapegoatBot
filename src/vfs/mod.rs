//! 虚拟文件系统（VFS）。
//!
//! 设计依据：设计 §6。
//!
//! 分工：
//! - [`path`]：路径解析、NFC、查询参数；
//! - [`sqlite`]：SQLite 后端（文本加密、二进制分块、回收站、Read-before-Edit）；
//! - [`quota`]：配额与磁盘水位；
//! - [`dynamic`]：动态视图（`/friends`、`/messages` 等，数据源由运行时提供）。
//!
//! 本文件只定义公共类型与 trait，不包含存储细节。

use serde::{Deserialize, Serialize};

pub mod dynamic;
pub mod path;
pub mod quota;
pub mod sqlite;

pub use path::{PathError, VfsPath};

/// 节点类型（设计 §6.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Dir,
    Text,
    Binary,
    Image,
    Audio,
    Video,
    File,
}

impl NodeKind {
    /// 是否为“文件”（非目录）。
    pub fn is_file(self) -> bool {
        !matches!(self, Self::Dir)
    }

    /// 存储字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dir => "dir",
            Self::Text => "text",
            Self::Binary => "binary",
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::File => "file",
        }
    }

    /// 从存储字符串还原。
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "dir" => Self::Dir,
            "text" => Self::Text,
            "binary" => Self::Binary,
            "image" => Self::Image,
            "audio" => Self::Audio,
            "video" => Self::Video,
            "file" => Self::File,
            _ => return None,
        })
    }
}

/// 节点元信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeMeta {
    pub path: VfsPath,
    pub kind: NodeKind,
    pub size: u64,
    pub mime: Option<String>,
    pub version: u64,
    pub created_at: String,
    pub updated_at: String,
    pub hash: Option<String>,
}

/// 读取结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileContent {
    /// 文本内容。
    Text {
        text: String,
        /// 文件总字节数（不受 offset/limit 影响）。
        total_bytes: u64,
        /// 是否只读了片段；只有全量读取才满足编辑前置条件。
        is_partial: bool,
    },
    /// 二进制只返回元信息，不把内容塞进上下文（设计 §6.4）。
    Metadata(NodeMeta),
}

/// 删除结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveOutcome {
    /// 移入回收站后的路径；永久删除时为 `None`。
    pub trashed_to: Option<VfsPath>,
    /// 是否递归删除了目录。
    pub recursive: bool,
    /// 受影响的节点数。
    pub removed_nodes: u64,
}

/// 用量统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    /// 活跃节点占用字节（不含回收站）。
    pub active_bytes: u64,
    /// 回收站占用字节。
    pub trash_bytes: u64,
    /// 活跃节点数（含目录）。
    pub nodes: u64,
    /// 回收站节点数。
    pub trash_nodes: u64,
}

/// VFS 错误。所有消息面向模型，英文。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VfsError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error("no such file or directory: {0}")]
    NotFound(String),
    #[error("not a directory: {0}")]
    NotADirectory(String),
    #[error("is a directory: {0}")]
    IsADirectory(String),
    #[error("file has not been read yet. Read it first.")]
    NotReadYet,
    #[error("only part of the file was read. Read the full file before editing.")]
    PartialRead,
    #[error("file has been modified since it was read. Read it again.")]
    ModifiedSinceRead,
    #[error("old_text was not found")]
    EditNotFound,
    #[error("old_text matches {count} times; set replace_all to replace every occurrence")]
    EditNotUnique { count: usize },
    #[error("text files are limited to {limit} bytes")]
    TextTooLarge { limit: u64 },
    #[error("blobs are limited to {limit} bytes")]
    BlobTooLarge { limit: u64 },
    #[error("VFS quota exceeded ({usage} of {limit} bytes)")]
    QuotaExceeded { usage: u64, limit: u64 },
    #[error("node limit exceeded ({limit} nodes)")]
    NodeLimitExceeded { limit: u64 },
    #[error("not enough free disk space (need at least {need} bytes)")]
    DiskFull { need: u64 },
    #[error("file already exists: {0}")]
    AlreadyExists(String),
    #[error("directory is not empty: {0}")]
    DirectoryNotEmpty(String),
    #[error("cannot move a directory into itself")]
    MoveIntoItself,
    #[error("read range is out of bounds")]
    RangeOutOfBounds,
    #[error("this view is unavailable while offline: {0}")]
    Offline(String),
    #[error("storage error: {0}")]
    Storage(String),
}

impl From<rusqlite::Error> for VfsError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<std::io::Error> for VfsError {
    fn from(error: std::io::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<anyhow::Error> for VfsError {
    fn from(error: anyhow::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

/// VFS 核心接口。
///
/// 方法都是同步的：底层是单个 SQLite 连接，工具层用 `spawn_blocking` 包一层即可。
/// 全进程只有一个 agent，按节点加锁的收益不及“单连接串行”带来的确定性，
/// 因此写路径统一由存储层的互斥锁串行化（见 `sqlite::VfsStore`）。
pub trait Vfs: Send + Sync {
    /// 读取元信息。
    fn stat(&self, path: &VfsPath) -> Result<NodeMeta, VfsError>;

    /// 列出目录内容；可按名称排序。
    fn list(&self, path: &VfsPath) -> Result<Vec<NodeMeta>, VfsError>;

    /// 读取文本（或二进制的元信息）。
    fn read(
        &self,
        path: &VfsPath,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<FileContent, VfsError>;

    /// 读取二进制内容（图片等）；默认实现不支持。
    fn read_bytes(&self, _path: &VfsPath) -> Result<Vec<u8>, VfsError> {
        Err(VfsError::Storage(
            "this VFS backend does not expose binary reads".to_owned(),
        ))
    }

    /// 写入文本；已存在的节点要求先全文读取。
    fn write_text(
        &self,
        path: &VfsPath,
        content: &str,
        origin_chat: Option<&str>,
    ) -> Result<NodeMeta, VfsError>;

    /// 精确替换文本。
    fn edit_text(
        &self,
        path: &VfsPath,
        old_text: &str,
        new_text: &str,
        replace_all: bool,
    ) -> Result<NodeMeta, VfsError>;

    /// 写入二进制；按 1 MiB 分块加密。
    fn write_binary(
        &self,
        path: &VfsPath,
        data: &[u8],
        kind: NodeKind,
    ) -> Result<NodeMeta, VfsError>;

    /// 递归创建目录；已存在视为成功。
    fn mkdir_all(&self, path: &VfsPath) -> Result<(), VfsError>;

    /// 删除；默认移入回收站，`permanent` 或已在回收站内则彻底删除。
    fn remove(&self, path: &VfsPath, permanent: bool) -> Result<RemoveOutcome, VfsError>;

    /// 移动 / 重命名。
    fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<(), VfsError>;

    /// 复制（文件或目录递归）。
    fn copy(&self, from: &VfsPath, to: &VfsPath) -> Result<(), VfsError>;

    /// 用量统计。
    fn usage(&self) -> Result<Usage, VfsError>;

    /// 清理超过保留期或超出配额的回收站，返回删除的节点数。
    fn purge_trash(&self) -> Result<u64, VfsError>;
}

/// 由扩展名推断 MIME；未知时返回 `None`。
pub fn guess_mime(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "md" | "markdown" => "text/markdown",
        "txt" | "log" => "text/plain",
        "json" | "jsonl" => "application/json",
        "toml" => "application/toml",
        "yaml" | "yml" => "application/yaml",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "xml" => "application/xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        _ => return None,
    })
}

/// 由扩展名判断节点类型。
pub fn kind_for_name(name: &str) -> NodeKind {
    match guess_mime(name) {
        Some(mime) if mime.starts_with("image/") => NodeKind::Image,
        Some(mime) if mime.starts_with("audio/") => NodeKind::Audio,
        Some(mime) if mime.starts_with("video/") => NodeKind::Video,
        Some(mime)
            if mime.starts_with("text/") || mime.contains("json") || mime.contains("toml") =>
        {
            NodeKind::Text
        }
        Some(_) => NodeKind::Binary,
        None => NodeKind::File,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_and_kind_detection() {
        assert_eq!(guess_mime("notes.md"), Some("text/markdown"));
        assert_eq!(kind_for_name("notes.md"), NodeKind::Text);
        assert_eq!(kind_for_name("photo.PNG"), NodeKind::Image);
        assert_eq!(kind_for_name("clip.mp4"), NodeKind::Video);
        assert_eq!(kind_for_name("voice.mp3"), NodeKind::Audio);
        assert_eq!(kind_for_name("data.bin"), NodeKind::File);
        assert_eq!(kind_for_name("noext"), NodeKind::File);
    }

    #[test]
    fn kind_strings_roundtrip() {
        for kind in [
            NodeKind::Dir,
            NodeKind::Text,
            NodeKind::Binary,
            NodeKind::Image,
            NodeKind::Audio,
            NodeKind::Video,
            NodeKind::File,
        ] {
            assert_eq!(NodeKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(NodeKind::parse("unknown"), None);
    }
}
