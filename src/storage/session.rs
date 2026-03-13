// src/storage/session.rs
// 会话文件管理模块
// 提供 JSONL 格式的完整会话记录、SQLite 会话索引和 SessionManager 协调器
//
// 设计目标：
// - 所有对话消息（含模型回复和工具调用结果）持久化到 JSONL 文件
// - SQLite 追踪当前活跃会话和会话链（prev_id → current_id）
// - 内存缓存与 JSONL 文件双写，文件为权威源
// - 每次 compact 创建新会话文件，旧文件保留供 AI 通过 Read 工具查阅
// - 系统提示中注入上一条会话的路径引用
//
// 数据流：
// 1. 启动时 SessionManager::open() 检查 is_current=1 → 恢复或新建
// 2. Agent 循环中 session.push(msg) 双写到 JSONL + 内存
// 3. compact 触发 → session.rotate(initial_messages) 创建新会话
// 4. 旧会话文件保留在 workspace/sessions/，AI 可通过 Read 工具查阅

use crate::providers::traits::ChatMessage;
use anyhow::Context as _;
use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// ============================================================================
// SQLite 会话索引
// ============================================================================

/// 会话记录（对应 sessions 表中的一行）
#[derive(Debug, Clone)]
pub struct SessionRecord {
    /// 自增主键
    pub id: i64,
    /// JSONL 文件路径（相对于 workspace，如 "sessions/2026-03-13_001.jsonl"）
    pub file_path: String,
    /// 前一个会话的 ID（链式引用，首个会话为 None）
    pub prev_id: Option<i64>,
    /// 创建时间（Unix 时间戳，秒）
    pub created_at: i64,
    /// 是否为当前活跃会话
    pub is_current: bool,
}

/// 会话索引数据库
/// 使用 SQLite 持久化存储会话链和当前活跃会话标记
/// 线程安全：通过 Mutex 保护 Connection（rusqlite::Connection 是 Send 但非 Sync）
pub struct SessionIndex {
    /// SQLite 数据库连接（Mutex 保证线程安全）
    conn: Mutex<rusqlite::Connection>,
}

