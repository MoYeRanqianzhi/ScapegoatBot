//! HTML → 纯文本，供 `WebFetch` 的本地抓取使用。
//!
//! 单次线性扫描：每个字节最多被看常数次。不用正则 `replace_all`——regex 文档写明
//! 遍历全部匹配的最坏复杂度是 O(m·n²)；页面里重复很多个没有结尾的 `<script`，
//! 每次匹配都要扫到文末，2 MiB 的页面就能卡住一个异步工作线程。
//!
//! 只做阅读需要的最少处理：去掉注释、`<script>` / `<style>` 及其内容和所有标签，
//! 解码常见命名实体与数字实体，折叠空白。不是完整的 HTML 解析器。

/// 实体名（`&` 与 `;` 之间）的最大长度；更长的不当实体，避免为一个 `&` 向后扫很远。
const MAX_ENTITY_CHARS: usize = 32;

/// 把 HTML 变成空白折叠后的文本。
pub(super) fn html_to_text(html: &str) -> String {
    // ASCII 小写副本：字节位置与原文一一对应（非 ASCII 字节不变），
    // 用来不区分大小写地查找 `</script` 等结束标记。
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() / 2);
    // 当前扫描位置（字节下标，始终落在字符边界上）。
    let mut pos = 0;
    while let Some(found) = html[pos..].find(['<', '&']) {
        let at = pos + found;
        // 特殊字符之前的普通文本原样保留。
        out.push_str(&html[pos..at]);
        if html.as_bytes()[at] == b'<' {
            pos = match skip_markup(html, &lower, at) {
                // 标签的位置换成一个空格，免得相邻两段文字粘在一起。
                Some(next) => {
                    out.push(' ');
                    next
                }
                // 不是标签（例如 `a < b`）：`<` 是正文。
                None => {
                    out.push('<');
                    at + 1
                }
            };
        } else {
            pos = match decode_entity(&html[at..]) {
                Some((decoded, consumed)) => {
                    out.push(decoded);
                    at + consumed
                }
                // 认不出的 `&` 原样保留。
                None => {
                    out.push('&');
                    at + 1
                }
            };
        }
    }
    out.push_str(&html[pos..]);
    // 折叠空白：标签换成的空格、换行、缩进都变成单个空格。
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `at` 处是 `<`：返回这段标记结束后的位置；不是标记时返回 `None`。
///
/// 所有查找都从 `at` 向后进行，找不到就吃掉余下全部，因此整个扫描是线性的。
fn skip_markup(html: &str, lower: &str, at: usize) -> Option<usize> {
    let rest = &lower[at..];
    // 注释：到 `-->` 为止。
    if rest.starts_with("<!--") {
        return Some(find_after(lower, at + 4, "-->"));
    }
    // 只有 `<` 后紧跟字母、`/`、`!`、`?` 才是标记（与 HTML 分词规则一致）。
    let next = *rest.as_bytes().get(1)?;
    if !(next.is_ascii_alphabetic() || matches!(next, b'/' | b'!' | b'?')) {
        return None;
    }
    // `<script>` / `<style>`：连同内容一起跳过，到对应的结束标签为止。
    for element in ["script", "style"] {
        if opens_element(rest, element) {
            let close = format!("</{element}");
            let body_end = lower[at..]
                .find(&close)
                .map_or(html.len(), |offset| at + offset);
            return Some(find_after(lower, body_end, ">"));
        }
    }
    // 其余标签：到下一个 `>` 为止。
    Some(find_after(lower, at + 1, ">"))
}

/// `rest` 是否以 `<element` 开头，且元素名到此结束（`<scripts>` 不算 `<script>`）。
fn opens_element(rest: &str, element: &str) -> bool {
    let Some(after) = rest
        .strip_prefix('<')
        .and_then(|rest| rest.strip_prefix(element))
    else {
        return false;
    };
    after
        .bytes()
        .next()
        .is_none_or(|byte| byte.is_ascii_whitespace() || matches!(byte, b'>' | b'/'))
}

/// 从 `from` 起找 `needle`，返回它之后的位置；找不到返回文末。
fn find_after(haystack: &str, from: usize, needle: &str) -> usize {
    match haystack.get(from..).and_then(|rest| rest.find(needle)) {
        Some(offset) => from + offset + needle.len(),
        None => haystack.len(),
    }
}

/// `text` 以 `&` 开头：解码一个实体，返回字符与占用的字节数。
fn decode_entity(text: &str) -> Option<(char, usize)> {
    // 在有限范围内找结尾的 `;`。
    let (end, _) = text
        .char_indices()
        .skip(1)
        .take(MAX_ENTITY_CHARS)
        .find(|(_, ch)| *ch == ';')?;
    let name = &text[1..end];
    let decoded = match name {
        "nbsp" => ' ',
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => {
            // 数字实体：`&#39;`、`&#x27;`。
            let number = name.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse::<u32>().ok()?,
            };
            // NUL、代理区与越界值换成替换字符，与浏览器的处理一致。
            match char::from_u32(code) {
                Some('\0') | None => char::REPLACEMENT_CHARACTER,
                Some(ch) => ch,
            }
        }
    };
    Some((decoded, end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_is_removed_and_text_is_kept() {
        let text = html_to_text(
            "<html><STYLE>x{}</STYLE><script type=x>if (a < b) {}</Script>\
             <!-- note --><p>Hello&nbsp;world &amp; friends</p><scripts>kept</scripts></html>",
        );
        assert_eq!(text, "Hello world & friends kept");
    }

    #[test]
    fn entities_are_decoded_once() {
        // `&amp;lt;` 是字面的 `&lt;`，不能再解一层变成 `<`。
        assert_eq!(html_to_text("a &amp;lt; b"), "a &lt; b");
        assert_eq!(html_to_text("&#39;&#x41;&#X42;&apos;"), "'AB'");
        assert_eq!(html_to_text("&#0;&#xD800;"), "\u{FFFD}\u{FFFD}");
        // 认不出的实体和裸 `&`、裸 `<` 都是正文。
        assert_eq!(html_to_text("R&D &unknown; 1 < 2"), "R&D &unknown; 1 < 2");
    }

    #[test]
    fn unterminated_markup_swallows_the_rest() {
        assert_eq!(html_to_text("before <script>never closed"), "before");
        assert_eq!(html_to_text("before <!-- open"), "before");
        assert_eq!(html_to_text("before <p class="), "before");
    }

    #[test]
    fn pathological_input_is_linear() {
        // 正则实现在这种输入上是二次方：每个 `<script` 都要扫到文末。
        let html = "<script>".repeat(200_000) + &"x".repeat(1_000_000);
        let started = std::time::Instant::now();
        assert_eq!(html_to_text(&html), "");
        // 每个 `&` 最多向后看 `MAX_ENTITY_CHARS` 个字符；不设这个上限的话每个都要扫到文末。
        // 规模的取法：线性实现在最慢的 CI 机器（未优化的测试构建）上也远不到 1 秒，二次方的
        // 实现要几分钟，5 秒的界线离两边都很远。曾经用 100 万个 `&`，线性实现在慢机器上
        // 自己就要 7 秒。
        let entities = "&".repeat(100_000);
        assert_eq!(html_to_text(&entities).len(), 100_000);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }
}
