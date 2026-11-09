//! 动态视图共用的筛选与分页。
//!
//! 条件语法与解析在 [`crate::vfs::filter`]；这里只负责求值：
//! - [`matches_message`]：消息视图（`/inbox`、`/messages`）的条件；
//! - [`check_listing_filter`] / [`matches_path`]：目录列举只认 `contains:`；
//! - [`page`]：筛选之后统一按 `order`、`offset`、`limit` 截取，并记下总数，
//!   让视图能告诉模型还剩多少没给出。

use super::*;
use crate::vfs::filter::{FilterTerm, ViewFilter};
use crate::vfs::path::{Order, ViewQuery};

/// 消息是否满足全部条件。
///
/// `state` 是这条消息的收件箱状态；没有状态（不在收件箱里）时为 `None`，此时任何
/// `state:` 条件都不满足。`self_id` 是本账号 QQ，`mentions_self` 用它判断。
pub(super) fn matches_message(
    filter: &ViewFilter,
    message: &QqMessage,
    state: Option<InboxState>,
    self_id: u64,
) -> bool {
    // 正文小写化只在真的有 `contains:` 时做一次。
    let mut text: Option<String> = None;
    // 本地日期同理，按需计算。
    let date = || message.time.with_timezone(&chrono::Local).date_naive();
    filter.terms().iter().all(|term| match term {
        FilterTerm::State(want) => state == Some(*want),
        FilterTerm::From(qq) => message.sender.qq == Some(*qq),
        FilterTerm::MentionsSelf => message.mentions_self(self_id),
        FilterTerm::Contains(needle) => text
            .get_or_insert_with(|| message.plain_text().to_lowercase())
            .contains(needle.as_str()),
        FilterTerm::After(day) => date() >= *day,
        FilterTerm::Before(day) => date() <= *day,
    })
}

/// 目录列举的条目只有路径可比，只接受 `contains:`；其余条件报错，而不是被忽略。
pub(super) fn check_listing_filter(filter: &ViewFilter) -> Result<(), VfsError> {
    match filter
        .terms()
        .iter()
        .find(|term| !matches!(term, FilterTerm::Contains(_)))
    {
        None => Ok(()),
        Some(term) => Err(VfsError::Path(crate::vfs::PathError::BadFilter(format!(
            "listings only support contains:<text>; {term} applies to message views \
             such as /inbox or a /messages day file"
        )))),
    }
}

/// 条目路径是否包含全部 `contains:` 子串（不区分大小写）。
///
/// 调用前应先用 [`check_listing_filter`] 拒绝其他条件；这里遇到其他条件按不匹配处理。
pub(super) fn matches_path(filter: &ViewFilter, path: &str) -> bool {
    let path = path.to_lowercase();
    filter.terms().iter().all(|term| match term {
        FilterTerm::Contains(needle) => path.contains(needle.as_str()),
        _ => false,
    })
}

/// 筛选之后的一页结果。
#[derive(Debug)]
pub(crate) struct Page<T> {
    /// 本页条目（已按 `order` 排好）。
    pub(crate) items: Vec<T>,
    /// 筛选后、截取前的总数。
    pub(crate) total: usize,
    /// 本页之前跳过的条数。
    pub(crate) offset: usize,
}

impl<T> Page<T> {
    /// 不分页：全部条目作为一页。
    pub(crate) fn all(items: Vec<T>) -> Self {
        Self {
            total: items.len(),
            items,
            offset: 0,
        }
    }

    /// 本页之后还剩多少条没给出。
    pub(crate) fn remaining(&self) -> usize {
        self.total
            .saturating_sub(self.offset.saturating_add(self.items.len()))
    }

    /// 可翻页视图（`/messages` 的日文件、目录列举）的提示行：还有下一页时说明怎样取，
    /// `offset` 越过末尾时说明总数；整页给全时为 `None`。
    pub(crate) fn next_page_hint(&self) -> Option<String> {
        if self.remaining() > 0 {
            let first = self.offset + 1;
            let last = self.offset + self.items.len();
            Some(format!(
                "[showing {first}-{last} of {}; continue with offset={last}]",
                self.total
            ))
        } else if self.items.is_empty() && self.offset > 0 && self.total > 0 {
            Some(format!(
                "[offset {} is past the end; {} in total]",
                self.offset, self.total
            ))
        } else {
            None
        }
    }
}