impl SessionIndex {
    /// 打开或创建会话索引数据库
    /// 自动创建父目录、表结构和索引
    ///
    /// # 参数
    /// - `db_path`: 数据库文件路径（如 `workspace/data/sessions.db`）
    pub fn open(db_path: &Path) -> anyhow::Result<Self> {
        // 确保父目录存在
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("创建会话索引数据库目录失败: {}", parent.display())
            })?;
        }

        // 打开 SQLite 数据库（不存在则自动创建）
        let conn = rusqlite::Connection::open(db_path).with_context(|| {
            format!("打开会话索引数据库失败: {}", db_path.display())
        })?;

        // 启用 WAL 模式（提高并发读写性能）
        conn.pragma_update(None, "journal_mode", "WAL")?;

        // 创建表结构和索引（如果不存在）
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                file_path   TEXT    NOT NULL UNIQUE,
                prev_id     INTEGER,
                created_at  INTEGER NOT NULL,
                is_current  INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY (prev_id) REFERENCES sessions(id)
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_current ON sessions(is_current);",
        )
        .with_context(|| "创建 sessions 表和索引失败")?;

        tracing::info!("会话索引数据库已打开: {}", db_path.display());

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 创建新会话记录
    /// 在事务内完成：先将所有旧会话的 is_current 清零，再插入新记录 is_current=1
    ///
    /// # 参数
    /// - `file_path`: 新会话的 JSONL 文件路径（相对于 workspace）
    /// - `prev_id`: 前一个会话的 ID（首个会话传 None）
    ///
    /// # 返回
    /// 新创建的会话 ID
    pub fn create_session(
        &self,
        file_path: &str,
        prev_id: Option<i64>,
    ) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().timestamp();

        // 在事务内完成：清零 + 插入
        let tx = conn.unchecked_transaction()?;

        // 将所有旧会话的 is_current 设为 0
        tx.execute("UPDATE sessions SET is_current = 0 WHERE is_current = 1", [])?;

        // 插入新记录，标记为当前活跃
        tx.execute(
            "INSERT INTO sessions (file_path, prev_id, created_at, is_current) VALUES (?1, ?2, ?3, 1)",
            rusqlite::params![file_path, prev_id, now],
        )?;

        // 获取新插入记录的 ID
        let id = tx.last_insert_rowid();
        tx.commit()?;

        tracing::info!(
            "创建新会话: id={}, file={}, prev_id={:?}",
            id, file_path, prev_id
        );

        Ok(id)
    }

    /// 获取当前活跃会话记录
    /// 返回 is_current=1 的会话（正常情况下最多只有一条）
    pub fn get_current(&self) -> anyhow::Result<Option<SessionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, file_path, prev_id, created_at, is_current \
             FROM sessions WHERE is_current = 1 LIMIT 1",
        )?;

        let record = stmt
            .query_row([], |row| {
                Ok(SessionRecord {
                    id: row.get(0)?,
                    file_path: row.get(1)?,
                    prev_id: row.get(2)?,
                    created_at: row.get(3)?,
                    is_current: row.get::<_, i64>(4)? != 0,
                })
            })
            .optional()?;

        Ok(record)
    }

    /// 通过 ID 获取会话记录
    pub fn get_by_id(&self, id: i64) -> anyhow::Result<Option<SessionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, file_path, prev_id, created_at, is_current \
             FROM sessions WHERE id = ?1",
        )?;

        let record = stmt
            .query_row(rusqlite::params![id], |row| {
                Ok(SessionRecord {
                    id: row.get(0)?,
                    file_path: row.get(1)?,
                    prev_id: row.get(2)?,
                    created_at: row.get(3)?,
                    is_current: row.get::<_, i64>(4)? != 0,
                })
            })
            .optional()?;

        Ok(record)
    }
}

// ============================================================================
// JSONL 文件 I/O
// ============================================================================

/// 追加一条 ChatMessage 到会话 JSONL 文件
/// 每条消息序列化为一行 JSON，追加到文件末尾
///
/// # 参数
/// - `workspace_dir`: 工作区根目录路径
/// - `session_file`: 会话文件的相对路径（如 "sessions/2026-03-13_001.jsonl"）
/// - `msg`: 要写入的消息
pub fn append_session_message(
    workspace_dir: &Path,
    session_file: &str,
    msg: &ChatMessage,
) -> anyhow::Result<()> {
    let path = workspace_dir.join(session_file);

    // 确保父目录存在
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("创建会话文件目录失败: {}", parent.display())
        })?;
    }

    // 序列化消息为 JSON 字符串
    let json_line = serde_json::to_string(msg)
        .with_context(|| "序列化 ChatMessage 失败")?;

    // 以追加模式打开文件，写入一行 JSON
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("打开会话文件失败: {}", path.display()))?;

    writeln!(file, "{json_line}")
        .with_context(|| format!("写入会话文件失败: {}", path.display()))?;

    Ok(())
}

/// 从 JSONL 文件加载所有消息
/// 逐行解析 JSON，跳过空行和格式错误的行（带警告日志）
///
/// # 参数
/// - `workspace_dir`: 工作区根目录路径
/// - `session_file`: 会话文件的相对路径
///
/// # 返回
/// 按写入顺序排列的消息列表
pub fn load_session_messages(
    workspace_dir: &Path,
    session_file: &str,
) -> anyhow::Result<Vec<ChatMessage>> {
    let path = workspace_dir.join(session_file);

    // 文件不存在视为空会话
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = std::fs::File::open(&path)
        .with_context(|| format!("打开会话文件失败: {}", path.display()))?;

    let reader = std::io::BufReader::new(file);
    let mut messages = Vec::new();

    for (line_num, line) in reader.lines().enumerate() {
        let line = line.with_context(|| {
            format!("读取会话文件第 {} 行失败: {}", line_num + 1, path.display())
        })?;

        // 跳过空行
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // 解析 JSON
        match serde_json::from_str::<ChatMessage>(trimmed) {
            Ok(msg) => messages.push(msg),
            Err(e) => {
                tracing::warn!(
                    "会话文件 {} 第 {} 行解析失败（已跳过）: {}",
                    session_file, line_num + 1, e
                );
            }
        }
    }

    Ok(messages)
}

