//! 回合缓冲（turn buffer）。
//!
//! 设计依据：设计 §8.1「回合缓冲」与「崩溃恢复」。
//!
//! 共享 transcript 允许多个 chat 的回合并行，但工具调用与结果必须连续成对。
//! 因此 assistant / tool 块先写入本回合的 buffer，回合结束才原子提交到 transcript；
//! LLM 请求 = 已提交日志前缀 + 本回合 buffer。
//!
//! 崩溃恢复：`state = open` 的回合被重放，未配对的 `tool_use` 补一条合成错误结果
//! （“interrupted; the action may or may not have taken effect”），然后提交。

use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::agent::message::{AgentMessage, AssistantBlock, ToolResult, ToolResultMessage};
use crate::config::EncryptionMode;
use crate::storage::Database;
use crate::storage::crypto::MasterKey;

use super::SessionStore;

/// 回合状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Open,
    Committing,
    Committed,
    Aborted,
}

impl TurnState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Committing => "committing",
            Self::Committed => "committed",
            Self::Aborted => "aborted",
        }
    }
}

/// 缓冲存储。
pub struct TurnBufferStore {
    db: Arc<Database>,
    key: Arc<MasterKey>,
    encryption: EncryptionMode,
}

impl TurnBufferStore {
    pub fn new(db: Arc<Database>, key: Arc<MasterKey>, encryption: EncryptionMode) -> Self {
        Self {
            db,
            key,
            encryption,
        }
    }

    /// 追加一个块（`seq` 在回合内单调递增）。
    ///
    /// `scope` 是回合所属的 transcript 作用域（`shared` 或 `group-<id>`）；
    /// 崩溃恢复按 scope 重放，避免把 A 聊天的未提交块写进 B 的链。
    pub fn append(
        &self,
        turn_id: &str,
        seq: u64,
        scope: &str,
        message: &AgentMessage,
    ) -> Result<()> {
        let payload = serde_json::to_vec(message)?;
        let line = match self.encryption {
            EncryptionMode::XChaCha20Poly1305 => {
                let aad = format!("turn:{turn_id}:{seq}");
                let sealed = self.key.seal("turn", aad.as_bytes(), &payload)?;
                base64::engine::general_purpose::STANDARD.encode(sealed)
            }
            EncryptionMode::None => base64::engine::general_purpose::STANDARD.encode(payload),
        };
        let conn = self.db.lock();
        conn.execute(
            "INSERT INTO turn_buffer (turn_id, seq, scope, line, state)
             VALUES (?1, ?2, ?3, ?4, 'open')
             ON CONFLICT(turn_id, seq) DO UPDATE SET line = excluded.line, scope = excluded.scope",
            params![turn_id, seq as i64, scope, line],
        )?;
        Ok(())
    }

