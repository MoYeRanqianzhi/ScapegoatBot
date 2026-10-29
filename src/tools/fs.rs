//! 文件工具：`Read` / `Write` / `Edit` / `LS` / `Grep` / `Glob` / `RM` / `MV` / `CP` / `Mkdir` / `Stat`。
//!
//! 设计依据：设计 §5.2 / §5.7 / §6.3 / §6.5。
//!
//! - 没有 Bash；这些工具就是 agent 的全部文件能力。
//! - 路径永远是 VFS 虚拟路径；工具不接受宿主路径。
//! - 动态视图（`/messages`、`/inbox`、`/sessions` 等）由 `tools::views` 渲染；
//!   静态文件走 `VfsStore`。
//! - `Read` 的 `offset` / `limit` 是**行号**；带分页的读取不算全文读取，
//!   不满足 `Edit` / 覆盖写的前置条件（设计 §6.5）。

use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};

use crate::agent::{ExecutionMode, ToolResult};
use crate::vfs::{FileContent, NodeKind, Vfs, VfsError, VfsPath};

use super::{
    Permission, Tool, ToolContext, arg_opt_bool, arg_opt_str, arg_opt_u64, arg_str, object_schema,
    parse_path, parse_target, render_content, vfs_error, views,
};

/// 单次 Grep 的文件上限（每个直接子目录）。
const GREP_MAX_FILES: usize = 200;
/// 单次 Grep 的匹配上限。
const GREP_MAX_MATCHES: usize = 200;

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// 读取文件。
pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "Read"
    }

    fn description(&self) -> &'static str {
        "Read a VFS file. `path` is an absolute VFS path. For text files, returns the content; \
         `offset` and `limit` select lines. For images, returns content; for other binary files, \
         returns metadata only. A full text read (no offset/limit) is required before `Edit` or \
         overwriting with `Write`.\n\n\
         Dynamic views accept query parameters: `/inbox?filter=from:10001&limit=20`, \
         `/messages/groups/123/2026-10?filter=contains(\"活动\")&order=desc`. `limit` defaults \
         to 50 (max 500). Whatever this call returns is marked read; items beyond `limit` stay \
         unread. Use `LS` or `Stat` when you only want names and counts."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "path": {"type": "string", "description": "Absolute VFS path, optionally with query parameters on dynamic views."},
                "offset": {"type": "number", "description": "First line to read (0-based)."},
                "limit": {"type": "number", "description": "Maximum number of lines to read."}
            }),
            &["path"],
        )
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let target = match parse_target(arg_str(&args, "path").unwrap_or_default()) {
            Ok(target) => target,
            Err(error) => return error,
        };
        let offset = match arg_opt_u64(&args, "offset") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let limit = match arg_opt_u64(&args, "limit") {
            Ok(value) => value,
            Err(error) => return error,
        };

        if views::is_dynamic_target(&target) {
            let Some(views) = &ctx.views else {
                return ToolResult::error("Dynamic views are not available in this runtime.");
            };
            return match views.read(&target).await {
                Ok(views::ViewRead::Text(text)) => ToolResult::ok(text),
                Ok(views::ViewRead::Metadata { kind, size, mime }) => {
                    ToolResult::ok(format!("{kind} ({} bytes, {mime})", size))
                }
                Err(error) => vfs_error(error),
            };
        }

        match ctx.vfs.read_lines(&target.path, offset, limit) {
            Ok(content) => ToolResult::ok(render_content(&target.path, content)),
            Err(error) => vfs_error(error),
        }
    }
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// 创建或覆盖文本文件。
pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "Write"
    }

    fn description(&self) -> &'static str {
        "Create or overwrite a UTF-8 text file in the VFS. Parent directories are created. \
         Overwriting an existing file requires a full prior `Read`; creating a new file does not. \
         Returns path, bytes, and version. Use `Download` for binary files; use `Edit` for small \
         changes."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "path": {"type": "string", "description": "Absolute VFS path."},
                "content": {"type": "string", "description": "UTF-8 text to write."}
            }),
            &["path", "content"],
        )
    }

    fn permissions(&self) -> &'static [Permission] {
        &[Permission::Write]
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let path = match parse_path(arg_str(&args, "path").unwrap_or_default()) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let Ok(content) = arg_str(&args, "content") else {
            return ToolResult::error("Missing required parameter \"content\" (string).");
        };
        // `/config/**` 与 `/config.toml` 走受限写入：白名单 + 校验 + 原子写回。
        if views::Views::is_config_path(&path) {
            let Some(views) = &ctx.views else {
                return ToolResult::error("Configuration views are not available in this runtime.");
            };
            let target = crate::vfs::path::ParsedTarget {
                path: path.clone(),
                query: None,
            };
            return match views.write_config(&target, content) {
                Ok(message) => ToolResult::ok(message),
                Err(error) => ToolResult::error(error.to_string()),
            };
        }
        if views::Views::handles(&path) {
            return ToolResult::error("Dynamic views are read-only.");
        }
        match ctx.vfs.write_text(&path, content, None) {
            Ok(meta) => ToolResult::ok(format!(
                "Wrote {} bytes to {} (version {}).",
                meta.size, meta.path, meta.version
            )),
            Err(error) => vfs_error(error),
        }
    }
}

