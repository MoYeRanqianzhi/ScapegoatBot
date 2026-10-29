//! `Memory` 工具：`/USER.md`、`/memory/<slug>.md` 与 `/MEMORY.md` 索引的唯一写入口。
//!
//! 设计依据：设计 §8.5 / `.agents/plan/2026-10-29-claude-memory-format.md`。
//!
//! 规则：
//! - `/USER.md` 写“主人希望被如何对待”（祈使句）；单条记忆写事实（陈述句）。
//! - `/MEMORY.md` 每条一行：`- [Title](memory/<slug>.md) — hook`；≤200 行 / 25k 字节。
//! - 单条记忆 ≤ 4000 字符，带 frontmatter（name/description/type/observed_at/source）。
//! - `ops` 是原子批量：预算按**最终状态**检查，允许“先删后加”。
//! - 拒绝清空索引；来源原样记录，不对来源做信任升级。
//! - 由文件工具直接写 `/MEMORY.md` 的路径被 VFS 层允许，但系统提示与工具描述明确
//!   `Memory` 是唯一入口；Phase 5 的记忆门控按 frontmatter 的 source 决定是否注入。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::ToolResult;
use crate::vfs::sqlite::VfsStore;
use crate::vfs::{Vfs, VfsPath};

use super::{Permission, Tool, ToolContext, arg_opt_str, arg_str, object_schema};

/// 记忆类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryType {
    User,
    Feedback,
    Project,
    Reference,
}

impl MemoryType {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "user" => Self::User,
            "feedback" => Self::Feedback,
            "project" => Self::Project,
            "reference" => Self::Reference,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }
}

/// 一条记忆的最终状态。
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    slug: String,
    description: String,
    kind: MemoryType,
    content: String,
    source: String,
}

/// 一次操作。
#[derive(Debug, Clone)]
enum Op {
    Add(Entry),
    Replace(Entry),
    Remove { slug: String },
}

/// `Memory` 工具。
pub struct MemoryTool {
    vfs: Arc<VfsStore>,
    max_entry_chars: usize,
    max_index_lines: usize,
    max_index_bytes: usize,
    user_max_chars: usize,
}

impl MemoryTool {
    pub fn new(
        vfs: Arc<VfsStore>,
        max_entry_chars: u32,
        max_index_lines: u32,
        max_index_bytes: u32,
        user_max_chars: u32,
    ) -> Self {
        Self {
            vfs,
            max_entry_chars: max_entry_chars as usize,
            max_index_lines: max_index_lines as usize,
            max_index_bytes: max_index_bytes as usize,
            user_max_chars: user_max_chars as usize,
        }
    }

    /// 解析 `memory` 类操作。
    fn parse_op(&self, params: &Value, action: &str) -> Result<Op, ToolResult> {
        match action {
            "add" | "replace" => {
                let kind = arg_opt_str(params, "type")
                    .and_then(MemoryType::parse)
                    .ok_or_else(|| {
                        ToolResult::error(
                            "Parameter \"type\" must be one of: user, feedback, project, reference.",
                        )
                    })?;
                let slug = validate_slug(arg_str(params, "name").unwrap_or_default())?;
                let description = arg_str(params, "description")
                    .unwrap_or_default()
                    .to_owned();
                let content = arg_str(params, "content").unwrap_or_default().to_owned();
                if description.trim().is_empty() {
                    return Err(ToolResult::error("Parameter \"description\" is required."));
                }
                if content.trim().is_empty() {
                    return Err(ToolResult::error("Parameter \"content\" is required."));
                }
                if content.chars().count() > self.max_entry_chars {
                    return Err(ToolResult::error(format!(
                        "Memory content is {} characters; the limit is {}.",
                        content.chars().count(),
                        self.max_entry_chars
                    )));
                }
                let source = arg_opt_str(params, "source").unwrap_or("self").to_owned();
                let entry = Entry {
                    slug,
                    description,
                    kind,
                    content,
                    source,
                };
                Ok(if action == "add" {
                    Op::Add(entry)
                } else {
                    Op::Replace(entry)
                })
            }
            "remove" => Ok(Op::Remove {
                slug: validate_slug(arg_str(params, "name").unwrap_or_default())?,
            }),
            other => Err(ToolResult::error(format!(
                "Unknown action {other:?}; use add, replace or remove."
            ))),
        }
    }

