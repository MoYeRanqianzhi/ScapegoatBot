//! 存储装配：数据库、主密钥、VFS / 消息归档 / transcript / turn buffer，以及启动时的对账
//! （保留期与大小上限、发送账本、消息索引）。

use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::config::{EncryptionMode, LoadedConfig};
use crate::paths::SbotPaths;
use crate::session::SessionStore;
use crate::session::turn_buffer::TurnBufferStore;
use crate::storage::Database;
use crate::storage::crypto::MasterKey;
use crate::storage::messages::MessageStore;
use crate::vfs::sqlite::VfsStore;

use crate::app::Stores;

/// 打开数据库、密钥与三套存储，并做启动对账。
pub(in crate::app) fn open_stores(paths: &SbotPaths, loaded: &LoadedConfig) -> Result<Stores> {
    let key = Arc::new(MasterKey::load_or_create(paths.key_file())?);
    let data_dir = paths.resolve(&loaded.config.storage.path, &loaded.dir);
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("cannot create storage path {}", data_dir.display()))?;
    let db = Arc::new(Database::open(data_dir.join("data.db"))?);
    db.ensure_key(&key)?;

    if loaded.config.storage.encryption == EncryptionMode::None {
        warn!("storage.encryption = \"none\": data is stored in plaintext (debug only)");
    }

    let vfs = Arc::new(VfsStore::from_config(
        Arc::clone(&db),
        Arc::clone(&key),
        data_dir.clone(),
        &loaded.config.storage,
    )?);
    let messages = Arc::new(
        MessageStore::open(
            Arc::clone(&db),
            Arc::clone(&key),
            data_dir.clone(),
            loaded.config.storage.max_message_bytes,
            loaded.config.storage.encryption,
        )?
        .with_retention_days(loaded.config.storage.message_retention_days),
    );
    // 保留期或大小上限任一生效就在启动时清理一次；大小上限在每次写入时也会检查（`make_room`）。
    if (loaded.config.storage.message_retention_days > 0
        || loaded.config.storage.max_message_bytes != 0)
        && let Err(error) = messages.enforce_retention(loaded.config.storage.message_retention_days)
    {
        warn!(%error, "cannot enforce message retention at startup");
    }
    // 重启核对发送账本：久悬的 pending 可能已经发出，标记为 unknown。
    match messages.mark_stale_intents(600) {
        Ok(0) => {}
        Ok(stale) => warn!(
            stale,
            "marked stale outbound intents as unknown; check /messages before resending"
        ),
        Err(error) => warn!(%error, "cannot reconcile the outbound intent ledger"),
    }

    // 启动校验：索引文件数与磁盘不一致时重建（索引只是缓存）。
    match messages.index_file_counts() {
        Ok((disk, indexed)) if disk != indexed => {
            warn!(
                disk,
                indexed, "message index file count mismatch; rebuilding"
            );
            match messages.rebuild_index() {
                Ok(repaired) => info!(repaired, "message index rebuilt"),
                Err(error) => warn!(%error, "message index rebuild failed"),
            }
        }
        Ok(_) => {}
        Err(error) => warn!(%error, "cannot compare the message index with the JSONL files"),
    }
    let sessions = Arc::new(
        SessionStore::open(
            Arc::clone(&db),
            Arc::clone(&key),
            data_dir.clone(),
            loaded.config.storage.max_session_bytes,
            loaded.config.storage.encryption,
        )?
        .with_retention_days(loaded.config.storage.session_retention_days),
    );
    // 保留期或大小上限任一生效就在启动时清理一次（默认只有 2 GiB 的大小上限）。
    if sessions.limits_retention()
        && let Err(error) = sessions.enforce_retention(loaded.config.storage.session_retention_days)
    {
        warn!(%error, "cannot enforce transcript retention at startup");
    }
    let turn_buffer = Arc::new(TurnBufferStore::new(
        Arc::clone(&db),
        Arc::clone(&key),
        loaded.config.storage.encryption,
    ));

    Ok(Stores {
        db,
        key,
        vfs,
        messages,
        sessions,
        turn_buffer,
    })
}
