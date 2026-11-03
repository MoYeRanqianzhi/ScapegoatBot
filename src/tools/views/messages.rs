//! `Views` 的 messages 部分。
//!
//! 由 `views/mod.rs` 的 `list` / `read` 路由调用。

use super::*;

impl Views {
    pub(super) fn list_messages_root(&self) -> Result<Vec<ViewEntry>, VfsError> {
        Ok(vec![
            dir_entry("/messages/groups"),
            dir_entry("/messages/private"),
        ])
    }

    pub(super) fn list_message_scope(&self, scope: &str) -> Result<Vec<ViewEntry>, VfsError> {
        let scope = normalize_scope(scope)?;
        let mut entries = Vec::new();
        for peer in self.messages.peers(scope).map_err(storage)? {
            entries.push(dir_entry(&format!("/messages/{scope}/{peer}")));
        }
        Ok(entries)
    }

    pub(super) fn list_message_peer(&self, scope: &str, peer: &str) -> Result<Vec<ViewEntry>, VfsError> {
        let scope = normalize_scope(scope)?;
        let chat = chat_from(scope, peer)?;
        let mut entries = Vec::new();
        for month in self.messages.months(chat).map_err(storage)? {
            entries.push(dir_entry(&format!("/messages/{scope}/{peer}/{month}")));
        }
        Ok(entries)
    }

    pub(super) fn list_message_month(
        &self,
        scope: &str,
        peer: &str,
        month: &str,
    ) -> Result<Vec<ViewEntry>, VfsError> {
        let scope = normalize_scope(scope)?;
        let chat = chat_from(scope, peer)?;
        let mut entries = Vec::new();
        for day in self.messages.days(chat, month).map_err(storage)? {
            let dd = day.rsplit('-').next().unwrap_or(&day);
            entries.push(text_entry(&format!(
                "/messages/{scope}/{peer}/{month}/{dd}.md"
            )));
        }
        Ok(entries)
    }

    pub(super) fn read_messages_digest(
        &self,
        _filter: Option<&str>,
        _limit: usize,
    ) -> Result<ViewRead, VfsError> {
        let counts = self.messages.unread_counts().map_err(storage)?;
        let mut out = String::from("# Unread messages\n");
        for (chat, count) in counts {
            out.push_str(&format!("- {chat}: {count} unread\n"));
        }
        if out.trim_end() == "# Unread messages" {
            out.push_str("- nothing unread\n");
        }
        Ok(ViewRead::Text(out))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn read_message_day(
        &self,
        scope: &str,
        peer: &str,
        month: &str,
        day: &str,
        filter: Option<&str>,
        limit: usize,
        mark_read: bool,
    ) -> Result<ViewRead, VfsError> {
        let scope = normalize_scope(scope)?;
        let chat = chat_from(scope, peer)?;
        let date = format!("{month}-{day:0>2}");
        let messages = self
            .messages
            .messages_for_day(chat, &date)
            .map_err(storage)?;
        let mut selected = Vec::new();
        for message in messages {
            if filter.is_none_or(|filter| filter_matches_message(filter, &message, None)) {
                selected.push(message);
            }
        }
        selected.truncate(limit);
        if mark_read {
            let ids: Vec<i64> = selected.iter().map(|message| message.message_id).collect();
            self.messages.mark_read(chat, &ids).map_err(storage)?;
        }
        Ok(ViewRead::Text(
            selected
                .iter()
                .map(format::render_inbound)
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }

    // ---------------------------------------------------------------------
    // /inbox
    // ---------------------------------------------------------------------
}