    /// 读取索引原文。
    fn read_index(&self) -> Result<String, ToolResult> {
        let path = VfsPath::parse("/MEMORY.md").expect("valid path");
        match self.vfs.read_text_unrecorded(&path) {
            Ok(text) => Ok(text),
            Err(_) => Ok(String::new()),
        }
    }

    /// 渲染单条记忆文件。
    fn render_entry(entry: &Entry) -> String {
        format!(
            "---\nname: {}\ndescription: {}\ntype: {}\nobserved_at: {}\nsource: {}\n---\n\n{}\n",
            entry.slug,
            entry.description.lines().next().unwrap_or_default(),
            entry.kind.as_str(),
            chrono::Utc::now().format("%Y-%m-%d"),
            entry.source,
            entry.content.trim_end()
        )
    }

    /// 索引行。
    fn index_line(entry: &Entry) -> String {
        format!(
            "- [{}](memory/{}.md) — {}",
            entry.slug, entry.slug, entry.description
        )
    }

    /// 把索引文本解析成 `(前置内容, slug → 行, 顺序)`。
    fn parse_index(text: &str) -> (Vec<String>, Vec<(String, String)>) {
        let mut prefix = Vec::new();
        let mut entries: Vec<(String, String)> = Vec::new();
        for line in text.lines() {
            if let Some(slug) = index_slug(line) {
                entries.push((slug, line.to_owned()));
            } else if !line.trim().is_empty() {
                prefix.push(line.to_owned());
            }
        }
        (prefix, entries)
    }

    /// 模拟并校验最终状态，然后落盘。
    fn apply(&self, ops: Vec<Op>) -> Result<Value, ToolResult> {
        let index_text = self.read_index()?;
        let (prefix, mut entries) = Self::parse_index(&index_text);
        let mut files: Vec<(String, String)> = Vec::new();
        let mut removals: Vec<String> = Vec::new();
        let mut applied = Vec::new();

        for op in ops {
            match op {
                Op::Add(entry) => {
                    if entries.iter().any(|(slug, _)| slug == &entry.slug) {
                        return Err(ToolResult::error(format!(
                            "Memory {:?} already exists; use replace.",
                            entry.slug
                        )));
                    }
                    entries.push((entry.slug.clone(), Self::index_line(&entry)));
                    files.push((entry.slug.clone(), Self::render_entry(&entry)));
                    applied.push(json!({"added": entry.slug}));
                }
                Op::Replace(entry) => {
                    let Some(slot) = entries.iter_mut().find(|(slug, _)| slug == &entry.slug)
                    else {
                        return Err(ToolResult::error(format!(
                            "Memory {:?} does not exist; use add.",
                            entry.slug
                        )));
                    };
                    slot.1 = Self::index_line(&entry);
                    files.push((entry.slug.clone(), Self::render_entry(&entry)));
                    applied.push(json!({"replaced": entry.slug}));
                }
                Op::Remove { slug } => {
                    let before = entries.len();
                    entries.retain(|(existing, _)| existing != &slug);
                    if entries.len() == before {
                        return Err(ToolResult::error(format!(
                            "Memory {slug:?} does not exist."
                        )));
                    }
                    removals.push(slug.clone());
                    applied.push(json!({"removed": slug}));
                }
            }
        }

        if entries.is_empty() {
            return Err(ToolResult::error(
                "Refusing to empty /MEMORY.md; the index must keep at least one entry.",
            ));
        }
        if entries.len() > self.max_index_lines {
            return Err(ToolResult::error(format!(
                "The memory index would have {} lines; the limit is {}.",
                entries.len(),
                self.max_index_lines
            )));
        }

        // 渲染最终索引（保持原有顺序，新增追加在末尾）。
        let mut index = String::new();
        for line in &prefix {
            index.push_str(line);
            index.push('\n');
        }
        if !prefix.is_empty() {
            index.push('\n');
        }
        for (_, line) in &entries {
            index.push_str(line);
            index.push('\n');
        }
        if index.len() > self.max_index_bytes {
            return Err(ToolResult::error(format!(
                "The memory index would be {} bytes; the limit is {}.",
                index.len(),
                self.max_index_bytes
            )));
        }

        // 先写条目文件，再写索引；索引写失败时回滚新文件。
        let created: Vec<VfsPath> = files
            .iter()
            .map(|(slug, _)| VfsPath::parse(&format!("/memory/{slug}.md")).expect("valid"))
            .collect();
        for ((slug, body), path) in files.iter().zip(&created) {
            self.ensure_memory_readable(path)?;
            if let Err(error) = self.vfs.write_text(path, body, None) {
                return Err(ToolResult::error(format!(
                    "Cannot write memory {slug}: {error}"
                )));
            }
        }
        let index_path = VfsPath::parse("/MEMORY.md").expect("valid");
        self.ensure_memory_readable(&index_path)?;
        if let Err(error) = self.vfs.write_text(&index_path, &index, None) {
            for path in &created {
                let _ = self.vfs.remove(path, true);
            }
            return Err(ToolResult::error(format!(
                "Cannot update the memory index: {error}"
            )));
        }
        for slug in &removals {
            let path = VfsPath::parse(&format!("/memory/{slug}.md")).expect("valid");
            let _ = self.vfs.remove(&path, true);
        }

        Ok(json!({"ok": true, "operations": applied}))
    }