/// 对已筛选的条目按 `order` 排序、跳过 `offset`、最多取 `limit`。
///
/// `items` 必须是升序（旧的在前）；`desc` 时整体反转。
pub(super) fn page<T>(mut items: Vec<T>, query: &ViewQuery) -> Page<T> {
    let total = items.len();
    if query.order == Order::Desc {
        items.reverse();
    }
    let items = items
        .into_iter()
        .skip(query.offset)
        .take(query.limit)
        .collect();
    Page {
        items,
        total,
        offset: query.offset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qq::event::{Segment, Sender};

    fn message(text: &str, segments: Vec<Segment>) -> QqMessage {
        let mut all = vec![Segment::Text {
            text: text.to_owned(),
        }];
        all.extend(segments);
        QqMessage {
            message_id: 1,
            chat: ChatId::Group(42),
            sender: Sender {
                qq: Some(10001),
                nickname: "Alice".to_owned(),
                card: String::new(),
                role: "member".to_owned(),
            },
            time: chrono::Utc::now(),
            segments: all,
            reply_to: None,
            is_self: false,
            raw: serde_json::Value::Null,
        }
    }

    fn filter(raw: &str) -> ViewFilter {
        ViewFilter::parse(raw).unwrap()
    }

    #[test]
    fn mentions_self_means_this_account_not_anyone() {
        let other = message(
            "hi",
            vec![Segment::At {
                qq: "555".to_owned(),
            }],
        );
        let me = message(
            "hi",
            vec![Segment::At {
                qq: "999".to_owned(),
            }],
        );
        let all = message(
            "hi",
            vec![Segment::At {
                qq: "all".to_owned(),
            }],
        );
        let mention = filter("mentions_self");
        assert!(!matches_message(&mention, &other, None, 999));
        assert!(matches_message(&mention, &me, None, 999));
        assert!(matches_message(&mention, &all, None, 999));
    }

    #[test]
    fn terms_combine_with_and() {
        let msg = message("Deploy the RELEASE today", Vec::new());
        let today = chrono::Local::now().date_naive();
        let yes = filter(&format!(
            "from:10001 contains:\"the release\" after:{today} before:{today} unread"
        ));
        assert!(matches_message(&yes, &msg, Some(InboxState::Unread), 1));
        // 同样的条件，状态不符就不匹配；没有状态时 `state:` 永不满足。
        assert!(!matches_message(&yes, &msg, Some(InboxState::Read), 1));
        assert!(!matches_message(&yes, &msg, None, 1));
        assert!(!matches_message(&filter("from:10002"), &msg, None, 1));
        let tomorrow = today.succ_opt().unwrap();
        assert!(!matches_message(
            &filter(&format!("after:{tomorrow}")),
            &msg,
            None,
            1
        ));
    }

    #[test]
    fn listings_reject_message_terms() {
        assert!(check_listing_filter(&filter("contains:group")).is_ok());
        assert!(check_listing_filter(&filter("contains:a from:1")).is_err());
        assert!(matches_path(&filter("contains:GROUP:4"), "/inbox/group:42"));
        assert!(!matches_path(
            &filter("contains:private"),
            "/inbox/group:42"
        ));
    }

    #[test]
    fn paging_orders_skips_and_counts() {
        let query = |raw: &str| ViewQuery::parse(raw).unwrap();
        let numbers: Vec<u32> = (1..=10).collect();

        let first = page(numbers.clone(), &query("limit=3"));
        assert_eq!(first.items, vec![1, 2, 3]);
        assert_eq!(first.remaining(), 7);
        assert_eq!(
            first.next_page_hint().as_deref(),
            Some("[showing 1-3 of 10; continue with offset=3]")
        );

        let newest = page(numbers.clone(), &query("order=desc&offset=2&limit=3"));
        assert_eq!(newest.items, vec![8, 7, 6]);

        let last = page(numbers.clone(), &query("offset=8&limit=5"));
        assert_eq!(last.items, vec![9, 10]);
        assert!(last.next_page_hint().is_none());

        let past = page(numbers, &query("offset=50"));
        assert!(past.items.is_empty());
        assert_eq!(
            past.next_page_hint().as_deref(),
            Some("[offset 50 is past the end; 10 in total]")
        );
        assert!(
            page(Vec::<u32>::new(), &query("offset=5"))
                .next_page_hint()
                .is_none()
        );
    }
}
