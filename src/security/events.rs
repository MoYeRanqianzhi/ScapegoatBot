//! 安全事件审计。
//!
//! 设计依据：设计 §5.9 / §5.10。
//!
//! 每次闸判定都写一条：位置、是否放行、分数、后端、原因、时间。
//! `shadow` 期靠这批数据校准阈值；`enforce` 期靠它解释为什么停机。
//! 事实由 runtime 写，agent 无法修改（模型没有写这张表的入口）。

use std::sync::Arc;

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::storage::Database;

/// 闸位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Checkpoint {
    /// 模型输入前。
    Input,
    /// 模型输出后、副作用前。
    Output,
    /// 单次 `napcat.*` 调用的门禁（`[gate]`）。
    Gate,
}

impl Checkpoint {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
            Self::Gate => "gate",
        }
    }
}

/// 一条安全事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityEvent {
    pub id: i64,
    pub checkpoint: Checkpoint,
    pub allowed: bool,
    pub score: f32,
    pub backend: String,
    pub reason: String,
    pub created_at: String,
}

/// 追加一条事件，返回自增 id。
pub fn record(
    db: &Arc<Database>,
    checkpoint: Checkpoint,
    allowed: bool,
    score: f32,
    backend: &str,
    reason: &str,
) -> anyhow::Result<i64> {
    let conn = db.lock();
    conn.execute(
        "INSERT INTO security_events
            (checkpoint, allowed, score, backend, reason, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            checkpoint.as_str(),
            allowed as i32,
            score,
            backend,
            reason,
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 最近 `limit` 条事件，按时间倒序。
pub fn recent(db: &Arc<Database>, limit: u32) -> anyhow::Result<Vec<SecurityEvent>> {
    let conn = db.lock();
    let mut statement = conn.prepare(
        "SELECT id, checkpoint, allowed, score, backend, reason, created_at
         FROM security_events ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = statement
        .query_map(params![limit], |row| {
            let checkpoint: String = row.get(1)?;
            Ok(SecurityEvent {
                id: row.get(0)?,
                checkpoint: match checkpoint.as_str() {
                    "output" => Checkpoint::Output,
                    "gate" => Checkpoint::Gate,
                    _ => Checkpoint::Input,
                },
                allowed: row.get::<_, i32>(2)? != 0,
                score: row.get::<_, f64>(3)? as f32,
                backend: row.get(4)?,
                reason: row.get(5)?,
                created_at: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 危险判定的条数（用于 `sbot security status` 与校准）。
pub fn count_blocked(db: &Arc<Database>) -> anyhow::Result<u64> {
    let conn = db.lock();
    let count: u64 = conn.query_row(
        "SELECT COUNT(*) FROM security_events WHERE allowed = 0",
        [],
        |row| row.get(0),
    )?;
    Ok(count)
}
