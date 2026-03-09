// src/storage/cursor.rs
// 消息游标系统
// 追踪每个会话最后被 AI 处理的消息位置
// AI 启动时读取游标，计算未处理的消息范围

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 单个会话的处理游标
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCursor {
    /// 最后处理的 MD 文件名（相对于会话目录）
    pub last_file: String,
    /// 最后处理的消息 ID
    pub last_message_id: i64,
}

/// 保存游标到文件
pub fn save_cursor(
    cursors_dir: &Path,
    session_key: &str,
    cursor: &SessionCursor,
) -> Result<()> {
    // 确保游标目录存在
    std::fs::create_dir_all(cursors_dir)
        .with_context(|| format!("创建游标目录失败: {}", cursors_dir.display()))?;
    let path = cursors_dir.join(format!("{session_key}.json"));
    let json = serde_json::to_string_pretty(cursor)?;
    std::fs::write(&path, json)
        .with_context(|| format!("写入游标失败: {}", path.display()))?;
    Ok(())
}

/// 读取游标
pub fn load_cursor(
    cursors_dir: &Path,
    session_key: &str,
) -> Result<Option<SessionCursor>> {
    let path = cursors_dir.join(format!("{session_key}.json"));
    if !path.exists() {
        return Ok(None);
    }
    let json = std::fs::read_to_string(&path)
        .with_context(|| format!("读取游标失败: {}", path.display()))?;
    let cursor: SessionCursor = serde_json::from_str(&json)?;
    Ok(Some(cursor))
}

/// 计算指定会话目录下，从游标位置之后的未处理消息范围
/// 返回人类可读的描述，如 "messages/group/123456/2026-03-09_001.md 从 ID 789 到最新"
/// 如果没有游标（首次启动），则返回最近的一些文件
pub fn compute_unprocessed_range(
    messages_dir: &Path,
    session_dir_relative: &str,
    cursor: Option<&SessionCursor>,
) -> Result<Vec<String>> {
    let session_dir = messages_dir.join(session_dir_relative);
    if !session_dir.exists() {
        return Ok(vec![]);
    }

    // 收集所有 .md 文件并排序
    let mut md_files: Vec<String> = std::fs::read_dir(&session_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
        .collect();
    md_files.sort();

    if md_files.is_empty() {
        return Ok(vec![]);
    }

    let mut ranges = Vec::new();

    match cursor {
        Some(cur) => {
            // 有游标：从游标文件（含）开始，到最新文件
            let start_idx = md_files
                .iter()
                .position(|f| f == &cur.last_file)
                .unwrap_or(0);
            for file in &md_files[start_idx..] {
                let display_path = format!("{session_dir_relative}/{file}");
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
                let display_path = format!("{session_dir_relative}/{file}");
                ranges.push(format!("`{display_path}` 全部"));
            }
        }
    }

    Ok(ranges)
}

/// 扫描 messages/ 目录下所有会话，结合游标计算全部未处理消息范围
pub fn compute_all_unprocessed(messages_dir: &Path) -> Vec<String> {
    let mut all_ranges = Vec::new();

    // 遍历 group/ 和 private/ 子目录
    for session_type in &["group", "private"] {
        let type_dir = messages_dir.join(session_type);
        if !type_dir.exists() {
            continue;
        }
        // 获取游标目录
        let cursors_dir = messages_dir.join(".cursors");

        // 遍历每个会话目录
        if let Ok(entries) = std::fs::read_dir(&type_dir) {
            let mut session_ids: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                .collect();
            session_ids.sort();

            for session_id in &session_ids {
                let session_key = format!("{session_type}_{session_id}");
                let relative = format!("{session_type}/{session_id}");
                let cursor = load_cursor(&cursors_dir, &session_key)
                    .ok()
                    .flatten();

                if let Ok(ranges) = compute_unprocessed_range(
                    messages_dir,
                    &relative,
                    cursor.as_ref(),
                ) {
                    all_ranges.extend(ranges);
                }
            }
        }
    }

    all_ranges
}
