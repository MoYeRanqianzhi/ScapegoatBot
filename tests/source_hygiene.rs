//! 源码卫生检查：模型可见的字符串里不能出现断裂的空白。
//!
//! 多行字符串字面量漏写行尾的 `\` 时，下一行的缩进会原样进入字符串，模型看到的
//! 错误提示就变成 `... not                          allowed`。这类错误编译器不报，
//! 只能靠扫描源码发现；历史上它出现过十多处。

use std::path::Path;

/// 递归收集目录下的 `.rs` 文件。
fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("readable source directory") {
        let path = entry.expect("readable directory entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn string_literals_have_no_broken_indentation() {
    // 一对引号之间：非空白字符后面跟着 8 个以上空格，再接非空白字符。
    let broken = regex::Regex::new(r#""[^"]*\S {8,}\S[^"]*""#).unwrap();
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let mut hits = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        for (index, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            if broken.is_match(line) {
                hits.push(format!("{}:{}: {}", file.display(), index + 1, line.trim()));
            }
        }
        for line in raw_newlines_followed_by_indentation(&text) {
            hits.push(format!(
                "{}:{line}: string literal continues on an indented line without `\\`",
                file.display()
            ));
        }
    }
    assert!(
        hits.is_empty(),
        "string literals with a run of spaces (missing `\\` line continuation?):\n{}",
        hits.join("\n")
    );
}

/// 普通（非 raw）字符串字面量里的真实换行，且下一行以缩进开头：几乎一定是漏写了 `\`。
///
/// 单行正则看不到跨行的字面量（开引号在上一行），所以这里用一个最小的词法扫描：
/// 跳过注释、raw 字符串与字符字面量，只在普通字符串内部检查换行。
/// 返回出问题的行号（1 起）。
fn raw_newlines_followed_by_indentation(source: &str) -> Vec<usize> {
    // Windows checkout（`core.autocrlf`）是 CRLF；`\` 续行后面是 `\r\n`，先归一化。
    let chars: Vec<char> = source.replace("\r\n", "\n").chars().collect();
    let mut hits = Vec::new();
    let mut line = 1;
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        let next = chars.get(index + 1).copied();
        match c {
            '\n' => line += 1,
            // 行注释：跳到行尾（换行在下一轮计数）。
            '/' if next == Some('/') => {
                while index + 1 < chars.len() && chars[index + 1] != '\n' {
                    index += 1;
                }
            }
            // 块注释（可嵌套）。
            '/' if next == Some('*') => {
                let mut depth = 1;
                index += 2;
                while index < chars.len() && depth > 0 {
                    match (chars[index], chars.get(index + 1).copied()) {
                        ('/', Some('*')) => {
                            depth += 1;
                            index += 1;
                        }
                        ('*', Some('/')) => {
                            depth -= 1;
                            index += 1;
                        }
                        ('\n', _) => line += 1,
                        _ => {}
                    }
                    index += 1;
                }
                continue;
            }
            // raw 字符串 r"…" / r#"…"#（前一个字符不能是标识符的一部分）。
            'r' if matches!(next, Some('"') | Some('#'))
                && !index
                    .checked_sub(1)
                    .is_some_and(|prev| chars[prev].is_alphanumeric() || chars[prev] == '_') =>
            {
                let mut hashes = 0;
                let mut cursor = index + 1;
                while chars.get(cursor) == Some(&'#') {
                    hashes += 1;
                    cursor += 1;
                }
                if chars.get(cursor) == Some(&'"') {
                    cursor += 1;
                    let closing: String = std::iter::once('"')
                        .chain(std::iter::repeat_n('#', hashes))
                        .collect();
                    let rest: String = chars[cursor..].iter().collect();
                    let end = rest
                        .find(&closing)
                        .map_or(rest.len(), |end| end + closing.len());
                    line += rest[..end].matches('\n').count();
                    index = cursor + rest[..end].chars().count();
                    continue;
                }
            }
            // 字符字面量 'x' / '\n' / '"'；生命周期 'a 不处理。
            '\'' => {
                if next == Some('\\') {
                    if let Some(close) = (index + 3..chars.len().min(index + 14))
                        .find(|&cursor| chars[cursor] == '\'')
                    {
                        index = close + 1;
                        continue;
                    }
                } else if chars.get(index + 2) == Some(&'\'') {
                    index += 3;
                    continue;
                }
            }
            // 普通字符串。
            '"' => {
                index += 1;
                let start = index;
                let mut suspicious = Vec::new();
                while index < chars.len() && chars[index] != '"' {
                    match chars[index] {
                        // 转义：跳过下一个字符（包括行尾续行的换行）。
                        '\\' => {
                            if chars.get(index + 1) == Some(&'\n') {
                                line += 1;
                            }
                            index += 1;
                        }
                        '\n' => {
                            line += 1;
                            let indented =
                                chars[index + 1..].iter().take_while(|c| **c == ' ').count() >= 4;
                            if indented {
                                suspicious.push(line);
                            }
                        }
                        _ => {}
                    }
                    index += 1;
                }
                // SQL 里的换行和缩进无害，而且多行书写更易读。
                let literal: String = chars[start..index.min(chars.len())].iter().collect();
                if !is_sql(&literal) {
                    hits.extend(suspicious);
                }
            }
            _ => {}
        }
        index += 1;
    }
    hits
}

/// 字面量是不是一段 SQL（按开头的关键字判断）。
fn is_sql(literal: &str) -> bool {
    let head: String = literal
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    matches!(
        head.as_str(),
        "SELECT"
            | "INSERT"
            | "UPDATE"
            | "DELETE"
            | "CREATE"
            | "WITH"
            | "ALTER"
            | "DROP"
            | "PRAGMA"
            | "REPLACE"
            | "BEGIN"
    )
}

#[test]
fn the_scanner_finds_continuations_missing_their_backslash() {
    let broken = "fn f() {\n    let s = \"one\n        two\";\n}\n";
    assert_eq!(raw_newlines_followed_by_indentation(broken), vec![3]);
    let fine = "fn f() {\n    let s = \"one \\\n        two\";\n    let r = r#\"a\n    b\"#;\n    let c = '\"';\n    // \"x\n    y\n}\n";
    assert!(raw_newlines_followed_by_indentation(fine).is_empty());
}