    /// 记忆工具自己拥有这些文件：写入前先读取，满足 Read-before-Edit。
    fn ensure_memory_readable(&self, path: &VfsPath) -> Result<(), ToolResult> {
        if self.vfs.stat(path).is_ok() {
            let _ = self.vfs.read_lines(path, None, None);
        }
        Ok(())
    }
}

fn validate_slug(raw: &str) -> Result<String, ToolResult> {
    let raw = raw.trim();
    let ok = !raw.is_empty()
        && raw.len() <= 64
        && raw
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        return Err(ToolResult::error(
            "Memory name must be a kebab-case slug: lowercase letters, digits and '-', up to 64 characters.",
        ));
    }
    Ok(raw.to_owned())
}

fn index_slug(line: &str) -> Option<String> {
    // 匹配 `- [Title](memory/<slug>.md) — hook`。
    let start = line.find("](memory/")? + "](memory/".len();
    let rest = &line[start..];
    let end = rest.find(".md)")?;
    Some(rest[..end].to_owned())
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &'static str {
        "Memory"
    }

    fn description(&self) -> &'static str {
        "Save durable memory. `target` is profile (/USER.md: how the owner wants to be treated) or \
         memory (one file under /memory/ plus one line in /MEMORY.md).\n\n\
         action is add, replace, or remove. For memory, add requires type (user, feedback, \
         project, reference), name (kebab-case), description (one specific line), content and \
         source. For profile, only content is needed. Use ops to apply several changes \
         atomically: budgets are checked against the final state, so a remove and an add in one \
         call can free room.\n\n\
         Lead with the rule or fact. For feedback and project, add **Why:** and **How to apply:**. \
         Write profile as directives, memory as declarative statements. Never save secrets, \
         transient chatter, or task progress; procedures belong in skills. The index is limited \
         to 200 lines / 25k bytes, each memory to 4k characters, /USER.md to 4k characters. \
         Never empty the index."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "target": {"type": "string", "enum": ["profile", "memory"]},
                "action": {"type": "string", "enum": ["add", "replace", "remove"]},
                "type": {"type": "string", "enum": ["user", "feedback", "project", "reference"]},
                "name": {"type": "string", "description": "Kebab-case slug."},
                "description": {"type": "string"},
                "content": {"type": "string"},
                "source": {"type": "string", "description": "chat_id + message_id, control:<id>, or self."},
                "ops": {"type": "array", "description": "Batch operations applied atomically."}
            }),
            &["target", "action"],
        )
    }

    fn permissions(&self) -> &'static [Permission] {
        &[Permission::Write]
    }

    fn execution_mode(&self) -> crate::agent::ExecutionMode {
        crate::agent::ExecutionMode::Sequential
    }

    async fn execute(&self, _ctx: &ToolContext, args: Value) -> ToolResult {
        let target = match arg_str(&args, "target") {
            Ok(target) => target,
            Err(error) => return error,
        };
        match target {
            "profile" => self.profile(&args),
            "memory" => self.memory(&args),
            other => ToolResult::error(format!("Unknown target {other:?}; use profile or memory.")),
        }
    }
}