/// 生成新的会话文件路径：sessions/{date}_{seq}.jsonl
/// 自动递增序号，避免与已存在的文件冲突
///
/// # 参数
/// - `workspace_dir`: 工作区根目录路径
///
/// # 返回
/// 相对于 workspace 的路径（如 "sessions/2026-03-13_001.jsonl"）
pub fn next_session_file_path(workspace_dir: &Path) -> String {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let sessions_dir = workspace_dir.join("sessions");

    // 查找当天已存在的最大序号
    let mut seq: u32 = 1;
    loop {
        let filename = format!("{today}_{seq:03}.jsonl");
        let full_path = sessions_dir.join(&filename);
        if !full_path.exists() {
            return format!("sessions/{filename}");
        }
        seq += 1;
    }
}

// ============================================================================
// SessionManager（协调器）
// ============================================================================

/// 会话管理器
/// 协调 SQLite 索引和 JSONL 文件，提供统一的双写接口
/// 内存缓存与文件同步，确保 API 请求可以直接从缓存构建
pub struct SessionManager {
    /// SQLite 会话索引
    index: SessionIndex,
    /// 工作区根目录路径
    workspace_dir: PathBuf,
    /// 当前活跃会话的数据库 ID
    current_session_id: i64,
    /// 当前活跃会话的 JSONL 文件路径（相对于 workspace）
    current_file_path: String,
    /// 内存缓存：与 JSONL 文件内容同步（双写策略）
    messages: Vec<ChatMessage>,
    /// 上一个会话的文件路径（用于系统提示注入，让 AI 可以 Read 查看历史）
    prev_session_path: Option<String>,
}

impl SessionManager {
    /// 打开会话管理器
    /// 启动时检查数据库：
    /// - 有 is_current=1 的会话 → 从 JSONL 文件恢复内存缓存
    /// - 无当前会话 → 创建新会话文件
    ///
    /// # 参数
    /// - `workspace_dir`: 工作区根目录路径（如 "./workspace"）
    pub fn open(workspace_dir: &Path) -> anyhow::Result<Self> {
        // 打开 SQLite 数据库（位于 workspace/data/sessions.db）
        let db_path = workspace_dir.join("data").join("sessions.db");
        let index = SessionIndex::open(&db_path)?;

        // 检查是否有当前活跃会话
        if let Some(current) = index.get_current()? {
            tracing::info!(
                "恢复现有会话: id={}, file={}",
                current.id, current.file_path
            );

            // 从 JSONL 文件加载消息到内存
            let messages = load_session_messages(workspace_dir, &current.file_path)?;
            tracing::info!("从会话文件恢复了 {} 条消息", messages.len());

            // 查找前一个会话路径（用于系统提示注入）
            let prev_session_path = if let Some(prev_id) = current.prev_id {
                index
                    .get_by_id(prev_id)?
                    .map(|r| r.file_path)
            } else {
                None
            };

            Ok(Self {
                index,
                workspace_dir: workspace_dir.to_path_buf(),
                current_session_id: current.id,
                current_file_path: current.file_path,
                messages,
                prev_session_path,
            })
        } else {
            // 首次启动：创建新会话
            let file_path = next_session_file_path(workspace_dir);
            let session_id = index.create_session(&file_path, None)?;

            tracing::info!(
                "创建首个会话: id={}, file={}",
                session_id, file_path
            );

            Ok(Self {
                index,
                workspace_dir: workspace_dir.to_path_buf(),
                current_session_id: session_id,
                current_file_path: file_path,
                messages: Vec::new(),
                prev_session_path: None,
            })
        }
    }

