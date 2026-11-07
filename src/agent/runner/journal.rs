//! 回合日志：把本回合并入上下文的块实时写进 turn buffer。
//!
//! 以前块只在回合结束、提交之前才一次性写入 buffer，进程在回合中途退出（崩溃、
//! 关机超时）就丢掉整个回合——包括已经执行过、对外产生过效果的工具调用。
//! 现在模型循环每并入一块就发一个 [`AgentEvent::Appended`]，日志在事件任务里
//! 立刻写入；提交时只补写循环之后才加入的块（Stop hook 等），再整体提交。
//! 崩溃恢复（`TurnBufferStore::recover_open`）会补齐未配对的工具结果。

use std::sync::Arc;

use anyhow::Result;
use tracing::warn;

use crate::session::SessionStore;
use crate::session::turn_buffer::TurnBufferStore;
use crate::transcript::AgentMessage;

/// transcript 作用域；全局单会话只有一个。
const SCOPE: &str = "shared";

/// 一个回合的日志。
pub(super) struct TurnJournal {
    buffer: Arc<TurnBufferStore>,
    turn_id: String,
    /// 开头不落盘的块数（runtime / pending 提示）。
    skip: usize,
    /// 已经看到的块数（含跳过的）。
    seen: usize,
    /// 已经写进 buffer 的块数；写入失败后不再增长，提交时从这里补写。
    recorded: usize,
    /// 写入失败过：之后的块都留给提交时补写，保证 seq 连续。
    broken: bool,
}

impl TurnJournal {
    pub(super) fn new(buffer: Arc<TurnBufferStore>, turn_id: String, skip: usize) -> Self {
        Self {
            buffer,
            turn_id,
            skip,
            seen: 0,
            recorded: 0,
            broken: false,
        }
    }

    /// 本回合的 id。
    pub(super) fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// 记录一块（按并入上下文的顺序调用）。
    pub(super) fn record(&mut self, message: &AgentMessage) {
        self.seen += 1;
        if self.seen <= self.skip || self.broken {
            return;
        }
        match self
            .buffer
            .append(&self.turn_id, self.recorded as u64, SCOPE, message)
        {
            Ok(()) => self.recorded += 1,
            Err(error) => {
                warn!(%error, turn = %self.turn_id, "cannot write a block to the turn buffer");
                self.broken = true;
            }
        }
    }

    /// 提交：`blocks` 是本回合全部要落盘的块（不含跳过的提示），按顺序。
    ///
    /// 已记录的前缀不重复写；循环之后加入的块与写入失败后的块在这里补写。
    pub(super) async fn commit(
        self,
        sessions: &SessionStore,
        blocks: &[AgentMessage],
    ) -> Result<()> {
        if blocks.is_empty() && self.recorded == 0 {
            return Ok(());
        }
        for (seq, message) in blocks.iter().enumerate().skip(self.recorded) {
            self.buffer
                .append(&self.turn_id, seq as u64, SCOPE, message)?;
        }
        self.buffer.commit(sessions, &self.turn_id).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EncryptionMode;
    use crate::storage::Database;
    use crate::storage::crypto::MasterKey;

    fn stores(dir: &std::path::Path) -> (Arc<TurnBufferStore>, SessionStore) {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([5u8; 32]));
        let buffer = Arc::new(TurnBufferStore::new(
            Arc::clone(&db),
            Arc::clone(&key),
            EncryptionMode::XChaCha20Poly1305,
        ));
        let sessions = SessionStore::open(
            db,
            key,
            dir.join("data"),
            0,
            EncryptionMode::XChaCha20Poly1305,
        )
        .unwrap();
        (buffer, sessions)
    }

    /// 回合中途退出：已经并入上下文的块在重启恢复时回到 transcript。
    #[tokio::test]
    async fn blocks_survive_an_exit_in_the_middle_of_a_turn() {
        let dir = tempfile::tempdir().unwrap();
        let (buffer, sessions) = stores(dir.path());
        let mut journal = TurnJournal::new(Arc::clone(&buffer), "t1".to_owned(), 1);
        journal.record(&AgentMessage::user("runtime context"));
        journal.record(&AgentMessage::user("tool happened"));
        // 没有提交就“退出”。
        drop(journal);
        assert_eq!(buffer.recover_open(&sessions).await.unwrap(), 1);
        let current = sessions.current().unwrap();
        let transcript = sessions.read_segment(&current.file).unwrap();
        assert_eq!(transcript, vec![AgentMessage::user("tool happened")]);
    }

    /// 提交只补写日志之后的块，不重复已记录的前缀。
    #[tokio::test]
    async fn commit_appends_only_the_unrecorded_tail() {
        let dir = tempfile::tempdir().unwrap();
        let (buffer, sessions) = stores(dir.path());
        let mut journal = TurnJournal::new(Arc::clone(&buffer), "t2".to_owned(), 0);
        let blocks = vec![AgentMessage::user("a"), AgentMessage::system("stop hook")];
        journal.record(&blocks[0]);
        journal.commit(&sessions, &blocks).await.unwrap();
        let current = sessions.current().unwrap();
        assert_eq!(sessions.read_segment(&current.file).unwrap(), blocks);
        assert!(buffer.open_turns().unwrap().is_empty());
    }
}
