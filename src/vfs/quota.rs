//! 配额与磁盘水位。
//!
//! 设计依据：设计 §6.4「单文件 512 MB、VFS 总量 10 GB / 100000 节点、写入前检查磁盘剩余 ≥ 1 GB」。
//!
//! 三条独立限制：
//! - 单文件：文本 1 MB，二进制 `max_blob_bytes`；
//! - 总量：`max_vfs_bytes`（0 = 不限制）与节点数 100000；
//! - 磁盘：写入前要求至少 1 GB 可用，避免把宿主磁盘写满。

use std::path::Path;

use crate::config::StorageConfig;

use super::VfsError;

/// 默认文本上限：1 MiB。
pub const DEFAULT_MAX_TEXT_BYTES: u64 = 1024 * 1024;
/// 默认节点上限：100,000。
pub const DEFAULT_MAX_NODES: u64 = 100_000;
/// 写入前要求的最小磁盘剩余：1 GiB。
pub const DEFAULT_MIN_FREE_BYTES: u64 = 1024 * 1024 * 1024;

/// 配额参数。
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_text_bytes: u64,
    pub max_blob_bytes: u64,
    pub max_total_bytes: u64,
    pub max_nodes: u64,
    pub min_free_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_text_bytes: DEFAULT_MAX_TEXT_BYTES,
            max_blob_bytes: 512 * 1024 * 1024,
            max_total_bytes: 10 * 1024 * 1024 * 1024,
            max_nodes: DEFAULT_MAX_NODES,
            min_free_bytes: DEFAULT_MIN_FREE_BYTES,
        }
    }
}

impl Limits {
    /// 从 `[storage]` 读取；0 表示不限制。
    pub fn from_config(config: &StorageConfig) -> Self {
        Self {
            max_text_bytes: DEFAULT_MAX_TEXT_BYTES,
            max_blob_bytes: config.max_blob_bytes,
            max_total_bytes: config.max_vfs_bytes,
            max_nodes: DEFAULT_MAX_NODES,
            min_free_bytes: DEFAULT_MIN_FREE_BYTES,
        }
    }
}

/// 一次写入前的配额检查。
#[derive(Debug, Clone, Copy)]
pub struct Quota {
    limits: Limits,
}

impl Quota {
    pub fn new(limits: Limits) -> Self {
        Self { limits }
    }

    /// 总量上限（0 = 不限制）；回收站比例清理需要它。
    pub fn max_total_bytes(&self) -> u64 {
        self.limits.max_total_bytes
    }

    /// 文本写入检查。
    pub fn check_text(&self, len: u64) -> Result<(), VfsError> {
        if self.limits.max_text_bytes != 0 && len > self.limits.max_text_bytes {
            return Err(VfsError::TextTooLarge {
                limit: self.limits.max_text_bytes,
            });
        }
        Ok(())
    }

    /// 二进制写入检查。
    pub fn check_blob(&self, len: u64) -> Result<(), VfsError> {
        if self.limits.max_blob_bytes != 0 && len > self.limits.max_blob_bytes {
            return Err(VfsError::BlobTooLarge {
                limit: self.limits.max_blob_bytes,
            });
        }
        Ok(())
    }

    /// 总量与节点数检查。
    ///
    /// `current_bytes` / `current_nodes` 是活跃用量（不含回收站）；
    /// `add_bytes` / `add_nodes` 是本次写入的增量；覆盖写时调用方传增量差额。
    pub fn check_growth(
        &self,
        current_bytes: u64,
        current_nodes: u64,
        add_bytes: u64,
        add_nodes: u64,
    ) -> Result<(), VfsError> {
        if self.limits.max_total_bytes != 0 {
            let usage = current_bytes.saturating_add(add_bytes);
            if usage > self.limits.max_total_bytes {
                return Err(VfsError::QuotaExceeded {
                    usage,
                    limit: self.limits.max_total_bytes,
                });
            }
        }
        if self.limits.max_nodes != 0 {
            let nodes = current_nodes.saturating_add(add_nodes);
            if nodes > self.limits.max_nodes {
                return Err(VfsError::NodeLimitExceeded {
                    limit: self.limits.max_nodes,
                });
            }
        }
        Ok(())
    }

    /// 磁盘剩余检查。
    ///
    /// 平台不支持查询时放行（不能因为查不到空间就拒绝写入）；
    /// 真实失败会在写入时以 I/O 错误暴露。
    pub fn check_disk(&self, storage_path: &Path) -> Result<(), VfsError> {
        if self.limits.min_free_bytes == 0 {
            return Ok(());
        }
        match fs2::available_space(storage_path) {
            Ok(free) if free < self.limits.min_free_bytes => Err(VfsError::DiskFull {
                need: self.limits.min_free_bytes,
            }),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quota() -> Quota {
        Quota::new(Limits {
            max_text_bytes: 10,
            max_blob_bytes: 100,
            max_total_bytes: 1000,
            max_nodes: 3,
            min_free_bytes: 0,
        })
    }

    #[test]
    fn text_and_blob_limits() {
        assert!(quota().check_text(10).is_ok());
        assert!(quota().check_text(11).is_err());
        assert!(quota().check_blob(100).is_ok());
        assert!(quota().check_blob(101).is_err());
    }

    #[test]
    fn total_and_node_limits() {
        assert!(quota().check_growth(900, 1, 100, 0).is_ok());
        assert!(quota().check_growth(901, 1, 100, 0).is_err());
        assert!(quota().check_growth(0, 2, 0, 1).is_ok());
        assert!(quota().check_growth(0, 3, 0, 1).is_err());
    }

    #[test]
    fn unlimited_means_no_check() {
        let unlimited = Quota::new(Limits {
            max_text_bytes: 0,
            max_blob_bytes: 0,
            max_total_bytes: 0,
            max_nodes: 0,
            min_free_bytes: 0,
        });
        assert!(unlimited.check_text(u64::MAX).is_ok());
        assert!(unlimited.check_blob(u64::MAX).is_ok());
        assert!(
            unlimited
                .check_growth(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
                .is_ok()
        );
    }
}