    /// 双写一条消息：追加到 JSONL 文件 + push 到内存缓存
    /// 文件写入失败不会导致内存缓存不一致（先写文件，成功后再 push 到内存）
    pub fn push(&mut self, message: ChatMessage) -> anyhow::Result<()> {
        // 先写文件（权威源）
        append_session_message(&self.workspace_dir, &self.current_file_path, &message)?;
        // 文件写入成功后，push 到内存缓存
        self.messages.push(message);
        Ok(())
    }

    /// 获取内存缓存中的所有消息（只读）
    /// 用于构建 API 请求体
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// 获取内存缓存的可变引用
    /// 供 compact::maybe_compact_history() 直接修改内存中的消息列表
    /// 注意：修改后内存缓存与 JSONL 文件不同步（旧文件保留原始完整记录）
    pub fn messages_mut(&mut self) -> &mut Vec<ChatMessage> {
        &mut self.messages
    }

    /// 轮换会话：创建新会话文件，写入初始消息
    /// 在 compact 后调用，将压缩后的消息写入新会话文件
    /// 旧会话文件保留完整历史记录，供 AI 通过 Read 工具查阅
    ///
    /// # 参数
    /// - `initial_messages`: 新会话的初始消息列表（通常包含摘要 + 保留的最近消息）
    pub fn rotate(&mut self, initial_messages: Vec<ChatMessage>) -> anyhow::Result<()> {
        // 1. 生成新文件路径
        let new_file_path = next_session_file_path(&self.workspace_dir);

        // 2. 在数据库中创建新会话记录，链接到当前会话
        let new_id = self
            .index
            .create_session(&new_file_path, Some(self.current_session_id))?;

        // 3. 逐条写入初始消息到新 JSONL 文件
        for msg in &initial_messages {
            append_session_message(&self.workspace_dir, &new_file_path, msg)?;
        }

        // 4. 更新内部状态
        self.prev_session_path = Some(self.current_file_path.clone());
        self.current_session_id = new_id;
        self.current_file_path = new_file_path;
        self.messages = initial_messages;

        tracing::info!(
            "会话已轮换: id={}, file={}, prev={}",
            self.current_session_id,
            self.current_file_path,
            self.prev_session_path.as_deref().unwrap_or("(none)"),
        );

        Ok(())
    }

    /// 获取上一个会话的文件路径（用于系统提示注入）
    pub fn prev_session_path(&self) -> Option<&str> {
        self.prev_session_path.as_deref()
    }

    /// 获取当前会话的文件路径（用于 compact 时记录引用）
    pub fn current_file_path(&self) -> &str {
        &self.current_file_path
    }
}

// ============================================================================
// 辅助 trait 导入
// ============================================================================

