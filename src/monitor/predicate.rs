//! `when` 谓词：`and` 连接的若干项，每项可加 `not` 取反。
//!
//! 支持的项：`event.mentions_self`、`event.type == '…'`、`event.chat == '…'`、
//! `event.sender == '…'` / `!=`、`event.text contains '…'`。创建 monitor 时用
//! [`validate_when`] 逐项校验；运行时用 [`matches`] 求值。

use super::MonitorEvent;

/// 谓词求值。表达式在 [`validate_when`] 里已经校验过；这里的错误只是防御。
pub fn matches(when: Option<&str>, event: &MonitorEvent) -> Result<bool, String> {
    let Some(when) = when else {
        return Ok(true);
    };
    for term in when.split(" and ") {
        let term = term.trim();
        if term.is_empty() {
            continue;
        }
        if !match_term(term, event)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 校验 `when` 表达式：每一项都必须是受支持的谓词。
///
/// 必须逐项检查：`matches` 遇到第一个为假的项就短路，后面写错的项可能永远不被求值，
/// 直到某个事件恰好让前面的项都为真才暴露。
pub fn validate_when(when: &str) -> Result<(), String> {
    // 空事件只用来让每一项都走一遍解析；结果不重要，错误才重要。
    let probe = MonitorEvent::default();
    let mut terms = 0;
    for term in when.split(" and ") {
        let term = term.trim();
        if term.is_empty() {
            return Err(format!("empty term in `when` expression {when:?}"));
        }
        match_term(term, &probe)?;
        terms += 1;
    }
    if terms == 0 {
        return Err("`when` expression is empty".to_owned());
    }
    Ok(())
}

/// 求值一项；不认识的项报错。
fn match_term(term: &str, event: &MonitorEvent) -> Result<bool, String> {
    // `not <term>` 对任意受支持的项取反。
    if let Some(inner) = term.strip_prefix("not ") {
        return match_term(inner.trim(), event).map(|matched| !matched);
    }
    if term == "event.mentions_self" {
        return Ok(event.mentions_self);
    }
    if let Some(value) = term.strip_prefix("event.type == ") {
        return Ok(event.kind == unquote(value));
    }
    if let Some(value) = term.strip_prefix("event.chat == ") {
        return Ok(event.chat == unquote(value));
    }
    if let Some(value) = term.strip_prefix("event.sender == ") {
        return Ok(event.sender.map(|sender| sender.to_string()).as_deref() == Some(unquote(value)));
    }
    if let Some(value) = term.strip_prefix("event.sender != ") {
        return Ok(event.sender.map(|sender| sender.to_string()).as_deref() != Some(unquote(value)));
    }
    if let Some(rest) = term.strip_prefix("event.text contains ") {
        return Ok(event.text.contains(unquote(rest)));
    }
    Err(format!("unsupported predicate term {term:?}"))
}

/// 去掉值两端的空白与单 / 双引号。
fn unquote(value: &str) -> &str {
    value.trim().trim_matches(|c| c == '\'' || c == '"')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(chat: &str, text: &str, mentions: bool) -> MonitorEvent {
        MonitorEvent {
            kind: "message".to_owned(),
            chat: chat.to_owned(),
            sender: Some(10001),
            text: text.to_owned(),
            mentions_self: mentions,
            ..MonitorEvent::default()
        }
    }

    #[test]
    fn every_term_is_checked_even_after_a_false_one() {
        assert!(validate_when("event.mentions_self and not event.text contains 'x'").is_ok());
        assert!(validate_when("event.type == 'notice' and event.bogus").is_err());
        assert!(validate_when("event.mentions_self and ").is_err());
    }
    #[test]
    fn predicates_support_the_catalog_subset() {
        let event = event("group:123", "报名了", true);
        assert!(matches(None, &event).unwrap());
        assert!(matches(Some("event.mentions_self"), &event).unwrap());
        assert!(matches(Some("event.type == 'message'"), &event).unwrap());
        assert!(matches(Some("event.chat == 'group:123'"), &event).unwrap());
        assert!(matches(Some("event.text contains '报名'"), &event).unwrap());
        assert!(
            matches(
                Some("event.mentions_self and event.chat == 'group:123'"),
                &event
            )
            .unwrap()
        );
        assert!(!matches(Some("event.chat == 'private:1'"), &event).unwrap());
        assert!(matches(Some("event.nonexistent"), &event).is_err());
    }
    #[test]
    fn sender_not_equal_predicate() {
        let event = MonitorEvent {
            kind: "message".to_owned(),
            chat: "group:1".to_owned(),
            sender: Some(42),
            text: "hi".to_owned(),
            mentions_self: true,
            owner_intent: false,
            message: None,
        };
        assert!(
            matches(
                Some("event.type == 'message' and event.mentions_self and event.sender != '7'"),
                &event
            )
            .unwrap()
        );
        assert!(
            !matches(
                Some("event.type == 'message' and event.sender != '42'"),
                &event
            )
            .unwrap()
        );
    }
    #[test]
    fn not_negates_any_supported_term() {
        let event = MonitorEvent {
            kind: "message".to_owned(),
            chat: "group:1".to_owned(),
            sender: Some(5),
            text: "hi".to_owned(),
            mentions_self: true,
            owner_intent: false,
            message: None,
        };
        assert_eq!(
            matches(
                Some("event.mentions_self and not event.sender == '7'"),
                &event
            ),
            Ok(true)
        );
        assert_eq!(matches(Some("not event.mentions_self"), &event), Ok(false));
        assert!(matches(Some("not event.bogus"), &event).is_err());
    }
}
