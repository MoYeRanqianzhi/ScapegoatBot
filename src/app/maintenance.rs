//! 后台维护任务：与消息无关、按固定间隔运行的整理工作。
//!
//! 每个任务都在 `spawn_blocking` 里跑（它们只做同步的 SQLite / 文件操作），
//! 失败只记录日志，下一次照常运行；任务本身从不退出。

use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use crate::config::SkillsConfig;
use crate::storage::Database;
use crate::vfs::Vfs;
use crate::vfs::sqlite::VfsStore;

/// 技能 curator 的检查间隔：只在积累了足够多新技能时才真正整理。
const CURATOR_INTERVAL: Duration = Duration::from_secs(3600);
/// 回收站清理间隔；启动时先清理一次。
const TRASH_PURGE_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// 技能 curator：每小时检查一次，只归档不删除（见 `tools::skill::curate`）。
pub(super) fn spawn_skill_curator(vfs: Arc<VfsStore>, db: Arc<Database>, config: SkillsConfig) {
    tokio::spawn(async move {
        loop {
            let (vfs, db, config) = (Arc::clone(&vfs), Arc::clone(&db), config.clone());
            match tokio::task::spawn_blocking(move || {
                crate::tools::skill::curate(&vfs, &db, &config)
            })
            .await
            {
                Ok(Ok(report)) if !report.archived.is_empty() => {
                    info!(archived = ?report.archived, "skill curator archived unused skills");
                }
                Ok(Ok(_)) => {}
                Ok(Err(error)) => warn!(%error, "skill curator failed"),
                Err(error) => warn!(%error, "skill curator task failed"),
            }
            tokio::time::sleep(CURATOR_INTERVAL).await;
        }
    });
}

/// 回收站清理：清除超过保留期的条目，并把回收站压回字节/节点上限之内。
///
/// 移入回收站时已经在同一事务里执行上限；这里补上“时间到了”的那一条——
/// 没有新的删除时，过期条目只能靠定期清理离开。
pub(super) fn spawn_trash_purge(vfs: Arc<VfsStore>) {
    tokio::spawn(async move {
        loop {
            let store = Arc::clone(&vfs);
            match tokio::task::spawn_blocking(move || store.purge_trash()).await {
                Ok(Ok(0)) => {}
                Ok(Ok(removed)) => info!(removed, "purged expired entries from /trash"),
                Ok(Err(error)) => warn!(%error, "cannot purge /trash"),
                Err(error) => warn!(%error, "the trash purge task failed"),
            }
            tokio::time::sleep(TRASH_PURGE_INTERVAL).await;
        }
    });
}
