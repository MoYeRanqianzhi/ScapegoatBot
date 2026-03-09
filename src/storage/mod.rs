// src/storage/mod.rs
// 消息存储管理器
// 路由消息到对应的 JSONL（原始）和 MD（AI可读）写入器
// 管理按日期+序号拆分的文件结构

pub mod cursor;
pub mod jsonl;
pub mod markdown;

use crate::config::StorageConfig;
use anyhow::{Context, Result};
use chrono::{Local, NaiveDate};
use std::path::{Path, PathBuf};

// ============================================================================
// 会话类型
// ============================================================================

/// 会话类型枚举，用于确定存储目录
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionType {
    /// 群聊会话
    Group,
    /// 私聊会话
    Private,
}

impl SessionType {
    /// 获取对应的子目录名
    fn dir_name(&self) -> &str {
        match self {
            SessionType::Group => "group",
            SessionType::Private => "private",
        }
    }
}

// ============================================================================
// 存储管理器
// ============================================================================

/// 消息存储管理器
/// 负责将消息持久化到文件系统
/// 文件结构: {messages_dir}/{group|private}/{session_id}/{date}_{seq}.{jsonl|md}
#[derive(Debug, Clone)]
pub struct StorageManager {
    /// 消息存储根目录（如 ./workspace/messages）
    messages_dir: PathBuf,
    /// 单个文件的最大消息条数
    max_messages_per_file: u32,
}

impl StorageManager {
    /// 创建存储管理器
    pub fn new(config: &StorageConfig) -> Self {
        let messages_dir = PathBuf::from(&config.workspace_dir).join("messages");
        Self {
            messages_dir,
            max_messages_per_file: config.max_messages_per_file,
        }
    }

    /// 获取指定会话的存储目录路径
    /// 如果目录不存在则自动创建
    fn session_dir(&self, session_type: SessionType, session_id: i64) -> Result<PathBuf> {
        let dir = self
            .messages_dir
            .join(session_type.dir_name())
            .join(session_id.to_string());
        // 确保目录存在
        if !dir.exists() {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("创建存储目录失败: {}", dir.display()))?;
        }
        Ok(dir)
    }

    /// 确定当前应写入的文件路径（根据日期+序号）
    /// 如果当天最新文件的行数已达上限，序号 +1
    ///
    /// # 参数
    /// - `session_dir`: 会话目录路径
    /// - `date`: 当天日期
    /// - `ext`: 文件扩展名（"jsonl" 或 "md"）
    fn resolve_file_path(
        &self,
        session_dir: &Path,
        date: NaiveDate,
        ext: &str,
    ) -> Result<PathBuf> {
        let date_str = date.format("%Y-%m-%d").to_string();
        let mut seq: u32 = 1;

        // 查找当天已存在的最大序号
        loop {
            let filename = format!("{date_str}_{seq:03}.{ext}");
            let path = session_dir.join(&filename);

            if !path.exists() {
                // 文件不存在：如果是序号 1 则直接用，否则前一个文件未满就用前一个
                if seq > 1 {
                    // 前一个文件存在但可能未满
                    return Ok(path);
                }
                return Ok(path);
            }

            // 文件存在，检查行数是否达到上限
            let line_count = count_lines(&path)?;
            if line_count < self.max_messages_per_file {
                // 还没满，继续使用这个文件
                return Ok(path);
            }

            // 已满，尝试下一个序号
            seq += 1;
        }
    }

    /// 存储一条原始消息到 JSONL 文件
    /// 将消息的完整 JSON 追加到对应的 .jsonl 文件中
    pub fn store_raw_jsonl(
        &self,
        session_type: SessionType,
        session_id: i64,
        raw_json: &serde_json::Value,
    ) -> Result<()> {
        let dir = self.session_dir(session_type, session_id)?;
        let today = Local::now().date_naive();
        let path = self.resolve_file_path(&dir, today, "jsonl")?;

        jsonl::append_jsonl(&path, raw_json)
    }

    /// 存储一条格式化消息到 MD 文件
    /// 将结构化的 MessageRecord 追加到对应的 .md 文件中
    pub fn store_markdown(
        &self,
        session_type: SessionType,
        session_id: i64,
        record: &markdown::MessageRecord,
    ) -> Result<()> {
        let dir = self.session_dir(session_type, session_id)?;
        let today = Local::now().date_naive();
        let path = self.resolve_file_path(&dir, today, "md")?;

        markdown::append_markdown(&path, record)
    }

    /// 存储主人的消息到风格分析文件
    /// 独立存储到 messages/master_style/ 目录下，按日期拆分
    /// 用于 LLM 学习主人的说话风格
    pub fn store_master_style(&self, record: &markdown::MessageRecord) -> Result<()> {
        let dir = self.messages_dir.join("master_style");
        if !dir.exists() {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("创建 master_style 目录失败: {}", dir.display()))?;
        }
        let today = Local::now().date_naive();
        let path = self.resolve_file_path(&dir, today, "md")?;

        markdown::append_markdown(&path, record)
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 统计文件的行数（用于判断是否需要拆分）
/// JSONL 中每行一条消息；MD 中每条消息由 "---" 分隔
fn count_lines(path: &Path) -> Result<u32> {
    if !path.exists() {
        return Ok(0);
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取文件失败: {}", path.display()))?;

    if ext == "jsonl" {
        // JSONL: 每行一条消息
        Ok(content.lines().filter(|l| !l.trim().is_empty()).count() as u32)
    } else {
        // MD: 按 "---" 分隔符计数消息条数
        // 每条消息以 "### Time" 开头，以 "---" 结尾
        Ok(content.matches("\n---\n").count() as u32
            + if content.ends_with("\n---\n") || content.ends_with("\n---") {
                0
            } else if content.contains("### Time") {
                // 最后一条可能还没有结尾 ---
                1
            } else {
                0
            })
    }
}