// ---------------------------------------------------------------------------
// Edit
// ---------------------------------------------------------------------------

/// 精确替换。
pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "Edit"
    }

    fn description(&self) -> &'static str {
        "Replace exact text in a UTF-8 VFS file. Requires a full prior `Read` of the current \
         version. `old_text` must match exactly and must be unique unless `replace_all` is true. \
         Returns replacement count and new version. If the file changed since the read, the match \
         is missing, or the match is ambiguous, re-read and try again."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "path": {"type": "string", "description": "Absolute VFS path."},
                "old_text": {"type": "string", "description": "Exact text to replace."},
                "new_text": {"type": "string", "description": "Replacement text."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)."}
            }),
            &["path", "old_text", "new_text"],
        )
    }

    fn permissions(&self) -> &'static [Permission] {
        &[Permission::Write]
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let path = match parse_path(arg_str(&args, "path").unwrap_or_default()) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let (Ok(old_text), Ok(new_text)) = (arg_str(&args, "old_text"), arg_str(&args, "new_text"))
        else {
            return ToolResult::error("Parameters \"old_text\" and \"new_text\" are required.");
        };
        let replace_all = match arg_opt_bool(&args, "replace_all") {
            Ok(value) => value,
            Err(error) => return error,
        };
        // 配置叶子：读取当前值、做精确替换，再走受限写入。
        if views::Views::is_config_path(&path) {
            let Some(views) = &ctx.views else {
                return ToolResult::error("Configuration views are not available in this runtime.");
            };
            let target = crate::vfs::path::ParsedTarget {
                path: path.clone(),
                query: None,
            };
            let current = match views.read(&target).await {
                Ok(views::ViewRead::Text(text)) => text,
                Ok(_) => return ToolResult::error("Configuration leaves are text."),
                Err(error) => return ToolResult::error(error.to_string()),
            };
            if !current.contains(old_text) {
                return ToolResult::error(format!("old_text was not found in {path}"));
            }
            let updated = if replace_all {
                current.replace(old_text, new_text)
            } else {
                current.replacen(old_text, new_text, 1)
            };
            return match views.write_config(&target, &updated) {
                Ok(message) => ToolResult::ok(message),
                Err(error) => ToolResult::error(error.to_string()),
            };
        }
        if views::Views::handles(&path) {
            return ToolResult::error("Dynamic views are read-only.");
        }
        match ctx
            .vfs
            .edit_text_count(&path, old_text, new_text, replace_all)
        {
            Ok((meta, count)) => ToolResult::ok(format!(
                "Replaced {count} occurrence(s) in {} (version {}).",
                meta.path, meta.version
            )),
            Err(error) => vfs_error(error),
        }
    }
}

// ---------------------------------------------------------------------------
// LS
// ---------------------------------------------------------------------------

/// 列目录。
pub struct LsTool;

