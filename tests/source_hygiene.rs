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
    }
    assert!(
        hits.is_empty(),
        "string literals with a run of spaces (missing `\\` line continuation?):\n{}",
        hits.join("\n")
    );
}