/// rusqlite 的 OptionalExtension trait，用于 query_row 返回 Option
use rusqlite::OptionalExtension as _;

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// 辅助函数：创建临时工作区目录
    fn temp_workspace() -> TempDir {
        tempfile::tempdir().expect("创建临时目录失败")
    }

    /// 辅助函数：在临时目录中打开 SessionIndex
    fn open_test_index(tmp: &Path) -> SessionIndex {
        let db_path = tmp.join("data").join("sessions.db");
        SessionIndex::open(&db_path).expect("打开测试数据库失败")
    }

    // ---- SessionIndex 测试 ----

    #[test]
    fn create_and_get_current() {
        // 创建会话后应能通过 get_current 获取
        let tmp = temp_workspace();
        let index = open_test_index(tmp.path());

        let id = index
            .create_session("sessions/2026-03-13_001.jsonl", None)
            .unwrap();
        assert_eq!(id, 1);

        let current = index.get_current().unwrap().expect("应有当前会话");
        assert_eq!(current.id, 1);
        assert_eq!(current.file_path, "sessions/2026-03-13_001.jsonl");
        assert!(current.prev_id.is_none());
        assert!(current.is_current);
    }

    #[test]
    fn rotate_updates_current() {
        // 创建第二个会话后，第一个应不再是 current
        let tmp = temp_workspace();
        let index = open_test_index(tmp.path());

        let id1 = index
            .create_session("sessions/2026-03-13_001.jsonl", None)
            .unwrap();
        let id2 = index
            .create_session("sessions/2026-03-13_002.jsonl", Some(id1))
            .unwrap();

        // 当前应为 id2
        let current = index.get_current().unwrap().expect("应有当前会话");
        assert_eq!(current.id, id2);
        assert_eq!(current.prev_id, Some(id1));

        // id1 不再是 current
        let old = index.get_by_id(id1).unwrap().expect("id1 应存在");
        assert!(!old.is_current);
    }

    #[test]
    fn chain_traversal() {
        // 连续创建 3 个会话，验证链式引用
        let tmp = temp_workspace();
        let index = open_test_index(tmp.path());

        let id1 = index
            .create_session("sessions/s1.jsonl", None)
            .unwrap();
        let id2 = index
            .create_session("sessions/s2.jsonl", Some(id1))
            .unwrap();
        let id3 = index
            .create_session("sessions/s3.jsonl", Some(id2))
            .unwrap();

        // 从 id3 反向遍历链
        let s3 = index.get_by_id(id3).unwrap().unwrap();
        assert_eq!(s3.prev_id, Some(id2));

        let s2 = index.get_by_id(s3.prev_id.unwrap()).unwrap().unwrap();
        assert_eq!(s2.prev_id, Some(id1));

        let s1 = index.get_by_id(s2.prev_id.unwrap()).unwrap().unwrap();
        assert!(s1.prev_id.is_none());
    }

    // ---- JSONL I/O 测试 ----

    #[test]
    fn append_and_load_messages() {
        // 写入多条消息后应能完整读回
        let tmp = temp_workspace();
        let ws = tmp.path();
        let file = "sessions/test_001.jsonl";

        // 写入 3 条消息
        append_session_message(ws, file, &ChatMessage::user("hello")).unwrap();
        append_session_message(ws, file, &ChatMessage::assistant("hi there")).unwrap();
        append_session_message(ws, file, &ChatMessage::user("bye")).unwrap();

        // 读回
        let loaded = load_session_messages(ws, file).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].role, "user");
        assert_eq!(loaded[0].content, "hello");
        assert_eq!(loaded[1].role, "assistant");
        assert_eq!(loaded[1].content, "hi there");
        assert_eq!(loaded[2].role, "user");
        assert_eq!(loaded[2].content, "bye");
    }

    #[test]
    fn load_nonexistent_file_returns_empty() {
        // 文件不存在时应返回空列表
        let tmp = temp_workspace();
        let loaded = load_session_messages(tmp.path(), "sessions/nonexistent.jsonl").unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn next_session_file_path_increments() {
        // 当文件已存在时应自动递增序号
        let tmp = temp_workspace();
        let ws = tmp.path();

        // 第一个路径
        let path1 = next_session_file_path(ws);
        assert!(path1.starts_with("sessions/"));
        assert!(path1.ends_with("_001.jsonl"));

        // 创建该文件
        append_session_message(ws, &path1, &ChatMessage::user("test")).unwrap();

        // 第二个路径应递增到 _002
        let path2 = next_session_file_path(ws);
        assert!(path2.ends_with("_002.jsonl"));
    }

    // ---- SessionManager 测试 ----

    #[test]
    fn session_manager_open_new() {
        // 首次打开应创建新会话，消息列表为空
        let tmp = temp_workspace();
        let mgr = SessionManager::open(tmp.path()).unwrap();

        assert!(mgr.messages().is_empty());
        assert!(mgr.prev_session_path().is_none());
        assert!(mgr.current_file_path().starts_with("sessions/"));
    }

    #[test]
    fn session_manager_push_and_read() {
        // push 消息后应能从 messages() 读取
        let tmp = temp_workspace();
        let mut mgr = SessionManager::open(tmp.path()).unwrap();

        mgr.push(ChatMessage::user("hello world")).unwrap();
        mgr.push(ChatMessage::assistant("hi")).unwrap();

        assert_eq!(mgr.messages().len(), 2);
        assert_eq!(mgr.messages()[0].content, "hello world");
        assert_eq!(mgr.messages()[1].content, "hi");
    }

    #[test]
    fn session_manager_restore() {
        // 关闭后重新打开应恢复消息
        let tmp = temp_workspace();

        // 第一次打开，写入消息
        {
            let mut mgr = SessionManager::open(tmp.path()).unwrap();
            mgr.push(ChatMessage::user("消息 1")).unwrap();
            mgr.push(ChatMessage::assistant("回复 1")).unwrap();
        }

        // 第二次打开，应恢复
        {
            let mgr = SessionManager::open(tmp.path()).unwrap();
            assert_eq!(mgr.messages().len(), 2);
            assert_eq!(mgr.messages()[0].content, "消息 1");
            assert_eq!(mgr.messages()[1].content, "回复 1");
        }
    }

    #[test]
    fn session_manager_rotate() {
        // rotate 后应创建新会话，旧文件保留
        let tmp = temp_workspace();
        let mut mgr = SessionManager::open(tmp.path()).unwrap();

        // 写入一些消息
        mgr.push(ChatMessage::user("原始消息 1")).unwrap();
        mgr.push(ChatMessage::assistant("原始回复 1")).unwrap();
        let old_file = mgr.current_file_path().to_string();

        // 轮换：新会话只有摘要
        let initial = vec![ChatMessage::user("这是压缩摘要")];
        mgr.rotate(initial).unwrap();

        // 验证新会话状态
        assert_eq!(mgr.messages().len(), 1);
        assert_eq!(mgr.messages()[0].content, "这是压缩摘要");
        assert_eq!(mgr.prev_session_path(), Some(old_file.as_str()));
        assert_ne!(mgr.current_file_path(), old_file);

        // 验证旧文件仍可读
        let old_messages = load_session_messages(tmp.path(), &old_file).unwrap();
        assert_eq!(old_messages.len(), 2);
        assert_eq!(old_messages[0].content, "原始消息 1");
    }

    #[test]
    fn persistence_across_reopens() {
        // 完整流程：push → rotate → 关闭 → 重新打开 → 验证
        let tmp = temp_workspace();

        let old_file;
        {
            let mut mgr = SessionManager::open(tmp.path()).unwrap();
            mgr.push(ChatMessage::user("old msg")).unwrap();
            old_file = mgr.current_file_path().to_string();

            mgr.rotate(vec![
                ChatMessage::user("summary of old session"),
                ChatMessage::user("kept recent msg"),
            ])
            .unwrap();

            mgr.push(ChatMessage::assistant("new reply")).unwrap();
        }

        // 重新打开
        let mgr = SessionManager::open(tmp.path()).unwrap();
        // 新会话应有 3 条：2 条 initial + 1 条 new reply
        assert_eq!(mgr.messages().len(), 3);
        assert_eq!(mgr.messages()[0].content, "summary of old session");
        assert_eq!(mgr.messages()[1].content, "kept recent msg");
        assert_eq!(mgr.messages()[2].content, "new reply");
        // prev_session_path 应指向旧会话
        assert_eq!(mgr.prev_session_path(), Some(old_file.as_str()));
    }
}
