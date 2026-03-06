// src/storage/markdown.rs
// Markdown 格式消息存储
// 将消息解析后以结构化的 Markdown 格式追加到 .md 文件
// 格式设计用于 AI 理解消息上下文

use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

// ============================================================================
// 消息记录结构
// ============================================================================

/// 一条格式化消息记录，包含所有 AI 需要理解的元数据
#[derive(Debug, Clone)]
pub struct MessageRecord {
    /// 消息时间（HH:MM:SS 格式）
    pub time: String,
    /// 消息 ID
    pub message_id: i64,
    /// 消息类型（Group / Private / FriendRequest / GroupRequest）
    pub message_type: String,
    /// 群信息（仅群消息有值）: "群名 (群号)"
    pub group_info: Option<String>,
    /// 发送者 QQ 号
    pub qq: i64,
    /// 发送者昵称
    pub nick: String,
    /// 群昵称/群名片（仅群消息有值）
    pub group_nick: Option<String>,
    /// 备注名称（仅好友有值）
    pub remark: Option<String>,
    /// 身份描述（Friend / GroupMember (群号列表) / Stranger）
    pub identity: String,
    /// 消息状态（Raw / Wrapped）
    pub state: String,
    /// 消息内容（已经过 CQ XML 格式化的 <message>...</message>）
    pub content: String,
}

// ============================================================================
// MD 追加写入
// ============================================================================

/// 将一条消息记录追加到 MD 文件末尾
/// 格式:
/// ```markdown
/// ### Time HH:MM:SS
/// - ID: xxx
/// - Type: Group
/// - Group: 群名 (群号)
/// - QQ: 123456
/// - Nick: ABC
/// - GroupNick: ABC的群名片
/// - Remark: 备注
/// - Identity: Friend, GroupMember (123456, 789012)
/// - State: Raw
///
/// <message>内容</message>
///
/// ---
/// ```
pub fn append_markdown(path: &Path, record: &MessageRecord) -> Result<()> {
    let mut out = String::new();

    // 消息头部：时间
    out.push_str(&format!("### Time {}\n", record.time));
    // 消息 ID
    out.push_str(&format!("- ID: {}\n", record.message_id));
    // 消息类型
    out.push_str(&format!("- Type: {}\n", record.message_type));

    // 群信息（仅群消息）
    if let Some(group_info) = &record.group_info {
        out.push_str(&format!("- Group: {group_info}\n"));
    }

    // 发送者 QQ
    out.push_str(&format!("- QQ: {}\n", record.qq));
    // 发送者昵称
    out.push_str(&format!("- Nick: {}\n", record.nick));

    // 群昵称（仅群消息且有群名片时）
    if let Some(group_nick) = &record.group_nick {
        out.push_str(&format!("- GroupNick: {group_nick}\n"));
    }

    // 备注名称（仅好友有备注时）
    if let Some(remark) = &record.remark {
        out.push_str(&format!("- Remark: {remark}\n"));
    }

    // 身份信息
    out.push_str(&format!("- Identity: {}\n", record.identity));
    // 消息状态
    out.push_str(&format!("- State: {}\n", record.state));

    // 空行 + 消息内容
    out.push('\n');
    out.push_str(&record.content);
    out.push('\n');

    // 分隔线
    out.push_str("\n---\n");

    // 以追加模式写入文件
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("打开 MD 文件失败: {}", path.display()))?;

    file.write_all(out.as_bytes())
        .with_context(|| format!("写入 MD 文件失败: {}", path.display()))?;

    Ok(())
}