#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &'static str {
        "LS"
    }

    fn description(&self) -> &'static str {
        "List a VFS directory. `recursive` includes descendants. Returns type, size, mtime, and \
         path; does not return file content. Accepts the same query parameters as `Read` on \
         dynamic views. Listing never marks anything read."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "path": {"type": "string", "description": "Absolute VFS path."},
                "recursive": {"type": "boolean", "description": "Include descendants."}
            }),
            &["path"],
        )
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let target = match parse_target(arg_str(&args, "path").unwrap_or_default()) {
            Ok(target) => target,
            Err(error) => return error,
        };
        let recursive = match arg_opt_bool(&args, "recursive") {
            Ok(value) => value,
            Err(error) => return error,
        };

        if views::is_dynamic_target(&target) {
            let Some(views) = &ctx.views else {
                return ToolResult::error("Dynamic views are not available in this runtime.");
            };
            let entries = if recursive {
                views.walk(&target.path).await
            } else {
                views.list(&target).await
            };
            return match entries {
                Ok(entries) => ToolResult::ok(render_entries(&entries)),
                Err(error) => vfs_error(error),
            };
        }

        let entries = if recursive {
            ctx.vfs.list_recursive(&target.path)
        } else {
            ctx.vfs.list(&target.path)
        };
        match entries {
            Ok(entries) => {
                let rendered: Vec<super::views::ViewEntry> = entries
                    .into_iter()
                    .map(|meta| super::views::ViewEntry {
                        path: meta.path.to_string(),
                        kind: if meta.kind == NodeKind::Dir {
                            "dir"
                        } else {
                            "file"
                        },
                        size: meta.size,
                        mtime: meta.updated_at,
                    })
                    .collect();
                ToolResult::ok(render_entries(&rendered))
            }
            Err(error) => vfs_error(error),
        }
    }
}

fn render_entries(entries: &[super::views::ViewEntry]) -> String {
    if entries.is_empty() {
        return "(empty)".to_owned();
    }
    entries
        .iter()
        .map(|entry| {
            format!(
                "{:<4} {:>10} {} {}",
                entry.kind, entry.size, entry.mtime, entry.path
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Grep
// ---------------------------------------------------------------------------

/// 内容搜索。
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "Grep"
    }

    fn description(&self) -> &'static str {
        "Search text file content under a VFS path. `pattern` is a regular expression; `path` \
         defaults to `/`; `file_ext` filters extensions; `case_insensitive` ignores case.\n\n\
         Large `/messages` and `/sessions` trees are budgeted per direct child. A child that \
         exceeds the budget is skipped or partially scanned, while matches from other children \
         are still returned. The result ends with `[skipped]` or `[partial]` notes only for those \
         children; these notes are informational. Do not scan entire histories.\n\n\
         Matched entries are marked read; files only scanned are not."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "pattern": {"type": "string", "description": "Regular expression."},
                "path": {"type": "string", "description": "Root to search (default /)."},
                "file_ext": {"type": "string", "description": "Only files with this extension."},
                "case_insensitive": {"type": "boolean", "description": "Case-insensitive match."}
            }),
            &["pattern"],
        )
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let pattern = match arg_str(&args, "pattern") {
            Ok(pattern) => pattern,
            Err(error) => return error,
        };
        let case_insensitive = match arg_opt_bool(&args, "case_insensitive") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let regex = match regex::RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .build()
        {
            Ok(regex) => regex,
            Err(error) => return ToolResult::error(format!("Invalid regular expression: {error}")),
        };
        let root = match parse_path(arg_opt_str(&args, "path").unwrap_or("/")) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let file_ext = arg_opt_str(&args, "file_ext");

        if views::Views::handles(&root) {
            let Some(views) = &ctx.views else {
                return ToolResult::error("Dynamic views are not available in this runtime.");
            };
            return match views.grep(&root, &regex, file_ext).await {
                Ok(matches) => ToolResult::ok(render_matches(&matches, &[])),
                Err(error) => vfs_error(error),
            };
        }

        // 静态树：按直接子节点分别计量预算（设计 §5.7）。
        let mut matches: Vec<(String, u64, String)> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        let mut roots: Vec<VfsPath> = Vec::new();
        match ctx.vfs.list(&root) {
            Ok(children) => {
                for child in children {
                    roots.push(child.path);
                }
            }
            Err(_) => roots.push(root.clone()),
        }

        'outer: for child in roots {
            // 直接子节点可能是文件：文件直接扫描，目录才递归。
            let entries = match ctx.vfs.stat(&child) {
                Ok(meta) if meta.kind != NodeKind::Dir => vec![meta],
                Ok(_) => match ctx.vfs.list_recursive(&child) {
                    Ok(entries) => entries,
                    Err(_) => continue,
                },
                Err(_) => continue,
            };
            let mut scanned = 0usize;
            for entry in entries {
                if entry.kind == NodeKind::Dir || entry.kind != NodeKind::Text {
                    continue;
                }
                if let Some(ext) = file_ext
                    && !entry.path.to_string().ends_with(&format!(".{ext}"))
                {
                    continue;
                }
                if scanned >= GREP_MAX_FILES {
                    skipped.push(format!("{} (file budget)", child));
                    continue 'outer;
                }
                scanned += 1;
                let Ok(text) = ctx.vfs.read_text_unrecorded(&entry.path) else {
                    continue;
                };
                let mut matched_this_file = false;
                for (line_no, line) in text.lines().enumerate() {
                    if regex.is_match(line) {
                        matches.push((entry.path.to_string(), line_no as u64 + 1, line.to_owned()));
                        matched_this_file = true;
                        if matches.len() >= GREP_MAX_MATCHES {
                            skipped.push(format!("{} (match budget)", child));
                            break;
                        }
                    }
                }
                if matched_this_file {
                    // 只把实际出现在结果里的文件记为“读过”。
                    let _ = ctx.vfs.record_read_full(&entry.path);
                }
                if matches.len() >= GREP_MAX_MATCHES {
                    break 'outer;
                }
            }
        }

        ToolResult::ok(render_matches(&matches, &skipped))
    }
}

