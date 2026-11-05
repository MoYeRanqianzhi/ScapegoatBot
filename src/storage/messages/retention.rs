use super::*;

impl MessageStore {
    /// 按年龄与配额清理最旧的完整月份，绝不删除当前月份。
    pub(crate) fn enforce_retention(&self, retention_days: u32) -> Result<CleanupReport> {
        let mut report = CleanupReport::default();
        let current_month = Local::now().format("%Y-%m").to_string();
        let cutoff_month = if retention_days == 0 {
            None
        } else {
            let cutoff = Local::now() - chrono::Duration::days(i64::from(retention_days));
            Some(cutoff.format("%Y-%m").to_string())
        };

        loop {
            let (total_bytes, _) = self.usage()?;
            let over = self.max_bytes != 0 && total_bytes > self.max_bytes;

            let month: Option<(String, u64, u64)> = {
                let conn = self.db.lock();
                conn.query_row(
                    "SELECT month, SUM(bytes), SUM(count) FROM message_files
                     WHERE month != ?1
                     GROUP BY month ORDER BY month ASC LIMIT 1",
                    params![current_month],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
            };

            let Some((month, bytes, count)) = month else {
                break;
            };
            let expired = cutoff_month
                .as_ref()
                .is_some_and(|cutoff| month.as_str() < cutoff.as_str());
            if !over && !expired {
                break;
            }

            let files: Vec<String> = {
                let conn = self.db.lock();
                let mut statement =
                    conn.prepare("SELECT path FROM message_files WHERE month = ?1")?;
                statement
                    .query_map(params![month], |row| row.get(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };
            for file in &files {
                let path = self.root.join(file);
                if let Err(error) = std::fs::remove_file(&path) {
                    warn!(%error, path = %path.display(), "cannot remove expired message file");
                }
            }
            let conn = self.db.lock();
            conn.execute(
                "DELETE FROM messages WHERE day LIKE ?1",
                params![format!("{month}-%")],
            )?;
            conn.execute("DELETE FROM message_files WHERE month = ?1", params![month])?;
            // 消息行已删，收件箱状态不能留下孤儿，否则未读数会永久虚高。
            conn.execute(
                "DELETE FROM inbox_state WHERE NOT EXISTS (
                     SELECT 1 FROM messages m
                     WHERE m.chat = inbox_state.chat AND m.message_id = inbox_state.message_id
                 )",
                [],
            )?;
            report.removed_files += files.len() as u64;
            report.removed_messages += count;
            report.freed_bytes += bytes;
        }
        Ok(report)
    }

    /// 写入前腾出空间。
    pub(super) fn make_room(&self) -> Result<()> {
        // 先按年龄清理；`retention_days = 0` 时不做任何事。
        if self.retention_days > 0 {
            self.enforce_retention(self.retention_days)?;
        }
        if self.max_bytes == 0 {
            return Ok(());
        }
        let (bytes, _) = self.usage()?;
        if bytes <= self.max_bytes {
            return Ok(());
        }
        let report = self.enforce_retention(self.retention_days)?;
        let (bytes, _) = self.usage()?;
        if bytes > self.max_bytes {
            warn!(
                bytes,
                limit = self.max_bytes,
                removed = report.removed_files,
                "message store still over quota after cleanup"
            );
        }
        Ok(())
    }
}
