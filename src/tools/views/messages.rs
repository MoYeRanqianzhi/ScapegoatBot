//! `Views` 的 messages 部分：未读摘要与按天的消息文件。
//!
//! 由 `views.rs` 的 `list` / `read` 路由调用。筛选与分页见 `views/filter.rs`。

use std::collections::BTreeMap;

use super::filter;
use super::*;
use crate::vfs::path::ViewQuery;

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
            let chat = chat_from(scope, &peer.to_string())?;
            if self.receive_allows(chat) {
                entries.push(dir_entry(&format!("/messages/{scope}/{peer}")));
            }
        }
        Ok(entries)
    }

    pub(super) fn list_message_peer(
        &self,
        scope: &str,
        peer: &str,
    ) -> Result<Vec<ViewEntry>, VfsError> {
        let scope = normalize_scope(scope)?;
        let chat = chat_from(scope, peer)?;
        self.ensure_receive(chat)?;
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
        self.ensure_receive(chat)?;
        let mut entries = Vec::new();
        for day in self.messages.days(chat, month).map_err(storage)? {
            let dd = day.rsplit('-').next().unwrap_or(&day);
            entries.push(text_entry(&format!(
                "/messages/{scope}/{peer}/{month}/{dd}.md"
            )));
        }
        Ok(entries)
    }

    /// `/messages`：每个 chat 的未读数。
    ///
    /// 带 `filter` 时只数满足条件的未读消息（例如 `from:<qq>` 得到这个人在各处的未读）；
    /// 分页作用于 chat 列表。
    pub(super) fn read_messages_digest(&self, query: &ViewQuery) -> Result<ViewRead, VfsError> {
        let counts: Vec<(ChatId, u64)> = match &query.filter {
            // 没有条件：直接用索引里的计数，不加载消息。
            None => self
                .messages
                .unread_counts()
                .map_err(storage)?
                .into_iter()
                .filter(|(chat, _)| self.receive_allows(*chat))
                .collect(),
            Some(filter) => {
                // 按 chat 的字符串形式排序，与索引计数的 `ORDER BY chat` 一致。
                let mut counts: BTreeMap<String, (ChatId, u64)> = BTreeMap::new();
                for message in self.messages.unread(None).map_err(storage)? {
                    if self.receive_allows(message.chat)
                        && filter::matches_message(
                            filter,
                            &message,
                            Some(InboxState::Unread),
                            self.context.self_id,
                        )
                    {
                        counts
                            .entry(message.chat.to_string())
                            .or_insert((message.chat, 0))
                            .1 += 1;
                    }
                }
                counts.into_values().collect()
            }
        };
        let page = filter::page(counts, query);
        let mut out = String::from("# Unread messages\n");
        if page.total == 0 {
            out.push_str(if query.filter.is_some() {
                "- no unread message matches the filter\n"
            } else {
                "- nothing unread\n"
            });
        }
        for (chat, count) in &page.items {
            out.push_str(&format!("- {chat}: {count} unread\n"));
        }
        if let Some(hint) = page.next_page_hint() {
            out.push_str(&hint);
            out.push('\n');
        }
        Ok(ViewRead::Text(out))
    }

    /// `/messages/<scope>/<peer>/<YYYY-MM>/<DD>.md`：某天的消息，一页。
    ///
    /// `date` 是 `YYYY-MM-DD`；那天没有任何消息时文件不存在。`mark_read` 时本页的
    /// 消息标记已读。
    pub(super) fn read_message_day(
        &self,
        path: &VfsPath,
        chat: ChatId,
        date: &str,
        query: &ViewQuery,
        mark_read: bool,
    ) -> Result<ViewRead, VfsError> {
        self.ensure_receive(chat)?;
        let messages = self
            .messages
            .messages_for_day(chat, date)
            .map_err(storage)?;
        if messages.is_empty() {
            return Err(VfsError::NotFound(path.to_string()));
        }
        // 只有条件里有 `state:` / `unread` 时才查状态，一次查完这一天。
        let states = match &query.filter {
            Some(filter) if filter.needs_state() => {
                self.messages.states_for_day(chat, date).map_err(storage)?
            }
            _ => std::collections::HashMap::new(),
        };
        let selected: Vec<QqMessage> = messages
            .into_iter()
            .filter(|message| {
                query.filter.as_ref().is_none_or(|filter| {
                    filter::matches_message(
                        filter,
                        message,
                        states.get(&message.message_id).copied(),
                        self.context.self_id,
                    )
                })
            })
            .collect();
        let page = filter::page(selected, query);
        if mark_read {
            let ids: Vec<i64> = page
                .items
                .iter()
                .map(|message| message.message_id)
                .collect();
            self.messages.mark_read(chat, &ids).map_err(storage)?;
        }
        let mut out = page
            .items
            .iter()
            .map(format::render_inbound)
            .collect::<Vec<_>>()
            .join("\n");
        if page.total == 0 {
            out.push_str("No message on this day matches the filter.");
        }
        if let Some(hint) = page.next_page_hint() {
            out.push_str("\n\n");
            out.push_str(&hint);
        }
        Ok(ViewRead::Text(out))
    }
}
