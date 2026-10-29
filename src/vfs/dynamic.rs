//! 动态视图接口。
//!
//! 设计依据：设计 §6.9。
//!
//! `/friends`、`/groups`、`/messages`、`/sessions`、`/inbox` 的一部分内容不存 VFS，
//! 而是由运行时实时生成（NapCat 查询、JSONL 渲染）。本模块只定义提供者契约：
//! - 存储层先查本地节点（例如 `/friends/<qq>/notes.md` 是可写的）；
//! - 本地没有时才询问动态提供者；
//! - 提供者不可用（离线/API 失败）时返回 [`VfsError::Offline`]，而不是空内容。
//!
//! Phase 3 会在这里接入 NapCat 与消息存储的实现；Phase 1 先把契约固定下来。

use super::{NodeKind, VfsError, path::VfsPath};

/// 动态条目（目录列表用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicEntry {
    pub name: String,
    pub kind: NodeKind,
    pub size: u64,
}

/// 动态文件内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicFile {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
}

/// 动态视图提供者。
///
/// 返回 `Ok(None)` 表示“这个路径不归我管”，由下一个提供者或存储层处理；
/// 返回 `Err(Offline)` 表示“归我管但现在取不到”。
pub trait DynamicProvider: Send + Sync {
    /// 提供者名称，用于日志与 `/system`。
    fn name(&self) -> &'static str;

    /// 列出目录；不负责该路径时返回 `Ok(None)`。
    fn list(&self, path: &VfsPath) -> Result<Option<Vec<DynamicEntry>>, VfsError>;

    /// 读取文件；不负责该路径时返回 `Ok(None)`。
    fn read(&self, path: &VfsPath) -> Result<Option<DynamicFile>, VfsError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 契约测试：提供者只负责自己的前缀，其余返回 None。
    struct FakeFriends;

    impl DynamicProvider for FakeFriends {
        fn name(&self) -> &'static str {
            "fake-friends"
        }

        fn list(&self, path: &VfsPath) -> Result<Option<Vec<DynamicEntry>>, VfsError> {
            if path.to_string() != "/friends" {
                return Ok(None);
            }
            Ok(Some(vec![DynamicEntry {
                name: "10001".to_owned(),
                kind: NodeKind::Dir,
                size: 0,
            }]))
        }

        fn read(&self, path: &VfsPath) -> Result<Option<DynamicFile>, VfsError> {
            if path.to_string() == "/friends/10001/info.json" {
                return Ok(Some(DynamicFile {
                    bytes: br#"{"qq":10001}"#.to_vec(),
                    mime: "application/json",
                }));
            }
            Ok(None)
        }
    }

    #[test]
    fn provider_owns_only_its_prefix() {
        let provider = FakeFriends;
        let friends = VfsPath::parse("/friends").unwrap();
        let other = VfsPath::parse("/groups").unwrap();
        let info = VfsPath::parse("/friends/10001/info.json").unwrap();

        assert_eq!(provider.list(&friends).unwrap().unwrap().len(), 1);
        assert!(provider.list(&other).unwrap().is_none());
        assert!(provider.read(&info).unwrap().is_some());
    }
}
