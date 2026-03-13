// src/storage/message_index.rs
// 消息索引数据库（SQLite 持久化）
// 建立 message_id → (file_path, line_number) 的快速映射
// 同时存储元数据（时间戳、发送者、群号、类型）以支持结构化搜索
//
// 设计目标：
// - 通过 message_id 快速定位消息在 JSONL 文件中的物理位置
// - 支持按发送者、群号、时间范围等条件高效搜索
// - 所有查询均通过 SQLite 索引加速，无需扫描 JSONL 文件
//
// 数据流：
// 1. main.rs 接收消息 → store_raw_jsonl() 返回 StoredLocation
// 2. main.rs 调用 message_index.index_message() 写入索引
// 3. GetMessage 工具调用 lookup() → 定位文件 + 行号 → 读取原始 JSON
// 4. SearchMessages 工具调用 search() → 返回匹配的消息元数据列表

use anyhow::Context as _;
use std::path::Path;
use std::sync::Mutex;

// ============================================================================
// 搜索条件与结果
// ============================================================================

/// 消息搜索条件
/// 所有字段均为可选，多个条件之间为 AND 关系
#[derive(Debug, Default)]
pub struct SearchCriteria {
    /// 按发送者 QQ 号过滤
    pub user_id: Option<i64>,
    /// 按群号过滤（NULL 表示私聊）
    pub group_id: Option<i64>,
    /// 按消息类型过滤："group" 或 "private"
    pub message_type: Option<String>,
    /// 起始时间戳（Unix 秒，包含）
    pub after: Option<i64>,
    /// 截止时间戳（Unix 秒，包含）
    pub before: Option<i64>,
    /// 最大返回条数（默认 20，上限 100）
    pub limit: u32,
    /// 分页偏移量（默认 0）
    pub offset: u32,
}

/// 消息搜索结果
/// 包含消息的元数据和在 JSONL 文件中的物理位置
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// 消息 ID（OneBot 协议中的 message_id）
    pub message_id: i64,
    /// JSONL 文件路径（相对于 messages_dir，如 "all/2026-03-10_001.jsonl"）
    pub file_path: String,
    /// 在 JSONL 文件中的行号（1-based）
    pub line_number: u32,
    /// Unix 时间戳
    pub timestamp: i64,
    /// 发送者 QQ 号
    pub user_id: i64,
    /// 群号（私聊为 None）
    pub group_id: Option<i64>,
    /// 消息类型："group" 或 "private"
    pub message_type: String,
}

// ============================================================================
// 消息索引数据库
// ============================================================================

/// 消息索引数据库
/// 使用 SQLite 持久化存储 message_id → 文件位置的映射关系
/// 线程安全：通过 Mutex 保护 Connection（rusqlite::Connection 是 Send 但非 Sync）
pub struct MessageIndex {
    /// SQLite 数据库连接（Mutex 保证线程安全）
    conn: Mutex<rusqlite::Connection>,
}