fn render_matches(matches: &[(String, u64, String)], skipped: &[String]) -> String {
    let mut out = matches
        .iter()
        .map(|(path, line, text)| format!("{path}:{line}: {text}"))
        .collect::<Vec<_>>()
        .join("\n");
    if out.is_empty() {
        out.push_str("(no matches)");
    }
    for path in skipped {
        out.push_str(&format!("\n[skipped] {path}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Glob
// ---------------------------------------------------------------------------

/// 文件名/路径匹配。
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "Glob"
    }

    fn description(&self) -> &'static str {
        "Find VFS paths matching a glob pattern. Returns matching paths only; no file content. \
         Use `LS` for a directory listing and `Grep` for content search."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "pattern": {"type": "string", "description": "Glob pattern, e.g. **/*.md."},
                "path": {"type": "string", "description": "Root to search (default /)."}
            }),
            &["pattern"],
        )
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let pattern = match arg_str(&args, "pattern") {
            Ok(pattern) => pattern,
            Err(error) => return error,
        };
        let root = match parse_path(arg_opt_str(&args, "path").unwrap_or("/")) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let regex = match glob_to_regex(pattern) {
            Ok(regex) => regex,
            Err(error) => return ToolResult::error(error),
        };

        let entries = if views::Views::handles(&root) {
            let Some(views) = &ctx.views else {
                return ToolResult::error("Dynamic views are not available in this runtime.");
            };
            match views.walk(&root).await {
                Ok(entries) => entries,
                Err(error) => return vfs_error(error),
            }
        } else {
            match ctx.vfs.list_recursive(&root) {
                Ok(entries) => entries
                    .into_iter()
                    .map(|meta| super::views::ViewEntry {
                        path: meta.path.to_string(),
                        kind: if meta.kind == NodeKind::Dir {
                            "dir"
                        } else {
                            "file"
                        },
                        size: meta.size,
                        mtime: meta.updated_at,
                    })
                    .collect(),
                Err(error) => return vfs_error(error),
            }
        };

        let mut paths: Vec<String> = entries
            .into_iter()
            .filter(|entry| regex.is_match(&entry.path))
            .map(|entry| entry.path)
            .collect();
        paths.sort();
        if paths.is_empty() {
            ToolResult::ok("(no matches)")
        } else {
            ToolResult::ok(paths.join("\n"))
        }
    }
}

/// 把 glob 转成正则：支持 `**`、`*`、`?`，其余字符转义。
fn glob_to_regex(pattern: &str) -> Result<Regex, String> {
    let mut regex = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '*' if index + 1 < chars.len() && chars[index + 1] == '*' => {
                regex.push_str(".*");
                index += 2;
            }
            '*' => {
                regex.push_str("[^/]*");
                index += 1;
            }
            '?' => {
                regex.push_str("[^/]");
                index += 1;
            }
            other => {
                regex.push_str(&regex::escape(&other.to_string()));
                index += 1;
            }
        }
    }
    regex.push('$');
    Regex::new(&regex).map_err(|error| format!("Invalid glob pattern: {error}"))
}

// ---------------------------------------------------------------------------
// RM
// ---------------------------------------------------------------------------

/// 删除 / 忽略。
pub struct RmTool;

