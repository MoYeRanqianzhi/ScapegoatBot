//! SQLite 存储：连接、迁移、键值元数据、幂等表。
//!
//! 设计依据：设计 §2.2（IPC 幂等）、§6.4（节点与存储）、§8。
//!
//! Phase 0 只建立底座：连接与 PRAGMA、迁移框架、`meta`（含密钥 canary）、
//! `control_inputs`（控制通道输入）与 `idempotency`（请求去重）。
//! VFS、消息索引、发送账本等表在各自阶段追加迁移，**只增不改**。

pub mod crypto;
pub mod messages;

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use tracing::{debug, warn};

use crypto::MasterKey;

/// 密钥 canary 的用途名；改动会导致已有数据库无法打开，必须与迁移版本一同升级。
const CANARY_PURPOSE: &str = "canary";
/// canary 的明文常量。
const CANARY_PLAINTEXT: &[u8] = b"scapegoat-bot/canary/v1";
/// canary 在 `meta` 表中的键名。
const CANARY_KEY: &str = "encryption.canary";

/// 迁移列表：`(版本号, 说明, SQL)`。只追加，不修改已发布条目。
const MIGRATIONS: &[(u32, &str, &str)] = &[
    (
        1,
        "phase0: meta, control_inputs, idempotency",
        r#"
    CREATE TABLE meta (
        key   TEXT PRIMARY KEY,
        value BLOB NOT NULL
    );

    -- 控制通道输入：CLI `chat` 持久化后作为 owner_context 注入（设计 §2.3）。
    CREATE TABLE control_inputs (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        text_enc   BLOB NOT NULL,
        actor      TEXT NOT NULL,
        created_at TEXT NOT NULL
    );

    -- IPC 幂等表：Chat / SetModel / SetEffort 带 idempotency_key，短期内重放返回原结果。
    CREATE TABLE idempotency (
        key        TEXT PRIMARY KEY,
        response   TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    "#,
    ),
    (
        2,
        "phase1: VFS nodes, chunks, read state",
        r#"
    -- 虚拟文件系统。`name` 是明文路径段（目录索引需要），内容单独加密。
    CREATE TABLE vfs_nodes (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        parent_id    INTEGER REFERENCES vfs_nodes(id) ON DELETE CASCADE,
        name         TEXT NOT NULL,
        kind         TEXT NOT NULL,
        content_enc  BLOB,
        size         INTEGER NOT NULL DEFAULT 0,
        mime         TEXT,
        mode         INTEGER NOT NULL DEFAULT 420,
        version      INTEGER NOT NULL DEFAULT 1,
        origin_chat  TEXT,
        created_at   TEXT NOT NULL,
        updated_at   TEXT NOT NULL,
        hash         TEXT,
        trashed_at   TEXT,
        trash_origin TEXT
    );

    -- 同一目录下不重名；回收站内的节点不参与唯一约束（可以同名重建）。
    CREATE UNIQUE INDEX vfs_nodes_unique_name
        ON vfs_nodes(parent_id, name)
        WHERE trashed_at IS NULL;
    CREATE INDEX vfs_nodes_parent ON vfs_nodes(parent_id);
    CREATE INDEX vfs_nodes_trashed ON vfs_nodes(trashed_at);

    -- 二进制分块索引；密文在 blobs/<node_id>/<idx>，不存 SQLite。
    CREATE TABLE vfs_chunks (
        node_id INTEGER NOT NULL REFERENCES vfs_nodes(id) ON DELETE CASCADE,
        idx     INTEGER NOT NULL,
        offset  INTEGER NOT NULL,
        length  INTEGER NOT NULL,
        hash    TEXT NOT NULL,
        PRIMARY KEY (node_id, idx)
    );

    -- Read-before-Edit：全局单 agent，无会话维度。
    CREATE TABLE vfs_reads (
        node_id      INTEGER PRIMARY KEY REFERENCES vfs_nodes(id) ON DELETE CASCADE,
        version      INTEGER NOT NULL,
        content_hash TEXT,
        is_partial   INTEGER NOT NULL,
        read_at      TEXT NOT NULL
    );

    INSERT INTO vfs_nodes (id, parent_id, name, kind, size, version, created_at, updated_at)
    VALUES (1, NULL, '', 'dir', 0, 1, datetime('now'), datetime('now'));
    "#,
    ),
    (
        3,
        "phase2: security events",
        r#"
    -- 每次输入闸/输出闸判定都记一条；shadow 期用于校准阈值。
    CREATE TABLE security_events (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        checkpoint TEXT NOT NULL,
        allowed    INTEGER NOT NULL,
        score      REAL NOT NULL,
        backend    TEXT NOT NULL,
        reason     TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    CREATE INDEX security_events_created ON security_events(created_at);
    "#,
    ),
    (
        4,
        "phase3: message index, inbox state, cursors, outbound ledger",
        r#"
    -- JSONL 是事实来源，本表是可重建索引（设计 §8.2）。
    CREATE TABLE message_files (
        path       TEXT PRIMARY KEY,
        scope      TEXT NOT NULL,
        peer       INTEGER NOT NULL,
        month      TEXT NOT NULL,
        day        TEXT NOT NULL,
        count      INTEGER NOT NULL DEFAULT 0,
        bytes      INTEGER NOT NULL DEFAULT 0,
        updated_at TEXT NOT NULL
    );
    CREATE INDEX message_files_month ON message_files(scope, peer, month);

    CREATE TABLE messages (
        chat        TEXT NOT NULL,
        message_id  INTEGER NOT NULL,
        sender      TEXT NOT NULL,
        ts          TEXT NOT NULL,
        day         TEXT NOT NULL,
        file_path   TEXT NOT NULL,
        line_no     INTEGER NOT NULL,
        reply_to    INTEGER,
        is_self     INTEGER NOT NULL,
        backfilled  INTEGER NOT NULL DEFAULT 0,
        recalled    INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (chat, message_id)
    );
    CREATE INDEX messages_chat_ts ON messages(chat, ts);
    CREATE INDEX messages_day ON messages(day);

    -- 收件箱状态：unread / read / dismissed。
    CREATE TABLE inbox_state (
        chat       TEXT NOT NULL,
        message_id INTEGER NOT NULL,
        state      TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        PRIMARY KEY (chat, message_id)
    );
    CREATE INDEX inbox_state_state ON inbox_state(state);

    -- 断线补消息的双轨游标。
    CREATE TABLE chat_cursors (
        chat            TEXT PRIMARY KEY,
        last_message_id INTEGER NOT NULL,
        last_ts         TEXT NOT NULL,
        updated_at      TEXT NOT NULL
    );

    -- 发送账本：先登记再发送，未确认的发送重启后先查账本与历史。
    CREATE TABLE outbound_intents (
        id         TEXT PRIMARY KEY,
        chat       TEXT NOT NULL,
        intent     TEXT NOT NULL,
        status     TEXT NOT NULL,
        message_id INTEGER,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE INDEX outbound_intents_chat ON outbound_intents(chat, status);
    "#,
    ),
    (
        5,
        "phase3: transcript segments, turn buffer, checkpoints",
        r#"
    -- transcript 分段索引；事实来源是 sessions/*.jsonl，本表可重建。
    CREATE TABLE transcript_files (
        file          TEXT PRIMARY KEY,
        file_id       TEXT NOT NULL,
        prev_file     TEXT,
        base_seq      INTEGER NOT NULL DEFAULT 0,
        started_at    TEXT NOT NULL,
        ended_at      TEXT,
        message_count INTEGER NOT NULL DEFAULT 0,
        bytes         INTEGER NOT NULL DEFAULT 0,
        pruned_at     TEXT
    );
    CREATE INDEX transcript_files_started ON transcript_files(started_at);

    -- 回合缓冲：assistant/tool 块在回合结束时才提交到分段。
    CREATE TABLE turn_buffer (
        turn_id TEXT NOT NULL,
        seq     INTEGER NOT NULL,
        line    TEXT NOT NULL,
        state   TEXT NOT NULL,
        PRIMARY KEY (turn_id, seq)
    );

    -- 压缩检查点；Phase 5 写入。
    CREATE TABLE compaction_checkpoints (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        from_file  TEXT NOT NULL,
        summary_enc BLOB NOT NULL,
        created_at TEXT NOT NULL
    );
    "#,
    ),
    (
        6,
        "memory entries",
        // 记忆的可查询索引：与 `/MEMORY.md` 和 `/memory/*.md` 三方对账。
        // 文件仍是事实来源，本表损坏时可从文件重建。
        r#"
    CREATE TABLE memory_entries (
        slug        TEXT PRIMARY KEY,
        path        TEXT NOT NULL,
        kind        TEXT NOT NULL,
        source      TEXT NOT NULL,
        observed_at TEXT NOT NULL,
        updated_at  TEXT NOT NULL
    );
    CREATE INDEX memory_entries_kind ON memory_entries(kind);
    "#,
    ),
    (
        7,
        "skill usage",
        // 技能使用记录；curator 只归档长期未用技能，不删除。
        r#"
    CREATE TABLE skill_usage (
        name      TEXT PRIMARY KEY,
        last_used TEXT NOT NULL,
        uses      INTEGER NOT NULL
    );
    "#,
    ),
];

/// 数据库句柄。
///
/// 用 `Mutex<Connection>` 而不是连接池：单实例、低频写入，锁竞争可以忽略；
/// 长事务（如百万级索引重建）在各自阶段用独立连接处理。
pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    /// 打开（或创建）数据库并应用迁移。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open database {}", path.display()))?;
        Self::from_connection(conn)
    }

    /// 内存数据库，供单元测试使用。
    #[allow(dead_code)]
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // WAL + NORMAL：单进程写入下可靠性足够，避免每次提交都 fsync 拖慢消息落库。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        let db = Self {
            conn: Mutex::new(conn),
        };
        db.apply_migrations()?;
        Ok(db)
    }

    /// 顺序应用未执行的迁移。
    fn apply_migrations(&self) -> Result<()> {
        let mut conn = self.conn.lock().expect("database mutex poisoned");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 version    INTEGER PRIMARY KEY,
                 applied_at TEXT NOT NULL
             );",
        )?;

        let applied: u32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        for (version, description, sql) in MIGRATIONS {
            if *version <= applied {
                continue;
            }
            debug!(version, description, "applying database migration");
            let tx = conn.transaction()?;
            tx.execute_batch(sql)
                .with_context(|| format!("migration {version} ({description}) failed"))?;
            tx.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                params![version, now_rfc3339()],
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    /// 当前迁移版本，供 `sbot doctor` 与测试使用。
    pub fn schema_version(&self) -> Result<u32> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        Ok(conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?)
    }

    /// 读取元数据。
    pub fn meta_get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        Ok(conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// 写入元数据（覆盖）。
    pub fn meta_set(&self, key: &str, value: &[u8]) -> Result<()> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// 校验密钥与数据库匹配；首次打开时写入 canary。
    ///
    /// 密钥换错或数据被换过时，AEAD 解密失败，直接拒绝启动——
    /// 这比“打开后读到乱码再报错”更早、更明确。
    pub fn ensure_key(&self, key: &MasterKey) -> Result<()> {
        match self.meta_get(CANARY_KEY)? {
            None => {
                let sealed =
                    key.seal(CANARY_PURPOSE, CANARY_PURPOSE.as_bytes(), CANARY_PLAINTEXT)?;
                self.meta_set(CANARY_KEY, &sealed)?;
                debug!("encryption canary written");
                Ok(())
            }
            Some(sealed) => {
                let opened = key
                    .open(CANARY_PURPOSE, CANARY_PURPOSE.as_bytes(), &sealed)
                    .context("database was created with a different master key")?;
                if opened != CANARY_PLAINTEXT {
                    bail!("encryption canary does not match; refusing to open the database");
                }
                Ok(())
            }
        }
    }

    /// 查询幂等结果。
    pub fn idempotency_get(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        Ok(conn
            .query_row(
                "SELECT response FROM idempotency WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// 记录幂等结果。
    pub fn idempotency_put(&self, key: &str, response: &str) -> Result<()> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        conn.execute(
            "INSERT INTO idempotency (key, response, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO NOTHING",
            params![key, response, now_rfc3339()],
        )?;
        Ok(())
    }

    /// 清理过期幂等记录（默认 5 分钟）；Phase 3 的周期维护任务调用。
    #[allow(dead_code)]
    pub fn idempotency_prune(&self, older_than: chrono::Duration) -> Result<usize> {
        let cutoff = (chrono::Utc::now() - older_than).to_rfc3339();
        let conn = self.conn.lock().expect("database mutex poisoned");
        let removed = conn.execute(
            "DELETE FROM idempotency WHERE created_at < ?1",
            params![cutoff],
        )?;
        if removed > 0 {
            warn!(removed, "pruned expired idempotency records");
        }
        Ok(removed)
    }

    /// 取得连接锁。
    ///
    /// VFS 等模块需要自己控制事务边界；单连接 + 单互斥锁保证写路径串行，
    /// 这比按节点加锁更简单，也消除了写写冲突。调用方必须在同一作用域内完成事务。
    pub fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("database mutex poisoned")
    }

    /// 写入一条控制通道输入，返回自增 id。
    pub fn insert_control_input(&self, text_enc: &[u8], actor: &str) -> Result<i64> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        conn.execute(
            "INSERT INTO control_inputs (text_enc, actor, created_at) VALUES (?1, ?2, ?3)",
            params![text_enc, actor, now_rfc3339()],
        )?;
        Ok(conn.last_insert_rowid())
    }
}

/// 校验 canary（`sbot doctor` 只读路径使用）。
pub fn verify_canary(key: &MasterKey, sealed: &[u8]) -> bool {
    key.open(CANARY_PURPOSE, CANARY_PURPOSE.as_bytes(), sealed)
        .map(|plain| plain == CANARY_PLAINTEXT)
        .unwrap_or(false)
}

/// RFC3339 UTC 时间戳，所有 `created_at` 统一用这个格式。
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> MasterKey {
        MasterKey::from_bytes([3u8; 32])
    }

    #[test]
    fn migrations_apply_and_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sbot.db");

        let db = Database::open(&path).unwrap();
        let version = db.schema_version().unwrap();
        assert_eq!(version, MIGRATIONS.last().unwrap().0);
        drop(db);

        // 第二次打开不应重复执行迁移或报错。
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), version);
    }

    #[test]
    fn meta_roundtrip_and_overwrite() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.meta_get("k").unwrap(), None);
        db.meta_set("k", b"one").unwrap();
        assert_eq!(db.meta_get("k").unwrap().as_deref(), Some(&b"one"[..]));
        db.meta_set("k", b"two").unwrap();
        assert_eq!(db.meta_get("k").unwrap().as_deref(), Some(&b"two"[..]));
    }

    #[test]
    fn canary_accepts_same_key_and_rejects_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sbot.db");

        let db = Database::open(&path).unwrap();
        db.ensure_key(&key()).unwrap();
        drop(db);

        let db = Database::open(&path).unwrap();
        db.ensure_key(&key()).unwrap();

        let other = MasterKey::from_bytes([9u8; 32]);
        let error = db.ensure_key(&other).unwrap_err().to_string();
        assert!(error.contains("different master key"));
    }

    #[test]
    fn idempotency_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.idempotency_get("abc").unwrap(), None);
        db.idempotency_put("abc", "{\"ok\":true}").unwrap();
        assert_eq!(
            db.idempotency_get("abc").unwrap().as_deref(),
            Some("{\"ok\":true}")
        );
        // 重复写入不覆盖原结果。
        db.idempotency_put("abc", "{\"ok\":false}").unwrap();
        assert_eq!(
            db.idempotency_get("abc").unwrap().as_deref(),
            Some("{\"ok\":true}")
        );
    }

    #[test]
    fn control_input_gets_incrementing_ids() {
        let db = Database::open_in_memory().unwrap();
        let first = db.insert_control_input(b"x", "cli").unwrap();
        let second = db.insert_control_input(b"y", "cli").unwrap();
        assert_eq!(second, first + 1);
    }
}
