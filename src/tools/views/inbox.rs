//! `Views` 的 inbox 部分：未读来源列表、某 chat 的未读、单条消息，以及忽略（dismiss）。
//!
//! 由 `views.rs` 的 `list` / `read` / `dismiss` 路由调用。筛选与分页见 `views/filter.rs`。

use std::collections::BTreeMap;

use super::filter::{self, Page};
use super::*;
use crate::vfs::path::ViewQuery;

/// 一次忽略的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Dismissed {
    /// 实际离开收件箱的条数。
    pub(crate) count: usize,
    /// 同样满足条件、但在本页之外没有处理的条数（只有带查询串时可能非零）。
    pub(crate) remaining: usize,
}

impl Views {
    pub(super) fn list_inbox_root(&self) -> Result<Vec<ViewEntry>, VfsError> {
        let mut entries = Vec::new();
        for (chat, count) in self.messages.unread_counts().map_err(storage)? {
            if !self.receive_allows(chat) {
                continue;
            }
            // 目录大小 = 该 chat 的未读数。
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
        self.ensure_receive(chat)?;
        // `index.md` 是整个 chat 的未读汇总，其后每条未读一个文件。
        let mut entries = vec![text_entry(&format!("/inbox/{chat}/index.md"))];
        for message in self.messages.unread(Some(chat)).map_err(storage)? {
            entries.push(text_entry(&format!(
                "/inbox/{chat}/{}.md",
                message.message_id
            )));
        }
        Ok(entries)
    }

    /// 满足查询的未读消息（一页）；`chat` 为 `None` 时跨全部允许读取的 chat。
    fn select_unread(
        &self,
        chat: Option<ChatId>,
        query: &ViewQuery,
    ) -> Result<Page<QqMessage>, VfsError> {
        let mut messages = self.messages.unread(chat).map_err(storage)?;
        messages.retain(|message| {
            // 越过 receive_scopes 的 chat 不出现，也不计入总数。
            self.receive_allows(message.chat)
                && query.filter.as_ref().is_none_or(|filter| {
                    filter::matches_message(
                        filter,
                        message,
                        Some(InboxState::Unread),
                        self.context.self_id,
                    )
                })
        });
        Ok(filter::page(messages, query))
    }

    /// `/inbox`（`chat` 为 `None`）或 `/inbox/<chat>`：渲染一页未读。
    ///
    /// `mark_read` 时本页每条都标记已读；页外的保持未读，结尾说明还剩多少。
    pub(super) fn read_inbox(
        &self,
        chat: Option<&str>,
        query: &ViewQuery,
        mark_read: bool,
    ) -> Result<ViewRead, VfsError> {
        let chat = chat.map(parse_chat_segment).transpose()?;
        if let Some(chat) = chat {
            self.ensure_receive(chat)?;
        }
        let page = self.select_unread(chat, query)?;
        let mut out = match chat {
            Some(chat) => format!("# Inbox: {chat}\n"),
            None => String::from("# Inbox\n"),
        };
        if page.total == 0 {
            out.push_str(if query.filter.is_some() {
                "No unread message matches the filter.\n"
            } else {
                "Nothing unread.\n"
            });
        }
        for message in &page.items {
            out.push_str(&format::render_inbound(message));
            out.push_str("\n\n");
        }
        if mark_read {
            // 内容进入上下文即已读；按 chat 分批写状态。
            for (chat, ids) in group_by_chat(&page.items) {
                self.messages.mark_read(chat, &ids).map_err(storage)?;
            }
        }
        let remaining = page.remaining();
        if remaining > 0 {
            out.push_str(&format!(
                "[{remaining} more unread not shown; they stay unread. Read again for the next ones]\n"
            ));
        } else if page.items.is_empty()
            && let Some(hint) = page.next_page_hint()
        {
            // 只剩“offset 越过末尾”这一种提示。
            out.push_str(&hint);
            out.push('\n');
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
        self.ensure_receive(chat)?;
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

    /// 忽略一页未读：`chat` 为 `None` 时跨全部 chat（调用方保证此时带 `filter`）。
    pub(super) fn dismiss_unread(
        &self,
        chat: Option<ChatId>,
        query: &ViewQuery,
    ) -> Result<Dismissed, VfsError> {
        let page = self.select_unread(chat, query)?;
        let mut count = 0;
        for (chat, ids) in group_by_chat(&page.items) {
            count += self.messages.dismiss(chat, &ids).map_err(storage)?;
        }
        Ok(Dismissed {
            count,
            remaining: page.remaining(),
        })
    }

    /// `/inbox/<chat>` 或它的 `index.md`：不带查询串时是该 chat 的全部未读，
    /// 带查询串时只处理筛选、分页后的一页。
    pub(super) fn dismiss_chat(
        &self,
        chat: &str,
        query: Option<&ViewQuery>,
    ) -> Result<Dismissed, VfsError> {
        let chat = parse_chat_segment(chat)?;
        self.ensure_receive(chat)?;
        match query {
            Some(query) => self.dismiss_unread(Some(chat), query),
            None => self.dismiss_unread(Some(chat), &ViewQuery::unbounded()),
        }
    }

    /// 忽略单条消息。
    pub(super) fn dismiss_one(&self, chat: ChatId, message_id: i64) -> Result<Dismissed, VfsError> {
        Ok(Dismissed {
            count: self
                .messages
                .dismiss(chat, &[message_id])
                .map_err(storage)?,
            remaining: 0,
        })
    }
}

/// 按 chat 分组消息 id，供批量写状态；`BTreeMap` 让写入顺序稳定。
fn group_by_chat(messages: &[QqMessage]) -> BTreeMap<ChatId, Vec<i64>> {
    let mut groups: BTreeMap<ChatId, Vec<i64>> = BTreeMap::new();
    for message in messages {
        groups
            .entry(message.chat)
            .or_default()
            .push(message.message_id);
    }
    groups
}