impl MessageIndex {
    /// 打开或创建消息索引数据库
    /// 自动创建父目录、表结构和索引
    ///
    /// # 参数
    /// - `db_path`: 数据库文件路径（如 `workspace/data/messages.db`）
    pub fn open(db_path: &Path) -> anyhow::Result<Self> {
        // 确保父目录存在
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("创建消息索引数据库目录失败: {}", parent.display())
            })?;
        }

        // 打开 SQLite 数据库（不存在则自动创建）
        let conn = rusqlite::Connection::open(db_path).with_context(|| {
            format!("打开消息索引数据库失败: {}", db_path.display())
        })?;

        // 启用 WAL 模式（提高并发读写性能）
        conn.pragma_update(None, "journal_mode", "WAL")?;

        // 创建表结构和索引（如果不存在）
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS message_index (
                message_id   INTEGER PRIMARY KEY,
                file_path    TEXT    NOT NULL,
                line_number  INTEGER NOT NULL,
                timestamp    INTEGER NOT NULL,
                user_id      INTEGER NOT NULL,
                group_id     INTEGER,
                message_type TEXT    NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_msg_timestamp ON message_index(timestamp);
            CREATE INDEX IF NOT EXISTS idx_msg_user_id ON message_index(user_id);
            CREATE INDEX IF NOT EXISTS idx_msg_group_id ON message_index(group_id);
            CREATE INDEX IF NOT EXISTS idx_msg_type ON message_index(message_type);",
        )
        .with_context(|| "创建 message_index 表和索引失败")?;

        tracing::info!("消息索引数据库已打开: {}", db_path.display());

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 写入一条消息的索引记录
    /// 在 store_raw_jsonl() 成功后调用，记录消息在 JSONL 文件中的物理位置
    /// 已存在的记录会被覆盖（INSERT OR REPLACE）
    ///
    /// # 参数
    /// - `msg_id`: 消息 ID（来自 OneBot 协议）
    /// - `file_path`: JSONL 文件路径（相对于 messages_dir）
    /// - `line_number`: 在文件中的行号（1-based）
    /// - `timestamp`: Unix 时间戳
    /// - `user_id`: 发送者 QQ 号
    /// - `group_id`: 群号（私聊传 None）
    /// - `msg_type`: 消息类型（"group" 或 "private"）
    pub fn index_message(
        &self,
        msg_id: i64,
        file_path: &str,
        line_number: u32,
        timestamp: i64,
        user_id: i64,
        group_id: Option<i64>,
        msg_type: &str,
    ) {
        let Ok(conn) = self.conn.lock() else {
            tracing::warn!("消息索引锁获取失败 (message_id={})", msg_id);
            return;
        };

        if let Err(e) = conn.execute(
            "INSERT OR REPLACE INTO message_index
                (message_id, file_path, line_number, timestamp, user_id, group_id, message_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![msg_id, file_path, line_number, timestamp, user_id, group_id, msg_type],
        ) {
            tracing::warn!(
                "写入消息索引失败 (message_id={}, file={}:{}): {}",
                msg_id, file_path, line_number, e
            );
        }
    }

    /// 通过消息 ID 快速查找其在 JSONL 文件中的物理位置
    /// GetMessage 工具调用此方法定位原始 JSON 数据
    ///
    /// # 返回
    /// - `Some((file_path, line_number))`: 找到索引记录
    /// - `None`: 消息 ID 不在索引中
    pub fn lookup(&self, msg_id: i64) -> Option<(String, u32)> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT file_path, line_number FROM message_index WHERE message_id = ?1",
            rusqlite::params![msg_id],
            |row| {
                let file_path: String = row.get(0)?;
                let line_number: u32 = row.get(1)?;
                Ok((file_path, line_number))
            },
        )
        .ok()
    }

    /// 按条件搜索消息索引
    /// SearchMessages 工具调用此方法进行结构化查询
    /// 多个条件之间为 AND 关系，按时间戳降序排列（最新的在前）
    ///
    /// # 参数
    /// - `criteria`: 搜索条件（所有字段可选）
    ///
    /// # 返回
    /// 匹配的消息元数据列表（按时间降序，受 limit/offset 限制）
    pub fn search(&self, criteria: &SearchCriteria) -> Vec<SearchResult> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };

        // 动态构建 WHERE 子句和参数
        let mut conditions: Vec<String> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        // 按发送者过滤
        if let Some(uid) = criteria.user_id {
            conditions.push(format!("user_id = ?{}", params.len() + 1));
            params.push(Box::new(uid));
        }

        // 按群号过滤
        if let Some(gid) = criteria.group_id {
            conditions.push(format!("group_id = ?{}", params.len() + 1));
            params.push(Box::new(gid));
        }

        // 按消息类型过滤
        if let Some(ref mt) = criteria.message_type {
            conditions.push(format!("message_type = ?{}", params.len() + 1));
            params.push(Box::new(mt.clone()));
        }

        // 按起始时间过滤
        if let Some(after) = criteria.after {
            conditions.push(format!("timestamp >= ?{}", params.len() + 1));
            params.push(Box::new(after));
        }

        // 按截止时间过滤
        if let Some(before) = criteria.before {
            conditions.push(format!("timestamp <= ?{}", params.len() + 1));
            params.push(Box::new(before));
        }

        // 组装 SQL 查询
        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        // 限制结果数量（上限 100）
        let limit = criteria.limit.min(100).max(1);
        let offset = criteria.offset;

        let sql = format!(
            "SELECT message_id, file_path, line_number, timestamp, user_id, group_id, message_type
             FROM message_index
             {where_clause}
             ORDER BY timestamp DESC
             LIMIT ?{} OFFSET ?{}",
            params.len() + 1,
            params.len() + 2,
        );

        // 添加 LIMIT 和 OFFSET 参数
        params.push(Box::new(limit));
        params.push(Box::new(offset));

        // 构建参数引用切片（rusqlite 要求 &[&dyn ToSql]）
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();

        // 执行查询
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return Vec::new();
        };

        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            // 读取 group_id（可能为 NULL）
            let group_id: Option<i64> = row.get(5)?;
            Ok(SearchResult {
                message_id: row.get(0)?,
                file_path: row.get(1)?,
                line_number: row.get(2)?,
                timestamp: row.get(3)?,
                user_id: row.get(4)?,
                group_id,
                message_type: row.get(6)?,
            })
        });

        // 收集结果，跳过读取失败的行
        match rows {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// 获取索引中的消息总数（用于诊断和测试）
    pub fn count(&self) -> usize {
        let Ok(conn) = self.conn.lock() else {
            return 0;
        };
        conn.query_row(
            "SELECT COUNT(*) FROM message_index",
            [],
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
    fn temp_index() -> (MessageIndex, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_messages.db");
        let index = MessageIndex::open(&db_path).unwrap();
        (index, dir)
    }

    /// 测试：写入索引并通过 ID 查找
    #[test]
    fn index_and_lookup() {
        let (index, _dir) = temp_index();

        // 写入一条群消息索引
        index.index_message(
            100,
            "all/2026-03-10_001.jsonl",
            1,
            1710000000,
            12345,
            Some(67890),
            "group",
        );

        // 通过 ID 查找
        let result = index.lookup(100);
        assert_eq!(
            result,
            Some(("all/2026-03-10_001.jsonl".to_string(), 1))
        );

        // 查找不存在的消息
        assert_eq!(index.lookup(999), None);
    }

    /// 测试：写入索引并通过 ID 查找私聊消息（group_id 为 NULL）
    #[test]
    fn index_private_message() {
        let (index, _dir) = temp_index();

        // 写入一条私聊消息索引（group_id = None）
        index.index_message(
            200,
            "all/2026-03-10_001.jsonl",
            5,
            1710000100,
            11111,
            None,
            "private",
        );

        // 查找
        let result = index.lookup(200);
        assert_eq!(
            result,
            Some(("all/2026-03-10_001.jsonl".to_string(), 5))
        );
    }

    /// 测试：重复写入覆盖旧值
    #[test]
    fn index_overwrites() {
        let (index, _dir) = temp_index();

        index.index_message(100, "all/old.jsonl", 1, 1710000000, 12345, Some(67890), "group");
        assert_eq!(
            index.lookup(100),
            Some(("all/old.jsonl".to_string(), 1))
        );

        // 重新写入覆盖
        index.index_message(100, "all/new.jsonl", 3, 1710000000, 12345, Some(67890), "group");
        assert_eq!(
            index.lookup(100),
            Some(("all/new.jsonl".to_string(), 3))
        );
    }

    /// 测试：消息总数统计
    #[test]
    fn message_count() {
        let (index, _dir) = temp_index();

        assert_eq!(index.count(), 0);

        index.index_message(1, "all/a.jsonl", 1, 1710000000, 100, Some(200), "group");
        index.index_message(2, "all/a.jsonl", 2, 1710000001, 101, None, "private");
        index.index_message(3, "all/a.jsonl", 3, 1710000002, 100, Some(300), "group");

        assert_eq!(index.count(), 3);
    }

    /// 测试：按发送者搜索
    #[test]
    fn search_by_user() {
        let (index, _dir) = temp_index();

        // 写入多条消息
        index.index_message(1, "all/a.jsonl", 1, 1710000000, 100, Some(200), "group");
        index.index_message(2, "all/a.jsonl", 2, 1710000001, 101, Some(200), "group");
        index.index_message(3, "all/a.jsonl", 3, 1710000002, 100, Some(300), "group");
        index.index_message(4, "all/a.jsonl", 4, 1710000003, 102, None, "private");

        // 搜索 user_id=100 的消息
        let results = index.search(&SearchCriteria {
            user_id: Some(100),
            limit: 20,
            ..Default::default()
        });

        // 应返回 2 条（message_id 1 和 3），按时间降序
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].message_id, 3); // 最新的在前
        assert_eq!(results[1].message_id, 1);
    }

    /// 测试：按群号搜索
    #[test]
    fn search_by_group() {
        let (index, _dir) = temp_index();

        index.index_message(1, "all/a.jsonl", 1, 1710000000, 100, Some(200), "group");
        index.index_message(2, "all/a.jsonl", 2, 1710000001, 101, Some(300), "group");
        index.index_message(3, "all/a.jsonl", 3, 1710000002, 100, Some(200), "group");

        let results = index.search(&SearchCriteria {
            group_id: Some(200),
            limit: 20,
            ..Default::default()
        });

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].message_id, 3);
        assert_eq!(results[1].message_id, 1);
    }

    /// 测试：按时间范围搜索
    #[test]
    fn search_by_time_range() {
        let (index, _dir) = temp_index();

        index.index_message(1, "all/a.jsonl", 1, 1000, 100, Some(200), "group");
        index.index_message(2, "all/a.jsonl", 2, 2000, 101, Some(200), "group");
        index.index_message(3, "all/a.jsonl", 3, 3000, 100, Some(200), "group");
        index.index_message(4, "all/a.jsonl", 4, 4000, 100, Some(200), "group");

        // 搜索时间在 1500-3500 之间的消息
        let results = index.search(&SearchCriteria {
            after: Some(1500),
            before: Some(3500),
            limit: 20,
            ..Default::default()
        });

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].message_id, 3); // timestamp=3000
        assert_eq!(results[1].message_id, 2); // timestamp=2000
    }

    /// 测试：按消息类型搜索
    #[test]
    fn search_by_type() {
        let (index, _dir) = temp_index();

        index.index_message(1, "all/a.jsonl", 1, 1000, 100, Some(200), "group");
        index.index_message(2, "all/a.jsonl", 2, 2000, 101, None, "private");
        index.index_message(3, "all/a.jsonl", 3, 3000, 100, Some(300), "group");

        let results = index.search(&SearchCriteria {
            message_type: Some("private".to_string()),
            limit: 20,
            ..Default::default()
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_id, 2);
        assert_eq!(results[0].group_id, None);
    }

    /// 测试：复合条件搜索（user_id + group_id）
    #[test]
    fn search_combined_criteria() {
        let (index, _dir) = temp_index();

        index.index_message(1, "all/a.jsonl", 1, 1000, 100, Some(200), "group");
        index.index_message(2, "all/a.jsonl", 2, 2000, 100, Some(300), "group");
        index.index_message(3, "all/a.jsonl", 3, 3000, 101, Some(200), "group");
        index.index_message(4, "all/a.jsonl", 4, 4000, 100, Some(200), "group");

        // user_id=100 AND group_id=200
        let results = index.search(&SearchCriteria {
            user_id: Some(100),
            group_id: Some(200),
            limit: 20,
            ..Default::default()
        });

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].message_id, 4);
        assert_eq!(results[1].message_id, 1);
    }

    /// 测试：分页（limit + offset）
    #[test]
    fn search_pagination() {
        let (index, _dir) = temp_index();

        // 写入 5 条消息
        for i in 1..=5 {
            index.index_message(i, "all/a.jsonl", i as u32, i * 1000, 100, Some(200), "group");
        }

        // 第一页：limit=2, offset=0
        let page1 = index.search(&SearchCriteria {
            limit: 2,
            offset: 0,
            ..Default::default()
        });
        assert_eq!(page1.len(), 2);
        assert_eq!(page1[0].message_id, 5); // 最新
        assert_eq!(page1[1].message_id, 4);

        // 第二页：limit=2, offset=2
        let page2 = index.search(&SearchCriteria {
            limit: 2,
            offset: 2,
            ..Default::default()
        });
        assert_eq!(page2.len(), 2);
        assert_eq!(page2[0].message_id, 3);
        assert_eq!(page2[1].message_id, 2);

        // 第三页：只剩 1 条
        let page3 = index.search(&SearchCriteria {
            limit: 2,
            offset: 4,
            ..Default::default()
        });
        assert_eq!(page3.len(), 1);
        assert_eq!(page3[0].message_id, 1);
    }

    /// 测试：limit 上限 100 的限制
    #[test]
    fn search_limit_capped() {
        let (index, _dir) = temp_index();

        // 写入 5 条消息
        for i in 1..=5 {
            index.index_message(i, "all/a.jsonl", i as u32, i * 1000, 100, Some(200), "group");
        }

        // 请求 limit=999，但实际不会超过 100（这里只有 5 条）
        let results = index.search(&SearchCriteria {
            limit: 999,
            ..Default::default()
        });
        assert_eq!(results.len(), 5);
    }

    /// 测试：空条件搜索返回所有消息
    #[test]
    fn search_no_criteria() {
        let (index, _dir) = temp_index();

        index.index_message(1, "all/a.jsonl", 1, 1000, 100, Some(200), "group");
        index.index_message(2, "all/a.jsonl", 2, 2000, 101, None, "private");

        let results = index.search(&SearchCriteria {
            limit: 20,
            ..Default::default()
        });
        assert_eq!(results.len(), 2);
    }

    /// 测试：数据库持久化（关闭后重新打开）
    #[test]
    fn persistence_across_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("persist_test.db");

        // 第一次打开，写入数据
        {
            let index = MessageIndex::open(&db_path).unwrap();
            index.index_message(42, "all/data.jsonl", 7, 1710000000, 12345, Some(67890), "group");
        }

        // 第二次打开，数据仍在
        {
            let index = MessageIndex::open(&db_path).unwrap();
            assert_eq!(
                index.lookup(42),
                Some(("all/data.jsonl".to_string(), 7))
            );
            assert_eq!(index.count(), 1);
        }
    }
}