#[async_trait]
impl Tool for RmTool {
    fn name(&self) -> &'static str {
        "RM"
    }

    fn description(&self) -> &'static str {
        "Remove a file or directory. Directories require `recursive: true`. Removal moves the \
         item to `/trash` unless `permanent: true`.\n\n\
         On `/inbox`, removal means dismiss: the entry leaves the inbox without being marked \
         read. `RM /inbox/<chat>/<message_id>.md` dismisses one message; \
         `RM /inbox/<chat>/index.md` dismisses every unread message from that chat; \
         `RM /inbox?filter=...` dismisses the matches and the filter is required. Dismissing \
         never deletes the underlying message: it stays searchable under `/messages`."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "path": {"type": "string", "description": "Absolute VFS path."},
                "recursive": {"type": "boolean", "description": "Required for directories."},
                "permanent": {"type": "boolean", "description": "Delete instead of moving to /trash."}
            }),
            &["path"],
        )
    }

    fn permissions(&self) -> &'static [Permission] {
        &[Permission::Delete]
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let target = match parse_target(arg_str(&args, "path").unwrap_or_default()) {
            Ok(target) => target,
            Err(error) => return error,
        };
        let recursive = match arg_opt_bool(&args, "recursive") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let permanent = match arg_opt_bool(&args, "permanent") {
            Ok(value) => value,
            Err(error) => return error,
        };

        if target
            .path
            .segments()
            .first()
            .is_some_and(|root| root == "inbox")
        {
            let Some(views) = &ctx.views else {
                return ToolResult::error("Dynamic views are not available in this runtime.");
            };
            return match views.dismiss(&target) {
                Ok(count) => ToolResult::ok(format!(
                    "Dismissed {count} inbox entr{}.",
                    if count == 1 { "y" } else { "ies" }
                )),
                Err(error) => vfs_error(error),
            };
        }
        if views::Views::handles(&target.path) {
            return ToolResult::error("Dynamic views are read-only.");
        }

        // 目录必须显式 recursive。
        match ctx.vfs.stat(&target.path) {
            Ok(meta) if meta.kind == NodeKind::Dir && !recursive => {
                return ToolResult::error("Directory removal requires \"recursive\": true.");
            }
            Ok(_) => {}
            Err(error) => return vfs_error(error),
        }

        match ctx.vfs.remove(&target.path, permanent) {
            Ok(outcome) => match outcome.trashed_to {
                Some(path) => ToolResult::ok(format!("Moved {} to {}.", target.path, path)),
                None => ToolResult::ok(format!(
                    "Permanently removed {} ({} nodes).",
                    target.path, outcome.removed_nodes
                )),
            },
            Err(error) => vfs_error(error),
        }
    }
}

// ---------------------------------------------------------------------------
// MV / CP / Mkdir / Stat
// ---------------------------------------------------------------------------

/// 移动或重命名。
pub struct MvTool;

#[async_trait]
impl Tool for MvTool {
    fn name(&self) -> &'static str {
        "MV"
    }

    fn description(&self) -> &'static str {
        "Move or rename a VFS file or directory. `from` and `to` are absolute VFS paths; both \
         mounts must be writable. Directories move recursively. Returns the new path."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "from": {"type": "string", "description": "Source path."},
                "to": {"type": "string", "description": "Destination path."}
            }),
            &["from", "to"],
        )
    }

    fn permissions(&self) -> &'static [Permission] {
        &[Permission::Write]
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let (Ok(from), Ok(to)) = (
            parse_path(arg_str(&args, "from").unwrap_or_default()),
            parse_path(arg_str(&args, "to").unwrap_or_default()),
        ) else {
            return ToolResult::error("Parameters \"from\" and \"to\" are required.");
        };
        if views::Views::handles(&from) || views::Views::handles(&to) {
            return ToolResult::error("Dynamic views are read-only.");
        }
        match ctx.vfs.rename(&from, &to) {
            Ok(()) => ToolResult::ok(format!("Moved {from} to {to}.")),
            Err(error) => vfs_error(error),
        }
    }
}

/// 复制。
pub struct CpTool;