    /// 读取回合内全部块（按 seq 升序）。
    pub fn load(&self, turn_id: &str) -> Result<Vec<(u64, AgentMessage)>> {
        let rows: Vec<(i64, String)> = {
            let conn = self.db.lock();
            let mut statement = conn
                .prepare("SELECT seq, line FROM turn_buffer WHERE turn_id = ?1 ORDER BY seq ASC")?;
            statement
                .query_map(params![turn_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut messages = Vec::with_capacity(rows.len());
        for (seq, line) in rows {
            messages.push((seq as u64, self.decode_line(turn_id, seq as u64, &line)?));
        }
        Ok(messages)
    }

    /// 设置整个回合的状态。
    pub fn set_state(&self, turn_id: &str, state: TurnState) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "UPDATE turn_buffer SET state = ?1 WHERE turn_id = ?2",
            params![state.as_str(), turn_id],
        )?;
        Ok(())
    }

    /// 列出某个作用域里未完成的回合。
    pub fn open_turns(&self, scope: &str) -> Result<Vec<String>> {
        let conn = self.db.lock();
        let mut statement = conn.prepare(
            "SELECT DISTINCT turn_id FROM turn_buffer WHERE state = 'open' AND scope = ?1",
        )?;
        Ok(statement
            .query_map(params![scope], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// 存在未完成回合的作用域（启动恢复用）。
    pub fn open_scopes(&self) -> Result<Vec<String>> {
        let conn = self.db.lock();
        let mut statement = conn.prepare(
            "SELECT DISTINCT scope FROM turn_buffer WHERE state = 'open' ORDER BY scope ASC",
        )?;
        Ok(statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// 删除某个回合的缓冲（提交或放弃后）。
    pub fn clear(&self, turn_id: &str) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "DELETE FROM turn_buffer WHERE turn_id = ?1",
            params![turn_id],
        )?;
        Ok(())
    }

    /// 提交：把 buffer 追加到 transcript，再标记 committed 并清空。
    pub async fn commit(&self, sessions: &SessionStore, turn_id: &str) -> Result<usize> {
        let messages: Vec<AgentMessage> = self
            .load(turn_id)?
            .into_iter()
            .map(|(_, message)| message)
            .collect();
        if messages.is_empty() {
            self.clear(turn_id)?;
            return Ok(0);
        }
        self.set_state(turn_id, TurnState::Committing)?;
        sessions.append(&messages)?;
        self.set_state(turn_id, TurnState::Committed)?;
        self.clear(turn_id)?;
        Ok(messages.len())
    }

    /// 崩溃恢复：重放该作用域里的 open 回合，补合成结果并提交。
    pub async fn recover_open(&self, sessions: &SessionStore, scope: &str) -> Result<usize> {
        let mut recovered = 0usize;
        for turn_id in self.open_turns(scope)? {
            let mut messages: Vec<AgentMessage> = self
                .load(&turn_id)?
                .into_iter()
                .map(|(_, message)| message)
                .collect();
            messages = fill_missing_tool_results(messages);
            if !messages.is_empty() {
                sessions.append(&messages)?;
                recovered += 1;
            }
            self.set_state(&turn_id, TurnState::Committed)?;
            self.clear(&turn_id)?;
        }
        Ok(recovered)
    }

    fn decode_line(&self, turn_id: &str, seq: u64, line: &str) -> Result<AgentMessage> {
        let raw = base64::engine::general_purpose::STANDARD.decode(line)?;
        let plain = match self.encryption {
            EncryptionMode::XChaCha20Poly1305 => {
                let aad = format!("turn:{turn_id}:{seq}");
                self.key.open("turn", aad.as_bytes(), &raw)?
            }
            EncryptionMode::None => raw,
        };
        Ok(serde_json::from_slice(&plain)?)
    }
}

/// 给没有结果的工具调用补合成错误结果，保证成对。
///
/// 顺序：在最后一个 assistant 块之后追加缺失结果；如果结果已经存在则不动。
pub fn fill_missing_tool_results(messages: Vec<AgentMessage>) -> Vec<AgentMessage> {
    let mut result = Vec::with_capacity(messages.len() + 1);
    let mut index = 0;
    while index < messages.len() {
        let message = messages[index].clone();
        result.push(message.clone());

        if let AgentMessage::Assistant(assistant) = &message {
            let calls: Vec<(String, String)> = assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    AssistantBlock::ToolCall(call) => Some((call.id.clone(), call.name.clone())),
                    _ => None,
                })
                .collect();
            if calls.is_empty() {
                index += 1;
                continue;
            }

            // 收集紧随其后的 tool 结果。
            let mut following = Vec::new();
            let mut cursor = index + 1;
            while cursor < messages.len() {
                if let AgentMessage::Tool(tool) = &messages[cursor] {
                    following.push(tool.clone());
                    cursor += 1;
                } else {
                    break;
                }
            }

            for (id, name) in calls {
                if !following.iter().any(|tool| tool.tool_call_id == id) {
                    following.push(ToolResultMessage {
                        tool_call_id: id,
                        tool_name: name,
                        content: "interrupted; the action may or may not have taken effect"
                            .to_owned(),
                        is_error: true,
                    });
                }
            }
            for tool in following {
                result.push(AgentMessage::Tool(tool));
            }
            index = cursor;
            continue;
        }
        index += 1;
    }
    result
}

/// 便捷：把工具结果包成消息。
pub fn tool_message(call_id: &str, name: &str, result: ToolResult) -> AgentMessage {
    AgentMessage::Tool(ToolResultMessage {
        tool_call_id: call_id.to_owned(),
        tool_name: name.to_owned(),
        content: result.content,
        is_error: result.is_error,
    })
}

/// 回合 id：时间戳 + 进程内自增，便于排序与排错。
pub fn new_turn_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ"),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// 包装成上下文错误的读取，便于调用方补上下文。
pub fn load_context(turn_id: &str, store: &TurnBufferStore) -> Result<Vec<(u64, AgentMessage)>> {
    store
        .load(turn_id)
        .with_context(|| format!("cannot load turn buffer {turn_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::message::{AssistantMessage, StopReason, Usage};

    fn setup() -> (TurnBufferStore, SessionStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([4u8; 32]));
        let sessions = SessionStore::open(
            Arc::clone(&db),
            Arc::clone(&key),
            dir.path().to_path_buf(),
            0,
            EncryptionMode::XChaCha20Poly1305,
        )
        .unwrap();
        let buffer = TurnBufferStore::new(db, key, EncryptionMode::XChaCha20Poly1305);
        (buffer, sessions, dir)
    }

    fn assistant_with_call(id: &str) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![AssistantBlock::ToolCall(crate::agent::message::ToolCall {
                id: id.to_owned(),
                name: "Read".to_owned(),
                arguments: serde_json::json!({"path": "/AGENTS.md"}),
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            refusal: None,
        })
    }

    #[tokio::test]
    async fn buffer_roundtrip_and_commit() {
        let (buffer, sessions, _dir) = setup();
        let turn = "t1";
        buffer.append(turn, 0, "shared", &AgentMessage::user("hi")).unwrap();
        buffer.append(turn, 1, "shared", &assistant_with_call("c1")).unwrap();
        assert_eq!(buffer.load(turn).unwrap().len(), 2);
        assert_eq!(buffer.open_turns("shared").unwrap(), vec![turn.to_owned()]);

        let committed = buffer.commit(&sessions, turn).await.unwrap();
        assert_eq!(committed, 2);
        assert!(buffer.load(turn).unwrap().is_empty());
        assert!(buffer.open_turns("shared").unwrap().is_empty());

        let current = sessions.current().unwrap();
        assert_eq!(sessions.read_segment(&current.file).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn recovery_fills_missing_tool_results() {
        let (buffer, sessions, _dir) = setup();
        let turn = "t2";
        buffer.append(turn, 0, "shared", &AgentMessage::user("go")).unwrap();
        buffer.append(turn, 1, "shared", &assistant_with_call("c1")).unwrap();
        // 故意不写工具结果。

        let recovered = buffer.recover_open(&sessions, "shared").await.unwrap();
        assert_eq!(recovered, 1);

        let current = sessions.current().unwrap();
        let messages = sessions.read_segment(&current.file).unwrap();
        assert_eq!(messages.len(), 3);
        match &messages[2] {
            AgentMessage::Tool(tool) => {
                assert!(tool.is_error);
                assert_eq!(tool.tool_call_id, "c1");
                assert!(tool.content.contains("interrupted"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn recovery_only_replays_the_matching_scope() {
        let (buffer, sessions, _dir) = setup();
        buffer.append("ta", 0, "group-1", &AgentMessage::user("from group one")).unwrap();
        buffer.append("tb", 0, "group-2", &AgentMessage::user("from group two")).unwrap();

        let one = sessions.for_scope("group-1");
        assert_eq!(buffer.recover_open(&one, "group-1").await.unwrap(), 1);
        // group-2 的回合还开着，不会被写进 group-1。
        assert_eq!(buffer.open_turns("group-2").unwrap(), vec!["tb".to_owned()]);
        let current = one.current().unwrap();
        let messages = one.read_segment(&current.file).unwrap();
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            AgentMessage::User { text } => assert!(text.contains("group one")),
            other => panic!("unexpected {other:?}"),
        }

        let two = sessions.for_scope("group-2");
        assert_eq!(buffer.recover_open(&two, "group-2").await.unwrap(), 1);
        assert!(buffer.open_scopes().unwrap().is_empty());
    }

    #[test]
    fn buffer_lines_are_encrypted() {
        let (buffer, _sessions, _dir) = setup();
        buffer
            .append("t3", 0, "shared", &AgentMessage::user("secret-marker"))
            .unwrap();
        let conn = buffer.db.lock();
        let line: String = conn
            .query_row(
                "SELECT line FROM turn_buffer WHERE turn_id = 't3'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!line.contains("secret-marker"));
    }

    #[test]
    fn fill_missing_does_not_duplicate_existing_results() {
        let messages = vec![
            AgentMessage::user("go"),
            assistant_with_call("c1"),
            tool_message("c1", "Read", ToolResult::ok("done")),
        ];
        let filled = fill_missing_tool_results(messages);
        assert_eq!(filled.len(), 3);
    }
}
