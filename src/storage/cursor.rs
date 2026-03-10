// src/storage/cursor.rs
// 消息游标系统
// 追踪 AI 最后处理的消息位置
// AI 启动时读取游标，计算统一消息流中未处理的消息范围

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 处理游标，追踪统一消息流中的最后处理位置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCursor {
    /// 最后处理的 MD 文件名（相对于 all/ 目录）
    pub last_file: String,
    /// 最后处理的消息 ID
    pub last_message_id: i64,
}

/// 保存游标到文件
pub fn save_cursor(
    cursors_dir: &Path,
    cursor: &SessionCursor,
) -> Result<()> {
    // 确保游标目录存在
    std::fs::create_dir_all(cursors_dir)
        .with_context(|| format!("创建游标目录失败: {}", cursors_dir.display()))?;
    let path = cursors_dir.join("unified.json");
    let json = serde_json::to_string_pretty(cursor)?;
    std::fs::write(&path, json)
        .with_context(|| format!("写入游标失败: {}", path.display()))?;
    Ok(())
}

/// 读取游标
pub fn load_cursor(
    cursors_dir: &Path,
) -> Result<Option<SessionCursor>> {
    let path = cursors_dir.join("unified.json");
    if !path.exists() {
        return Ok(None);
    }
    let json = std::fs::read_to_string(&path)
        .with_context(|| format!("读取游标失败: {}", path.display()))?;
    let cursor: SessionCursor = serde_json::from_str(&json)?;
    Ok(Some(cursor))
}

/// 计算 messages/all/ 目录下，从游标位置之后的未处理消息范围
/// 返回人类可读的描述，如 "`all/2026-03-09_001.md` 从 ID 789 之后到最新"
/// 如果没有游标（首次启动），则返回最近的一些文件
pub fn compute_all_unprocessed(messages_dir: &Path) -> Vec<String> {
    let all_dir = messages_dir.join("all");
    if !all_dir.exists() {
        return vec![];
    }

    // 收集所有 .md 文件并排序
    let mut md_files: Vec<String> = match std::fs::read_dir(&all_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect(),
        Err(_) => return vec![],
    };
    md_files.sort();

    if md_files.is_empty() {
        return vec![];
    }

    // 加载游标
    let cursors_dir = messages_dir.join(".cursors");
    let cursor = load_cursor(&cursors_dir).ok().flatten();

    let mut ranges = Vec::new();

    match cursor {
        Some(cur) => {
            // 有游标：从游标文件（含）开始，到最新文件
            let start_idx = md_files
                .iter()
                .position(|f| f == &cur.last_file)
                .unwrap_or(0);
            for file in &md_files[start_idx..] {
                let display_path = format!("all/{file}");
                if file == &cur.last_file {
                    ranges.push(format!(
                        "`{display_path}` 从 ID {} 之后到最新",
                        cur.last_message_id
                    ));
                } else {
                    ranges.push(format!("`{display_path}` 全部"));
                }
            }
        }
        None => {
            // 无游标（首次启动）：只取最近 2 个文件
            let start = md_files.len().saturating_sub(2);
            for file in &md_files[start..] {
                let display_path = format!("all/{file}");
                ranges.push(format!("`{display_path}` 全部"));
            }
        }
    }

    ranges
}
