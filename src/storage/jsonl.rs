// src/storage/jsonl.rs
// JSONL 原始消息存储
// 将收到的原始 JSON 事件按行追加到 .jsonl 文件中
// 每行一个完整的 JSON 对象，保留所有原始信息

use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// 将一个 JSON Value 追加到 JSONL 文件末尾
/// 文件不存在时自动创建
pub fn append_jsonl(path: &Path, value: &serde_json::Value) -> Result<()> {
    // 将 JSON 序列化为紧凑的单行格式（不含换行符）
    let line = serde_json::to_string(value)
        .context("序列化 JSON 失败")?;

    // 以追加模式打开文件
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("打开 JSONL 文件失败: {}", path.display()))?;

    // 写入一行 JSON + 换行符
    writeln!(file, "{line}")
        .with_context(|| format!("写入 JSONL 文件失败: {}", path.display()))?;

    Ok(())
}