impl MemoryTool {
    fn profile(&self, args: &Value) -> ToolResult {
        let action = match arg_str(args, "action") {
            Ok(action) => action,
            Err(error) => return error,
        };
        let content = match action {
            "add" | "replace" => match arg_str(args, "content") {
                Ok(content) => content,
                Err(error) => return error,
            },
            "remove" => {
                return ToolResult::error(
                    "Refusing to remove /USER.md; replace it with updated directives instead.",
                );
            }
            other => {
                return ToolResult::error(format!("Unknown action {other:?}; use add or replace."));
            }
        };
        let path = VfsPath::parse("/USER.md").expect("valid");
        let current = self.vfs.read_text_unrecorded(&path).unwrap_or_default();
        let next = if action == "add" && !current.trim().is_empty() {
            format!("{}\n\n{}", current.trim_end(), content.trim())
        } else {
            content.trim().to_owned()
        };
        if next.chars().count() > self.user_max_chars {
            return ToolResult::error(format!(
                "/USER.md would be {} characters; the limit is {}.",
                next.chars().count(),
                self.user_max_chars
            ));
        }
        if self.vfs.stat(&path).is_ok() {
            let _ = self.vfs.read_lines(&path, None, None);
        }
        match self.vfs.write_text(&path, &next, None) {
            Ok(meta) => ToolResult::ok(
                json!({"ok": true, "path": "/USER.md", "version": meta.version}).to_string(),
            ),
            Err(error) => ToolResult::error(error.to_string()),
        }
    }

