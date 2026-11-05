use super::*;

impl MessageStore {
    /// 发送账本：登记意图（发送前）。
    pub(crate) fn begin_send(&self, id: &str, chat: ChatId, intent: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self.db.lock();
        conn.execute(
            "INSERT INTO outbound_intents (id, chat, intent, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'pending', ?4, ?4)
             ON CONFLICT(id) DO NOTHING",
            params![id, chat.to_string(), intent, now],
        )?;
        Ok(())
    }

    /// 发送账本：记录结果。
    pub(crate) fn finish_send(
        &self,
        id: &str,
        status: &str,
        message_id: Option<i64>,
    ) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "UPDATE outbound_intents
             SET status = ?1, message_id = ?2, updated_at = ?3
             WHERE id = ?4",
            params![status, message_id, Utc::now().to_rfc3339(), id],
        )?;
        Ok(())
    }

    /// 启动核对：超过 `age_secs` 仍是 `pending` 的发送标记为 `unknown`。
    ///
    /// 崩溃/超时可能发生在“对方已收到、本地没记下 message_id”之间；不能当作
    /// 失败自动重试，先标记为不确定，交给 agent 查历史后决定。
    pub(crate) fn mark_stale_intents(&self, age_secs: u64) -> Result<u64> {
        let cutoff = (Utc::now() - chrono::Duration::seconds(age_secs as i64)).to_rfc3339();
        let conn = self.db.lock();
        let changed = conn.execute(
            "UPDATE outbound_intents SET status = 'unknown', updated_at = ?1
             WHERE status = 'pending' AND created_at < ?2",
            params![Utc::now().to_rfc3339(), cutoff],
        )?;
        Ok(changed as u64)
    }

    /// 未确认的发送（崩溃恢复时先查它们）。
    pub(crate) fn unconfirmed_sends(&self) -> Result<Vec<(String, ChatId, String)>> {
        let conn = self.db.lock();
        let mut statement = conn.prepare(
            "SELECT id, chat, intent FROM outbound_intents
             WHERE status IN ('pending', 'unknown') ORDER BY created_at ASC",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, chat, intent)| ChatId::parse(&chat).map(|chat| (id, chat, intent)))
            .collect())
    }
}