#[async_trait]
impl Tool for CpTool {
    fn name(&self) -> &'static str {
        "CP"
    }

    fn description(&self) -> &'static str {
        "Copy a VFS file or directory. Directories require `recursive: true`. Checks source \
         readability, target mount, and quota. Returns target path and bytes copied."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "from": {"type": "string", "description": "Source path."},
                "to": {"type": "string", "description": "Destination path."},
                "recursive": {"type": "boolean", "description": "Required for directories."}
            }),
            &["from", "to"],
        )
    }

    fn permissions(&self) -> &'static [Permission] {
        &[Permission::Write]
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let (Ok(from), Ok(to)) = (
            parse_path(arg_str(&args, "from").unwrap_or_default()),
            parse_path(arg_str(&args, "to").unwrap_or_default()),
        ) else {
            return ToolResult::error("Parameters \"from\" and \"to\" are required.");
        };
        let recursive = match arg_opt_bool(&args, "recursive") {
            Ok(value) => value,
            Err(error) => return error,
        };
        if views::Views::handles(&from) || views::Views::handles(&to) {
            return ToolResult::error("Dynamic views are read-only.");
        }
        match ctx.vfs.stat(&from) {
            Ok(meta) if meta.kind == NodeKind::Dir && !recursive => {
                return ToolResult::error("Directory copy requires \"recursive\": true.");
            }
            Ok(_) => {}
            Err(error) => return vfs_error(error),
        }
        match ctx.vfs.copy(&from, &to) {
            Ok(()) => {
                let size = ctx.vfs.stat(&to).map(|meta| meta.size).unwrap_or(0);
                ToolResult::ok(format!("Copied {from} to {to} ({size} bytes)."))
            }
            Err(error) => vfs_error(error),
        }
    }
}

/// 创建目录。
pub struct MkdirTool;

#[async_trait]
impl Tool for MkdirTool {
    fn name(&self) -> &'static str {
        "Mkdir"
    }

    fn description(&self) -> &'static str {
        "Create a VFS directory. `parents: true` creates missing parents. Creating an existing \
         directory succeeds. Returns the path."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "path": {"type": "string", "description": "Directory path."},
                "parents": {"type": "boolean", "description": "Create missing parents."}
            }),
            &["path"],
        )
    }

    fn permissions(&self) -> &'static [Permission] {
        &[Permission::Write]
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let path = match parse_path(arg_str(&args, "path").unwrap_or_default()) {
            Ok(path) => path,
            Err(error) => return error,
        };
        if views::Views::handles(&path) {
            return ToolResult::error("Dynamic views are read-only.");
        }
        let parents = match arg_opt_bool(&args, "parents") {
            Ok(value) => value,
            Err(error) => return error,
        };
        if !parents
            && let Some(parent) = path.parent()
            && !parent.is_root()
            && ctx.vfs.stat(&parent).is_err()
        {
            return ToolResult::error(format!(
                "Parent directory {parent} does not exist; retry with \"parents\": true."
            ));
        }
        match ctx.vfs.mkdir_all(&path) {
            Ok(()) => ToolResult::ok(format!("Created {path}.")),
            Err(error) => vfs_error(error),
        }
    }
}

/// 元信息。
pub struct StatTool;

#[async_trait]
impl Tool for StatTool {
    fn name(&self) -> &'static str {
        "Stat"
    }

    fn description(&self) -> &'static str {
        "Return VFS metadata: type, size, mtime, version, content hash, child count, mount, and \
         permissions. Does not read content."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({"path": {"type": "string", "description": "Absolute VFS path."}}),
            &["path"],
        )
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let path = match parse_path(arg_str(&args, "path").unwrap_or_default()) {
            Ok(path) => path,
            Err(error) => return error,
        };
        if views::Views::handles(&path) {
            let Some(views) = &ctx.views else {
                return ToolResult::error("Dynamic views are not available in this runtime.");
            };
            return match views.stat(&path).await {
                Ok(entry) => ToolResult::ok(format!(
                    "path: {}\ntype: {}\nsize: {}\nmtime: {}",
                    entry.path, entry.kind, entry.size, entry.mtime
                )),
                Err(error) => vfs_error(error),
            };
        }
        match ctx.vfs.stat(&path) {
            Ok(meta) => {
                let children = if meta.kind == NodeKind::Dir {
                    ctx.vfs.list(&path).map(|items| items.len()).unwrap_or(0)
                } else {
                    0
                };
                ToolResult::ok(format!(
                    "path: {}\ntype: {}\nsize: {}\nmime: {}\nversion: {}\ncreated: {}\nupdated: {}\nhash: {}\nchildren: {}\npermissions: rw",
                    meta.path,
                    meta.kind.as_str(),
                    meta.size,
                    meta.mime.unwrap_or_else(|| "-".to_owned()),
                    meta.version,
                    meta.created_at,
                    meta.updated_at,
                    meta.hash.unwrap_or_else(|| "-".to_owned()),
                    children
                ))
            }
            Err(error) => vfs_error(error),
        }
    }
}

