//! 动态视图查询串中 `filter` 参数的语法。
//!
//! 本模块只把文本解析成结构化条件；条件怎样与消息、列表条目比较由视图层决定
//! （`tools::views::filter`）。解析发生在查询串解析时：写错的条件立即报错，
//! 而不是在求值时被当成“不匹配”悄悄吞掉，返回一个看似正常的空结果。
//!
//! 语法：空白分隔的若干条件，全部满足才算匹配（AND）。
//!
//! | 条件 | 含义 |
//! |------|------|
//! | `unread` | 等同 `state:unread` |
//! | `state:<unread\|read\|dismissed>` | 收件箱状态 |
//! | `from:<qq>` | 发送者 QQ 号 |
//! | `mentions_self` | @ 了本账号或 @全体成员 |
//! | `contains:<text>` | 文本包含子串（不区分大小写） |
//! | `after:<YYYY-MM-DD>` | 本地日期在这一天或之后 |
//! | `before:<YYYY-MM-DD>` | 本地日期在这一天或之前 |
//!
//! 值里含空白时用双引号包住：`contains:"发布 计划"`；引号内 `\"` 表示引号本身、
//! `\\` 表示反斜杠。

use chrono::NaiveDate;

use crate::storage::messages::InboxState;

/// `filter` 原文的长度上限（字符数）；防止超长查询串放大求值成本。
const MAX_FILTER_CHARS: usize = 1024;

/// 单个 `filter` 内的条件数上限。
const MAX_TERMS: usize = 16;

/// 一个过滤条件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterTerm {
    /// 收件箱状态等于给定值（`unread` 是 `state:unread` 的简写）。
    State(InboxState),
    /// 发送者 QQ 号。
    From(u64),
    /// @ 了本账号或 @全体成员。
    MentionsSelf,
    /// 文本包含子串；解析时已转成小写，比较时对正文同样转小写。
    Contains(String),
    /// 本地日期不早于这一天。
    After(NaiveDate),
    /// 本地日期不晚于这一天。
    Before(NaiveDate),
}

impl std::fmt::Display for FilterTerm {
    /// 写回条件语法，用在错误信息里；`contains:` 的值总是加引号（已是小写）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::State(state) => write!(f, "state:{}", state.as_str()),
            Self::From(qq) => write!(f, "from:{qq}"),
            Self::MentionsSelf => f.write_str("mentions_self"),
            Self::Contains(text) => write!(f, "contains:{text:?}"),
            Self::After(day) => write!(f, "after:{day}"),
            Self::Before(day) => write!(f, "before:{day}"),
        }
    }
}

/// 解析后的 `filter`：全部条件同时满足才算匹配。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewFilter {
    /// 至少一个条件（空 `filter` 在解析时就被拒绝）。
    terms: Vec<FilterTerm>,
}

impl ViewFilter {
    /// 解析 `filter` 原文（已完成百分号解码）。
    ///
    /// 错误信息面向模型，英文，说明哪个条件出了什么问题。
    pub fn parse(raw: &str) -> Result<Self, String> {
        // 长度按字符计，与路径长度上限的口径一致。
        if raw.chars().count() > MAX_FILTER_CHARS {
            return Err(format!(
                "filter is longer than {MAX_FILTER_CHARS} characters"
            ));
        }
        // 先按空白与引号切词，再逐个解释。
        let tokens = tokenize(raw)?;
        if tokens.is_empty() {
            return Err("filter is empty".to_owned());
        }
        if tokens.len() > MAX_TERMS {
            return Err(format!("filter has more than {MAX_TERMS} terms"));
        }
        let terms = tokens
            .iter()
            .map(|token| parse_term(token))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { terms })
    }

    /// 全部条件，按书写顺序。
    pub fn terms(&self) -> &[FilterTerm] {
        &self.terms
    }

    /// 是否有条件需要查询收件箱状态；没有时调用方可以省掉状态查询。
    pub fn needs_state(&self) -> bool {
        self.terms
            .iter()
            .any(|term| matches!(term, FilterTerm::State(_)))
    }
}

