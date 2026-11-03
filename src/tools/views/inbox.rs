//! `Views` 的 inbox 部分。
//!
//! 由 `views/mod.rs` 的 `list` / `read` 路由调用。

use super::*;

impl Views {
    pub(super) fn list_inbox_root(&self) -> Result<Vec<ViewEntry>, VfsError> {
        let mut entries = Vec::new();
        for (chat, count) in self.messages.unread_counts().map_err(storage)? {
            entries.push(ViewEntry {
                path: format!("/inbox/{chat}"),
                kind: "dir",
                size: count,
                mtime: String::new(),
            });
        }
        Ok(entries)
    }

    pub(super) fn list_inbox_chat(&self, chat: &str) -> Result<Vec<ViewEntry>, VfsError> {
        let chat = parse_chat_segment(chat)?;
        let mut entries = vec![text_entry(&format!("/inbox/group:{}/index.md", chat.id()))];
        if !chat.is_group() {
            entries[0] = text_entry(&format!("/inbox/private:{}/index.md", chat.id()));
        }
        let prefix = if chat.is_group() { "group" } else { "private" };
        for message in self
            .messages
            .inbox_for_chat(chat, InboxState::Unread)
            .map_err(storage)?
        {
            entries.push(text_entry(&format!(
                "/inbox/{prefix}:{}/{}.md",
                chat.id(),
                message.message_id
            )));
        }
        Ok(entries)
    }

    pub(super) fn read_inbox_root(
        &self,
        filter: Option<&str>,
        limit: usize,
        mark_read: bool,
    ) -> Result<ViewRead, VfsError> {
        let mut out = String::from("# Inbox\n");
        let mut rendered = 0usize;
        for message in self.messages.unread(None).map_err(storage)? {
            if rendered >= limit {
                break;
            }
            if filter.is_some_and(|filter| {
                !filter_matches_message(filter, &message, Some(InboxState::Unread))
            }) {
                continue;
            }
            out.push_str(&format::render_inbound(&message));
            out.push_str("\n\n");
            if mark_read {
                // 内容进入上下文即已读。
                self.messages
                    .mark_read(message.chat, &[message.message_id])
                    .map_err(storage)?;
            }
            rendered += 1;
        }
        if rendered == 0 {
            out.push_str("Nothing unread.\n");
        }
        Ok(ViewRead::Text(out))
    }

    pub(super) fn read_inbox_chat(
        &self,
        chat: &str,
        filter: Option<&str>,
        limit: usize,
        mark_read: bool,
    ) -> Result<ViewRead, VfsError> {
        let chat = parse_chat_segment(chat)?;
        let messages = self
            .messages
            .inbox_for_chat(chat, InboxState::Unread)
            .map_err(storage)?;
        let mut out = format!("# Inbox: {chat}\n");
        let mut selected = Vec::new();
        for message in messages {
            if selected.len() >= limit {
                break;
            }
            if filter.is_some_and(|filter| {
                !filter_matches_message(filter, &message, Some(InboxState::Unread))
            }) {
                continue;
            }
            selected.push(message.message_id);
            out.push_str(&format::render_inbound(&message));
            out.push_str("\n\n");
        }
        if selected.is_empty() {
            out.push_str("Nothing unread.\n");
        }
        if mark_read {
            self.messages.mark_read(chat, &selected).map_err(storage)?;
        }
        Ok(ViewRead::Text(out))
    }

    pub(super) fn read_inbox_message(
        &self,
        chat: &str,
        message_id: i64,
        mark_read: bool,
    ) -> Result<ViewRead, VfsError> {
        let chat = parse_chat_segment(chat)?;
        let message = self
            .messages
            .load(chat, message_id)
            .map_err(storage)?
            .ok_or_else(|| VfsError::NotFound(format!("/inbox/{chat}/{message_id}.md")))?;
        if mark_read {
            self.messages
                .mark_read(chat, &[message_id])
                .map_err(storage)?;
        }
        Ok(ViewRead::Text(format::render_inbound(&message)))
    }

    // ---------------------------------------------------------------------
    // /sessions
    // ---------------------------------------------------------------------
}
