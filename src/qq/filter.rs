//! 消息准入过滤器。
//!
//! 设计依据：设计 §4 / §13「过滤是拒收，不是提示开关」。
//!
//! 这是**外层的准入门**：被拒的消息完全不进系统——
//! 不写 JSONL、不进 inbox、不进 transcript，agent 看不到也搜不到。
//! 它因此成为 agent 划定自己活动范围的工具（例如排除含保密内容的工作群）。
//!
//! 审计只记录命中的规则名与条数，不记录内容、发送者或 chat_id。

use std::collections::HashSet;

use regex::Regex;

use crate::config::FilterConfig;

use super::event::QqMessage;

/// 过滤结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterVerdict {
    Accept,
    /// 拒收，原因只记规则名。
    Reject {
        rule: &'static str,
    },
}

impl FilterVerdict {
    pub fn is_accept(self) -> bool {
        matches!(self, Self::Accept)
    }
}

/// 过滤器。
pub struct MessageFilter {
    ignore_groups: HashSet<u64>,
    ignore_users: HashSet<u64>,
    patterns: Vec<Regex>,
    require_mention: bool,
}

impl MessageFilter {
    /// 从配置构造；正则已在启动校验中验证过。
    pub fn from_config(config: &FilterConfig) -> Self {
        Self {
            ignore_groups: config.ignore_groups.iter().copied().collect(),
            ignore_users: config.ignore_users.iter().copied().collect(),
            patterns: config
                .ignore_patterns
                .iter()
                .filter_map(|pattern| Regex::new(pattern).ok())
                .collect(),
            require_mention: config.require_mention,
        }
    }

    /// 判定一条消息是否准入。
    pub fn check(&self, message: &QqMessage, self_id: u64) -> FilterVerdict {
        // 自己发出的消息永远记录：过滤只针对外部输入。
        if message.is_self {
            return FilterVerdict::Accept;
        }

        if let super::event::ChatId::Group(group) = message.chat {
            if self.ignore_groups.contains(&group) {
                return FilterVerdict::Reject {
                    rule: "ignore_groups",
                };
            }
            if self.require_mention && !message.mentions_self(self_id) {
                return FilterVerdict::Reject {
                    rule: "require_mention",
                };
            }
        }

        if let Some(qq) = message.sender.qq
            && self.ignore_users.contains(&qq) {
                return FilterVerdict::Reject {
                    rule: "ignore_users",
                };
            }

        let text = message.plain_text();
        if self.patterns.iter().any(|pattern| pattern.is_match(&text)) {
            return FilterVerdict::Reject {
                rule: "ignore_patterns",
            };
        }

        FilterVerdict::Accept
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qq::event::{ChatId, Segment, Sender};
    use chrono::Utc;
    use serde_json::Value;

    fn message(group: u64, user: u64, text: &str) -> QqMessage {
        QqMessage {
            message_id: 1,
            chat: ChatId::Group(group),
            sender: Sender {
                qq: Some(user),
                nickname: "n".to_owned(),
                card: String::new(),
                role: "member".to_owned(),
            },
            time: Utc::now(),
            segments: vec![Segment::Text {
                text: text.to_owned(),
            }],
            reply_to: None,
            is_self: false,
            backfilled: false,
            raw: Value::Null,
        }
    }

    fn filter(config: FilterConfig) -> MessageFilter {
        MessageFilter::from_config(&config)
    }

    #[test]
    fn accepts_ordinary_messages() {
        let filter = filter(FilterConfig::default());
        assert!(filter.check(&message(1, 2, "hi"), 10).is_accept());
    }

    #[test]
    fn drops_ignored_groups_and_users() {
        let filter = filter(FilterConfig {
            ignore_groups: vec![1],
            ignore_users: vec![2],
            ..FilterConfig::default()
        });
        assert_eq!(
            filter.check(&message(1, 3, "hi"), 10),
            FilterVerdict::Reject {
                rule: "ignore_groups"
            }
        );
        assert_eq!(
            filter.check(&message(4, 2, "hi"), 10),
            FilterVerdict::Reject {
                rule: "ignore_users"
            }
        );
    }

    #[test]
    fn pattern_matches_any_segment_text() {
        let filter = filter(FilterConfig {
            ignore_patterns: vec!["机密".to_owned()],
            ..FilterConfig::default()
        });
        assert_eq!(
            filter.check(&message(1, 2, "这是机密内容"), 10),
            FilterVerdict::Reject {
                rule: "ignore_patterns"
            }
        );
    }

    #[test]
    fn require_mention_only_applies_to_groups() {
        let filter = filter(FilterConfig {
            require_mention: true,
            ..FilterConfig::default()
        });
        assert_eq!(
            filter.check(&message(1, 2, "hi"), 10),
            FilterVerdict::Reject {
                rule: "require_mention"
            }
        );

        let mut mentioned = message(1, 2, "hi");
        mentioned.segments.push(Segment::At {
            qq: "10".to_owned(),
        });
        assert!(filter.check(&mentioned, 10).is_accept());

        let mut private = message(1, 2, "hi");
        private.chat = ChatId::Private(2);
        assert!(filter.check(&private, 10).is_accept());
    }

    #[test]
    fn self_messages_always_pass() {
        let filter = filter(FilterConfig {
            ignore_groups: vec![1],
            ignore_patterns: vec![".*".to_owned()],
            ..FilterConfig::default()
        });
        let mut own = message(1, 2, "anything");
        own.is_self = true;
        own.sender.qq = None;
        assert!(filter.check(&own, 10).is_accept());
    }
}
