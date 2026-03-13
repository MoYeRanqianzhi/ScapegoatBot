// src/storage/images.rs
// 消息图片注册表（SQLite 持久化）
// 接收消息时提取 MessageSegment::Image 中的 URL 并注册到数据库
// ReadImage 工具通过 message_id + index 安全查找，避免 LLM 注入任意 URL
//
// 安全设计：
// - URL 仅来自已接收消息的 MessageSegment::Image 字段
// - LLM 只能提供 message_id 和 index，无法直接指定 URL
// - 数据库为 SQLite 文件，存储在 workspace/data/ 目录下
// - 即使 LLM 通过 Read 工具尝试读取 .db 文件，也会因非 UTF-8 而失败

use anyhow::Context as _;
use std::path::Path;
use std::sync::Mutex;

// ============================================================================
// 图片注册表
// ============================================================================

/// 消息图片注册表
/// 使用 SQLite 持久化存储 message_id → image_url 的映射关系
/// 线程安全：通过 Mutex 保护 Connection（rusqlite::Connection 是 Send 但非 Sync）
pub struct ImageStore {
    /// SQLite 数据库连接（Mutex 保证线程安全）
    conn: Mutex<rusqlite::Connection>,
}

impl ImageStore {
    /// 打开或创建图片注册表数据库
    /// 自动创建父目录和表结构
    ///
    /// # 参数
    /// - `db_path`: 数据库文件路径（如 `workspace/data/images.db`）
    pub fn open(db_path: &Path) -> anyhow::Result<Self> {
        // 确保父目录存在
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("创建图片数据库目录失败: {}", parent.display())
            })?;
        }

        // 打开 SQLite 数据库（不存在则自动创建）
        let conn = rusqlite::Connection::open(db_path).with_context(|| {
            format!("打开图片数据库失败: {}", db_path.display())
        })?;

        // 启用 WAL 模式（提高并发读写性能）
        conn.pragma_update(None, "journal_mode", "WAL")?;

        // 创建表结构（如果不存在）
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS message_images (
                message_id  INTEGER NOT NULL,
                image_index INTEGER NOT NULL,
                url         TEXT    NOT NULL,
                registered_at TEXT NOT NULL DEFAULT (datetime('now', 'localtime')),
                PRIMARY KEY (message_id, image_index)
            );"
        ).with_context(|| "创建 message_images 表失败")?;

        tracing::info!("图片注册表已打开: {}", db_path.display());

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 注册消息中的图片 URL
    /// 接收消息时调用，从 MessageSegment::Image 中提取的 URL 逐条插入
    /// 已存在的记录会被覆盖（INSERT OR REPLACE）
    ///
    /// # 参数
    /// - `message_id`: 消息 ID（来自 OneBot 协议）
    /// - `urls`: 该消息包含的所有图片 URL 列表（按出现顺序）
    pub fn register(&self, message_id: i64, urls: &[String]) {
        if urls.is_empty() {
            return;
        }

        let Ok(conn) = self.conn.lock() else {
            tracing::warn!("图片注册表锁获取失败 (message_id={})", message_id);
            return;
        };

        for (i, url) in urls.iter().enumerate() {
            if let Err(e) = conn.execute(
                "INSERT OR REPLACE INTO message_images (message_id, image_index, url) VALUES (?1, ?2, ?3)",
                rusqlite::params![message_id, i as i64, url],
            ) {
                tracing::warn!(
                    "注册图片失败 (message_id={}, index={}): {}",
                    message_id, i, e
                );
            }
        }
    }

    /// 查找消息中指定索引的图片 URL
    /// ReadImage 工具调用此方法获取已验证的图片 URL
    ///
    /// # 返回
    /// - `Some(url)`: 找到对应的图片 URL
    /// - `None`: 消息不存在或索引越界
    pub fn get_url(&self, message_id: i64, index: usize) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT url FROM message_images WHERE message_id = ?1 AND image_index = ?2",
            rusqlite::params![message_id, index as i64],
            |row| row.get(0),
        )
        .ok()
    }

    /// 获取消息中的图片数量
    /// 用于 ReadImage 工具在索引越界时给出准确的错误提示
    pub fn get_count(&self, message_id: i64) -> usize {
        let Ok(conn) = self.conn.lock() else {
            return 0;
        };
        conn.query_row(
            "SELECT COUNT(*) FROM message_images WHERE message_id = ?1",
            rusqlite::params![message_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize
    }
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 辅助函数：创建临时数据库
    fn temp_store() -> (ImageStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_images.db");
        let store = ImageStore::open(&db_path).unwrap();
        (store, dir)
    }

    /// 测试：注册和查找图片
    #[test]
    fn register_and_lookup() {
        let (store, _dir) = temp_store();

        // 注册消息 100 的两张图片
        store.register(100, &[
            "https://example.com/img1.jpg".to_string(),
            "https://example.com/img2.png".to_string(),
        ]);

        // 查找第一张
        assert_eq!(
            store.get_url(100, 0),
            Some("https://example.com/img1.jpg".to_string())
        );
        // 查找第二张
        assert_eq!(
            store.get_url(100, 1),
            Some("https://example.com/img2.png".to_string())
        );
        // 索引越界
        assert_eq!(store.get_url(100, 2), None);
        // 不存在的消息
        assert_eq!(store.get_url(999, 0), None);
    }

    /// 测试：图片计数
    #[test]
    fn image_count() {
        let (store, _dir) = temp_store();

        assert_eq!(store.get_count(100), 0);

        store.register(100, &[
            "https://example.com/a.jpg".to_string(),
            "https://example.com/b.jpg".to_string(),
            "https://example.com/c.jpg".to_string(),
        ]);

        assert_eq!(store.get_count(100), 3);
        assert_eq!(store.get_count(999), 0);
    }

    /// 测试：重复注册覆盖旧值
    #[test]
    fn register_overwrites() {
        let (store, _dir) = temp_store();

        store.register(100, &["https://old.com/img.jpg".to_string()]);
        assert_eq!(
            store.get_url(100, 0),
            Some("https://old.com/img.jpg".to_string())
        );

        // 重新注册覆盖
        store.register(100, &["https://new.com/img.jpg".to_string()]);
        assert_eq!(
            store.get_url(100, 0),
            Some("https://new.com/img.jpg".to_string())
        );
    }

    /// 测试：空 URL 列表不会写入
    #[test]
    fn register_empty_is_noop() {
        let (store, _dir) = temp_store();
        store.register(100, &[]);
        assert_eq!(store.get_count(100), 0);
    }

    /// 测试：数据库持久化（关闭后重新打开）
    #[test]
    fn persistence_across_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("persist_test.db");

        // 第一次打开，写入数据
        {
            let store = ImageStore::open(&db_path).unwrap();
            store.register(42, &["https://example.com/persistent.jpg".to_string()]);
        }

        // 第二次打开，数据仍在
        {
            let store = ImageStore::open(&db_path).unwrap();
            assert_eq!(
                store.get_url(42, 0),
                Some("https://example.com/persistent.jpg".to_string())
            );
            assert_eq!(store.get_count(42), 1);
        }
    }
}