// ---------------------------------------------------------------------------
// 注册
// ---------------------------------------------------------------------------

/// 注册全部文件工具。
pub fn register(registry: &mut super::ToolRegistry) {
    registry.register(Arc::new(ReadTool));
    registry.register(Arc::new(WriteTool));
    registry.register(Arc::new(EditTool));
    registry.register(Arc::new(LsTool));
    registry.register(Arc::new(GrepTool));
    registry.register(Arc::new(GlobTool));
    registry.register(Arc::new(RmTool));
    registry.register(Arc::new(MvTool));
    registry.register(Arc::new(CpTool));
    registry.register(Arc::new(MkdirTool));
    registry.register(Arc::new(StatTool));
}

/// 供测试或其他模块使用：解析并读取一个静态文本。
pub async fn read_static(ctx: &ToolContext, raw_path: &str) -> Result<String, VfsError> {
    let path = VfsPath::parse(raw_path)?;
    match ctx.vfs.read_lines(&path, None, None)? {
        FileContent::Text { text, .. } => Ok(text),
        FileContent::Metadata(meta) => Ok(format!("{} {}", meta.path, meta.kind.as_str())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ToolCall;
    use crate::agent::ToolExecutor;
    use crate::config::EncryptionMode;
    use crate::session::SessionStore;
    use crate::storage::Database;
    use crate::storage::crypto::MasterKey;
    use crate::storage::messages::MessageStore;
    use crate::tools::views::Views;
    use crate::vfs::quota::Limits;
    use crate::vfs::sqlite::VfsStore;

    type Registry = super::super::ToolRegistry;

    fn setup() -> (Registry, Arc<VfsStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([2u8; 32]));
        let vfs = Arc::new(
            VfsStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                Limits::default(),
            )
            .unwrap(),
        );
        let messages = Arc::new(
            MessageStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let sessions = Arc::new(
            SessionStore::open(
                Arc::clone(&db),
                Arc::clone(&key),
                dir.path().join("data"),
                0,
                EncryptionMode::XChaCha20Poly1305,
            )
            .unwrap(),
        );
        let views = Arc::new(Views::new(Arc::clone(&vfs), messages, sessions, None));
        let mut registry = Registry::new(Arc::clone(&vfs), Some(views));
        register(&mut registry);
        (registry, vfs, dir)
    }

    async fn run(registry: &Registry, name: &str, args: Value) -> ToolResult {
        registry
            .execute(&ToolCall {
                id: "t".to_owned(),
                name: name.to_owned(),
                arguments: args,
            })
            .await
    }

    #[tokio::test]
    async fn write_read_and_edit_flow() {
        let (registry, _vfs, _dir) = setup();

        let result = run(
            &registry,
            "Write",
            json!({"path": "/notes/a.md", "content": "one two"}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);

        let result = run(&registry, "Read", json!({"path": "/notes/a.md"})).await;
        assert_eq!(result.content, "one two");

        let result = run(
            &registry,
            "Edit",
            json!({"path": "/notes/a.md", "old_text": "two", "new_text": "three"}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("version 2"));

        let result = run(&registry, "Read", json!({"path": "/notes/a.md"})).await;
        assert_eq!(result.content, "one three");
    }

    #[tokio::test]
    async fn partial_read_blocks_edit() {
        let (registry, _vfs, _dir) = setup();
        run(
            &registry,
            "Write",
            json!({"path": "/a.md", "content": "line1\nline2\nline3"}),
        )
        .await;

        let result = run(
            &registry,
            "Read",
            json!({"path": "/a.md", "offset": 1, "limit": 1}),
        )
        .await;
        assert!(result.content.contains("line2"));
        assert!(result.content.contains("partial read"));

        let result = run(
            &registry,
            "Edit",
            json!({"path": "/a.md", "old_text": "line2", "new_text": "x"}),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("part of the file"));
    }

    #[tokio::test]
    async fn overwrite_requires_read() {
        let (registry, vfs, _dir) = setup();
        vfs.write_text(&VfsPath::parse("/a.md").unwrap(), "one", None)
            .unwrap();
        vfs.clear_reads();

        let result = run(
            &registry,
            "Write",
            json!({"path": "/a.md", "content": "two"}),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("has not been read"));

        run(&registry, "Read", json!({"path": "/a.md"})).await;
        let result = run(
            &registry,
            "Write",
            json!({"path": "/a.md", "content": "two"}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
    }

    #[tokio::test]
    async fn ls_glob_grep_and_stat() {
        let (registry, _vfs, _dir) = setup();
        for (path, text) in [
            ("/docs/a.md", "hello world"),
            ("/docs/b.txt", "hello rust"),
            ("/other/c.md", "nothing"),
        ] {
            run(&registry, "Write", json!({"path": path, "content": text})).await;
        }

        let result = run(&registry, "LS", json!({"path": "/", "recursive": true})).await;
        assert!(result.content.contains("/docs/a.md"));
        assert!(result.content.contains("/other/c.md"));

        let result = run(&registry, "Glob", json!({"pattern": "**/*.md"})).await;
        assert!(result.content.contains("/docs/a.md"));
        assert!(!result.content.contains("b.txt"));

        let result = run(&registry, "Grep", json!({"pattern": "hello"})).await;
        assert!(result.content.contains("/docs/a.md:1: hello world"));
        assert!(result.content.contains("/docs/b.txt:1: hello rust"));

        let result = run(&registry, "Stat", json!({"path": "/docs"})).await;
        assert!(result.content.contains("type: dir"));
        assert!(result.content.contains("children: 2"));
    }

    #[tokio::test]
    async fn grep_marks_only_matched_files_read() {
        let (registry, vfs, _dir) = setup();
        vfs.write_text(&VfsPath::parse("/hit.md").unwrap(), "needle", None)
            .unwrap();
        vfs.write_text(&VfsPath::parse("/miss.md").unwrap(), "nothing", None)
            .unwrap();
        vfs.clear_reads();

        run(&registry, "Grep", json!({"pattern": "needle"})).await;

        // 命中的文件可直接编辑；未命中的仍然要求先读。
        let hit = run(
            &registry,
            "Edit",
            json!({"path": "/hit.md", "old_text": "needle", "new_text": "x"}),
        )
        .await;
        assert!(!hit.is_error, "{}", hit.content);

        let miss = run(
            &registry,
            "Edit",
            json!({"path": "/miss.md", "old_text": "nothing", "new_text": "x"}),
        )
        .await;
        assert!(miss.is_error);
    }

    #[tokio::test]
    async fn rm_trash_mv_and_cp() {
        let (registry, _vfs, _dir) = setup();
        run(
            &registry,
            "Write",
            json!({"path": "/a.md", "content": "keep"}),
        )
        .await;

        let result = run(&registry, "RM", json!({"path": "/a.md"})).await;
        assert!(result.content.contains("/trash/"), "{}", result.content);
        let result = run(&registry, "Read", json!({"path": "/a.md"})).await;
        assert!(result.is_error);

        run(&registry, "Mkdir", json!({"path": "/dst", "parents": true})).await;
        let result = run(
            &registry,
            "Write",
            json!({"path": "/src.md", "content": "copy me"}),
        )
        .await;
        assert!(!result.is_error);
        let result = run(
            &registry,
            "CP",
            json!({"from": "/src.md", "to": "/dst/copy.md"}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let result = run(
            &registry,
            "MV",
            json!({"from": "/dst/copy.md", "to": "/dst/moved.md"}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let result = run(&registry, "Read", json!({"path": "/dst/moved.md"})).await;
        assert_eq!(result.content, "copy me");
    }

    #[tokio::test]
    async fn mkdir_without_parents_requires_existing_parent() {
        let (registry, _vfs, _dir) = setup();
        let result = run(&registry, "Mkdir", json!({"path": "/a/b"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("parents"));
        let result = run(&registry, "Mkdir", json!({"path": "/a/b", "parents": true})).await;
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn directory_removal_requires_recursive() {
        let (registry, _vfs, _dir) = setup();
        run(&registry, "Mkdir", json!({"path": "/d", "parents": true})).await;
        let result = run(&registry, "RM", json!({"path": "/d"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("recursive"));
        let result = run(&registry, "RM", json!({"path": "/d", "recursive": true})).await;
        assert!(!result.is_error, "{}", result.content);
    }
}
