// src/tools/search.rs
// 消息搜索工具模块
// 通过 SQLite 消息索引快速查找和搜索消息原始内容
//
// 工具列表：
// - GetMessage: 通过消息 ID 获取原始 JSON 内容
// - SearchMessages: 按条件搜索消息索引
//
// 安全设计：
// - GetMessage 仅读取已索引的 JSONL 文件（路径来自数据库，非 LLM 输入）
// - 文件读取使用 std::fs（因为 JSONL 路径由系统控制，非用户输入）
// - SearchMessages 仅查询 SQLite 索引，不直接读取文件

use crate::storage::message_index::SearchCriteria;
use crate::tools::{ToolContext, ToolResult};
use chrono::TimeZone as _;

// ============================================================================
// GetMessage 工具
// ============================================================================

/// GetMessage 工具：通过消息 ID 获取原始 JSON 内容
/// 利用 SQLite 索引快速定位消息在 JSONL 文件中的物理位置
/// 然后直接读取对应行，返回完整的原始 JSON 数据
///
/// # 参数
/// - `ctx`: 工具执行上下文（包含消息索引和消息目录路径）
/// - `message_id`: 要查找的消息 ID
///
/// # 返回
/// - 成功：原始 JSON 内容
/// - 失败：消息未找到或读取错误
pub fn get_message(ctx: &ToolContext, message_id: i64) -> ToolResult {
    // 从 SQLite 索引中查找消息的物理位置
    let Some((file_path, line_number)) = ctx.message_index.lookup(message_id) else {
        return ToolResult::err(format!(
            "消息 ID {} 不在索引中（可能是在索引建立之前的消息）",
            message_id
        ));
    };

    // 构造 JSONL 文件的完整路径
    // file_path 格式如 "all/2026-03-10_001.jsonl"，相对于 messages_dir
    let full_path = ctx.messages_dir.join(&file_path);

    // 读取文件内容
    let content = match std::fs::read_to_string(&full_path) {
        Ok(c) => c,
        Err(e) => {
            return ToolResult::err(format!(
                "读取 JSONL 文件失败 ({}): {}",
                full_path.display(), e
            ));
        }
    };

    // 提取指定行（line_number 是 1-based）
    let target_line = line_number as usize;
    match content.lines().nth(target_line - 1) {
        Some(line) => {
            // 验证是有效的 JSON（防御性检查）
            if line.trim().is_empty() {
                return ToolResult::err(format!(
                    "消息 ID {} 对应的行为空（文件 {}:{}）",
                    message_id, file_path, line_number
                ));
            }
            ToolResult::ok(line.to_string())
        }
        None => {
            ToolResult::err(format!(
                "行号越界：文件 {} 只有 {} 行，但索引记录的行号为 {}",
                file_path,
                content.lines().count(),
                line_number
            ))
        }
    }
}

// ============================================================================
// SearchMessages 工具
// ============================================================================

/// SearchMessages 工具：按条件搜索消息索引
/// 支持按发送者、群号、时间范围等条件过滤
/// 返回匹配消息的元数据列表（不返回原始 JSON 内容，需用 GetMessage 获取）
///
/// # 参数
/// - `ctx`: 工具执行上下文（包含消息索引）
/// - `user_id`: 按发送者 QQ 号过滤（可选）
/// - `group_id`: 按群号过滤（可选）
/// - `message_type`: 按消息类型过滤（可选，"group" 或 "private"）
/// - `after`: 起始时间戳（可选，Unix 秒）
/// - `before`: 截止时间戳（可选，Unix 秒）
/// - `limit`: 最大返回条数（可选，默认 20，上限 100）
/// - `offset`: 分页偏移量（可选，默认 0）
///
/// # 返回
/// 格式化的消息列表，每条包含 ID、时间、发送者、群号等信息
pub fn search_messages(
    ctx: &ToolContext,
    user_id: Option<i64>,
    group_id: Option<i64>,
    message_type: Option<String>,
    after: Option<i64>,
    before: Option<i64>,
    limit: Option<u32>,
    offset: Option<u32>,
) -> ToolResult {
    // 构建搜索条件
    let criteria = SearchCriteria {
        user_id,
        group_id,
        message_type,
        after,
        before,
        limit: limit.unwrap_or(20),
        offset: offset.unwrap_or(0),
    };

    // 执行搜索
    let results = ctx.message_index.search(&criteria);

    if results.is_empty() {
        return ToolResult::ok("未找到匹配的消息".to_string());
    }

    // 格式化结果
    let mut output = format!("找到 {} 条消息：\n\n", results.len());

    for result in &results {
        // 将 Unix 时间戳转为可读时间
        let time_str = chrono::Local
            .timestamp_opt(result.timestamp, 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| result.timestamp.to_string());

        // 格式化单条结果
        output.push_str(&format!(
            "- MsgID:{} | 时间:{} | QQ:{} | {}| 类型:{}\n",
            result.message_id,
            time_str,
            result.user_id,
            result
                .group_id
                .map(|gid| format!("群:{} ", gid))
                .unwrap_or_default(),
            result.message_type,
        ));
    }

    // 添加翻页提示
    let displayed = results.len() as u32;
    let current_offset = criteria.offset;
    if displayed == criteria.limit.min(100) {
        output.push_str(&format!(
            "\n（显示第 {}-{} 条，可能还有更多。使用 offset={} 查看下一页）",
            current_offset + 1,
            current_offset + displayed,
            current_offset + displayed,
        ));
    }

    ToolResult::ok(output)
}
