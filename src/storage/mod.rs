// src/storage/mod.rs
// 消息存储管理器
// 路由消息到对应的 JSONL（原始）和 MD（AI可读）写入器
// 管理按日期+序号拆分的文件结构

pub mod cursor;
pub mod images;
pub mod jsonl;
pub mod markdown;
pub mod message_index;
pub mod session;

use crate::config::StorageConfig;
use anyhow::{Context, Result};
use chrono::{Local, NaiveDate};
use std::path::{Path, PathBuf};

// ============================================================================
// 存储位置信息
// ============================================================================

/// JSONL 文件写入后返回的物理位置信息
/// main.rs 用此信息调用 message_index.index_message() 建立索引
#[derive(Debug, Clone)]
pub struct StoredLocation {
    /// JSONL 文件路径（相对于 messages_dir，如 "all/2026-03-10_001.jsonl"）
    pub file_path: String,
    /// 写入的行号（1-based）
    pub line_number: u32,
}

// ============================================================================
// 存储管理器
// ============================================================================

/// 消息存储管理器
/// 负责将消息持久化到文件系统
///
/// 核心设计：所有消息按时间顺序统一存储在 `messages/all/` 目录中，
/// 不按群/私聊分离，确保 Agent 能看到完整的时间线上下文。
///
/// 文件结构:
/// - `messages/all/{date}_{seq}.{jsonl|md}` — 全部消息的统一时间流
/// - `messages/master/{date}_{seq}.md` — 主人消息副本（用于风格学习）
/// - `messages/self/{date}_{seq}.md` — Agent 自发消息副本（用于自我分析）
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

    /// 确保指定子目录存在并返回其路径
    fn ensure_dir(&self, sub: &str) -> Result<PathBuf> {
        let dir = self.messages_dir.join(sub);
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
    /// - `dir`: 目标目录路径
    /// - `date`: 当天日期
    /// - `ext`: 文件扩展名（"jsonl" 或 "md"）
    fn resolve_file_path(
        &self,
        dir: &Path,
        date: NaiveDate,
        ext: &str,
    ) -> Result<PathBuf> {
        let date_str = date.format("%Y-%m-%d").to_string();
        let mut seq: u32 = 1;

        // 查找当天已存在的最大序号
        loop {
            let filename = format!("{date_str}_{seq:03}.{ext}");
            let path = dir.join(&filename);

            if !path.exists() {
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
    /// 所有群聊和私聊的原始 JSON 按时间顺序追加到 messages/raw/ 下
    /// 此目录受写保护，LLM 不可修改（仅系统写入）
    /// 返回文件路径和行号，供消息索引使用
    pub fn store_raw_jsonl(&self, raw_json: &serde_json::Value) -> Result<StoredLocation> {
        let dir = self.ensure_dir("raw")?;
        let today = Local::now().date_naive();
        let path = self.resolve_file_path(&dir, today, "jsonl")?;

        // 在追加前统计当前行数，确定写入后的行号
        let current_count = count_lines(&path)?;
        let line_number = current_count + 1;

        // 追加 JSON 到文件
        jsonl::append_jsonl(&path, raw_json)?;

        // 提取相对于 messages_dir 的路径（如 "raw/2026-03-10_001.jsonl"）
        let relative = path
            .strip_prefix(&self.messages_dir)
            .unwrap_or(&path)
            .to_string_lossy()
            // 统一使用正斜杠（跨平台兼容）
            .replace('\\', "/");

        Ok(StoredLocation {
            file_path: relative,
            line_number,
        })
    }

    /// 存储一条格式化消息到统一 MD 文件
    /// 所有群聊和私聊的结构化消息按时间顺序追加到 messages/all/ 下
    pub fn store_markdown(&self, record: &markdown::MessageRecord) -> Result<()> {
        let dir = self.ensure_dir("all")?;
        let today = Local::now().date_naive();
        let path = self.resolve_file_path(&dir, today, "md")?;
        markdown::append_markdown(&path, record)
    }

    /// 存储主人的消息到风格分析文件
    /// 独立存储到 messages/master/ 目录下，按日期拆分
    /// 用于 LLM 学习主人的说话风格
    /// 注意：当 master=self 时，此处仅包含主人本人发送的消息，不含 Agent 发送的
    pub fn store_master(&self, record: &markdown::MessageRecord) -> Result<()> {
        let dir = self.ensure_dir("master")?;
        let today = Local::now().date_naive();
        let path = self.resolve_file_path(&dir, today, "md")?;
        markdown::append_markdown(&path, record)
    }

    /// 存储 Agent 自发消息到独立文件
    /// 独立存储到 messages/self/ 目录下，按日期拆分
    /// 仅包含 Agent 确认发送的消息（排除主人或他人通过同账号发送的）
    /// 用于自我分析、模仿程度评估等
    pub fn store_self(&self, record: &markdown::MessageRecord) -> Result<()> {
        let dir = self.ensure_dir("self")?;
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