/// 按空白切词；双引号内的空白属于值，`\"` 与 `\\` 是引号内的转义。
fn tokenize(raw: &str) -> Result<Vec<String>, String> {
    // 已切出的词。
    let mut tokens = Vec::new();
    // 正在拼的词。
    let mut current = String::new();
    // 当前词里是否出现过引号：`contains:""` 这样的空值也要切出一个词，交给后面报错。
    let mut quoted_token = false;
    // 是否位于一对引号之内。
    let mut in_quotes = false;
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        match ch {
            // 引号切换“引号内”状态，本身不进入值。
            '"' => {
                in_quotes = !in_quotes;
                quoted_token = true;
            }
            // 引号内的反斜杠转义下一个字符；只允许转义引号和反斜杠，其余写法报错，
            // 免得 `\n` 之类被误以为是换行。
            '\\' if in_quotes => match chars.next() {
                Some(escaped @ ('"' | '\\')) => current.push(escaped),
                _ => return Err("inside quotes only \\\" and \\\\ are valid escapes".to_owned()),
            },
            // 引号外的空白结束当前词。
            ch if ch.is_whitespace() && !in_quotes => {
                if !current.is_empty() || quoted_token {
                    tokens.push(std::mem::take(&mut current));
                }
                quoted_token = false;
            }
            // 其余字符（含引号内的空白）原样进入当前词。
            ch => current.push(ch),
        }
    }
    if in_quotes {
        return Err("unterminated quote in filter".to_owned());
    }
    // 收尾：最后一个词后面没有空白。
    if !current.is_empty() || quoted_token {
        tokens.push(current);
    }
    Ok(tokens)
}

/// 解释一个词。
fn parse_term(token: &str) -> Result<FilterTerm, String> {
    // 不带值的条件。
    match token {
        "unread" => return Ok(FilterTerm::State(InboxState::Unread)),
        "mentions_self" => return Ok(FilterTerm::MentionsSelf),
        _ => {}
    }
    // 其余条件都是 `key:value`；第一个冒号之后都属于值。
    let Some((key, value)) = token.split_once(':') else {
        return Err(format!("unknown filter term {token:?}"));
    };
    if value.is_empty() {
        return Err(format!("filter term {key:?} needs a value"));
    }
    match key {
        "state" => InboxState::parse(value)
            .map(FilterTerm::State)
            .ok_or_else(|| format!("state must be unread, read or dismissed, not {value:?}")),
        "from" => value
            .parse::<u64>()
            .map(FilterTerm::From)
            .map_err(|_| format!("from expects a QQ number, not {value:?}")),
        "contains" => Ok(FilterTerm::Contains(value.to_lowercase())),
        "after" => parse_date(key, value).map(FilterTerm::After),
        "before" => parse_date(key, value).map(FilterTerm::Before),
        _ => Err(format!("unknown filter term {key:?}")),
    }
}

/// `YYYY-MM-DD`；`2026-02-30` 这类不存在的日期同样报错。
fn parse_date(key: &str, value: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| format!("{key} expects a date like 2026-11-08, not {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_are_parsed_into_structure() {
        let filter =
            ViewFilter::parse("unread from:10001 contains:\"Hello World\" after:2026-11-01")
                .unwrap();
        assert_eq!(
            filter.terms(),
            &[
                FilterTerm::State(InboxState::Unread),
                FilterTerm::From(10001),
                // 值转成小写，引号内的空白保留。
                FilterTerm::Contains("hello world".to_owned()),
                FilterTerm::After(NaiveDate::from_ymd_opt(2026, 11, 1).unwrap()),
            ]
        );
        assert!(filter.needs_state());
        assert!(!ViewFilter::parse("mentions_self").unwrap().needs_state());
        let rendered: Vec<String> = filter.terms().iter().map(ToString::to_string).collect();
        assert_eq!(
            rendered,
            [
                "state:unread",
                "from:10001",
                "contains:\"hello world\"",
                "after:2026-11-01"
            ]
        );
    }

    #[test]
    fn quotes_support_escapes_and_colons() {
        let filter = ViewFilter::parse(r#"contains:"say \"hi\" a:b \\""#).unwrap();
        assert_eq!(
            filter.terms(),
            &[FilterTerm::Contains(r#"say "hi" a:b \"#.to_owned())]
        );
    }

    #[test]
    fn mistakes_are_errors_not_silent_mismatches() {
        for bad in [
            "",
            "   ",
            "contains(\"活动\")",
            "contains:",
            "contains:\"\"",
            "contains:\"open",
            "contains:\"a\\nb\"",
            "from:alice",
            "state:new",
            "after:2026-02-30",
            "before:yesterday",
            "sender:1",
        ] {
            assert!(
                ViewFilter::parse(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        let many = vec!["unread"; MAX_TERMS + 1].join(" ");
        assert!(ViewFilter::parse(&many).is_err());
        assert!(ViewFilter::parse(&"x".repeat(MAX_FILTER_CHARS + 1)).is_err());
    }
}