    fn memory(&self, args: &Value) -> ToolResult {
        // 批量 ops 优先。
        if let Some(ops) = args.get("ops").and_then(Value::as_array) {
            let mut parsed = Vec::with_capacity(ops.len());
            for op in ops {
                let action = match arg_str(op, "action") {
                    Ok(action) => action,
                    Err(error) => return error,
                };
                match self.parse_op(op, action) {
                    Ok(op) => parsed.push(op),
                    Err(error) => return error,
                }
            }
            return self
                .apply(parsed)
                .map(|value| ToolResult::ok(value.to_string()))
                .unwrap_or_else(|error| error);
        }

        let action = match arg_str(args, "action") {
            Ok(action) => action,
            Err(error) => return error,
        };
        let op = match self.parse_op(args, action) {
            Ok(op) => op,
            Err(error) => return error,
        };
        self.apply(vec![op])
            .map(|value| ToolResult::ok(value.to_string()))
            .unwrap_or_else(|error| error)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::storage::Database;
    use crate::storage::crypto::MasterKey;
    use crate::vfs::quota::Limits;

    fn setup() -> (MemoryTool, Arc<VfsStore>) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([6u8; 32]));
        let vfs =
            Arc::new(VfsStore::open(db, key, dir.path().join("data"), Limits::default()).unwrap());
        // 目录需要存在。
        vfs.mkdir_all(&VfsPath::parse("/memory").unwrap()).unwrap();
        let tool = MemoryTool::new(Arc::clone(&vfs), 4000, 200, 25_000, 4000);
        (tool, vfs)
    }

    async fn run(tool: &MemoryTool, args: Value) -> ToolResult {
        tool.execute(
            &ToolContext::new(
                // 这个工具只用得到 vfs，但仍需要满足上下文签名。
                Arc::new(
                    crate::vfs::sqlite::VfsStore::open(
                        Arc::new(Database::open_in_memory().unwrap()),
                        Arc::new(MasterKey::from_bytes([1u8; 32])),
                        tempfile::tempdir().unwrap().keep(),
                        Limits::default(),
                    )
                    .unwrap(),
                ),
                None,
            ),
            args,
        )
        .await
    }

    #[tokio::test]
    async fn add_writes_file_and_index() {
        let (tool, vfs) = setup();
        let result = run(
            &tool,
            json!({
                "target": "memory",
                "action": "add",
                "type": "user",
                "name": "prefers-short-replies",
                "description": "Owner prefers short replies",
                "content": "The owner prefers short replies in group chats.",
                "source": "self"
            }),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);

        let index = vfs
            .read_text_unrecorded(&VfsPath::parse("/MEMORY.md").unwrap())
            .unwrap();
        assert!(index.contains("- [prefers-short-replies](memory/prefers-short-replies.md) — Owner prefers short replies"));
        let entry = vfs
            .read_text_unrecorded(&VfsPath::parse("/memory/prefers-short-replies.md").unwrap())
            .unwrap();
        assert!(entry.contains("type: user"));
        assert!(entry.contains("source: self"));
    }

    #[tokio::test]
    async fn duplicate_add_is_rejected_and_replace_works() {
        let (tool, vfs) = setup();
        let base = json!({
            "target": "memory",
            "action": "add",
            "type": "project",
            "name": "rebuild",
            "description": "Rebuild in progress",
            "content": "Version 0.2 rebuild.",
        });
        assert!(!run(&tool, base.clone()).await.is_error);

        let mut duplicate = base.clone();
        duplicate["action"] = json!("add");
        let result = run(&tool, duplicate).await;
        assert!(result.is_error);
        assert!(result.content.contains("already exists"));

        let mut replace = base.clone();
        replace["action"] = json!("replace");
        replace["content"] = json!("Rebuild finished.");
        let result = run(&tool, replace).await;
        assert!(!result.is_error, "{}", result.content);
        let entry = vfs
            .read_text_unrecorded(&VfsPath::parse("/memory/rebuild.md").unwrap())
            .unwrap();
        assert!(entry.contains("Rebuild finished."));
    }

    #[tokio::test]
    async fn remove_deletes_file_and_line() {
        let (tool, vfs) = setup();
        for (name, description, content) in [
            ("docs", "Docs site", "https://example.com"),
            ("other", "Another note", "kept"),
        ] {
            run(
                &tool,
                json!({
                    "target": "memory", "action": "add", "type": "reference",
                    "name": name, "description": description, "content": content
                }),
            )
            .await;
        }
        let result = run(
            &tool,
            json!({"target": "memory", "action": "remove", "name": "docs"}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let index = vfs
            .read_text_unrecorded(&VfsPath::parse("/MEMORY.md").unwrap())
            .unwrap();
        assert!(!index.contains("docs"));
        assert!(index.contains("other"));
        assert!(
            vfs.stat(&VfsPath::parse("/memory/docs.md").unwrap())
                .is_err()
        );
    }

    #[tokio::test]
    async fn batch_ops_check_final_state() {
        let (tool, _vfs) = setup();
        let result = run(
            &tool,
            json!({
                "target": "memory", "action": "add",
                "ops": [
                    {"action": "add", "type": "user", "name": "a", "description": "A", "content": "A fact"},
                    {"action": "add", "type": "user", "name": "b", "description": "B", "content": "B fact"},
                    {"action": "remove", "name": "a"}
                ]
            }),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let value: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(value["operations"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn invalid_slug_and_type_are_rejected() {
        let (tool, _vfs) = setup();
        for (name, kind) in [("Bad Slug", "user"), ("ok", "nope")] {
            let result = run(
                &tool,
                json!({
                    "target": "memory", "action": "add", "type": kind,
                    "name": name, "description": "x", "content": "y"
                }),
            )
            .await;
            assert!(result.is_error);
        }
    }

    #[tokio::test]
    async fn profile_is_bounded_and_directive() {
        let (tool, vfs) = setup();
        let result = run(
            &tool,
            json!({"target": "profile", "action": "add", "content": "Prefer short replies."}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let result = run(
            &tool,
            json!({"target": "profile", "action": "add", "content": "Address the owner as Boss."}),
        )
        .await;
        assert!(!result.is_error);
        let text = vfs
            .read_text_unrecorded(&VfsPath::parse("/USER.md").unwrap())
            .unwrap();
        assert!(text.contains("Prefer short replies."));
        assert!(text.contains("Address the owner as Boss."));
    }

    #[test]
    fn index_parser_extracts_slugs() {
        let (prefix, entries) = MemoryTool::parse_index(
            "# Memory\n\n- [A](memory/a.md) — first\n- [B](memory/b.md) — second\n",
        );
        assert_eq!(prefix, vec!["# Memory"]);
        assert_eq!(entries[0].0, "a");
        assert_eq!(entries[1].0, "b");
    }
}
