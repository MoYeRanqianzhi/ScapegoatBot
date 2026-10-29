//! `Skill` 工具：加载 `/skills/<name>/SKILL.md` 与资源清单。
//!
//! 设计依据：设计 §5.2「技能机制」。
//!
//! - 技能是“这类任务怎么做”的流程，记忆是事实；两者不混。
//! - 脚本永远不执行，只作为文本资源列出。
//! - 加载会记录“读过”（内容进入上下文）。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::ToolResult;
use crate::vfs::sqlite::VfsStore;
use crate::vfs::{FileContent, VfsPath};

use super::{Tool, ToolContext, arg_opt_str, arg_str, object_schema};

/// `Skill` 工具。
pub struct SkillTool {
    vfs: Arc<VfsStore>,
}

impl SkillTool {
    pub fn new(vfs: Arc<VfsStore>) -> Self {
        Self { vfs }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &'static str {
        "Skill"
    }

    fn description(&self) -> &'static str {
        "Load the full instructions for a skill by `name`. Returns SKILL.md and a list of \
         resources. Scripts are text only and are never executed. Use only when the skill \
         matches the task. Skills hold procedures; memory holds facts. When a loaded skill is \
         wrong or missing a step, patch it rather than appending a correction note."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "name": {"type": "string", "description": "Skill directory name."},
                "args": {"type": "string", "description": "Optional arguments for the task."}
            }),
            &["name"],
        )
    }

    async fn execute(&self, _ctx: &ToolContext, args: Value) -> ToolResult {
        let name = match arg_str(&args, "name") {
            Ok(name) => name,
            Err(error) => return error,
        };
        let valid = !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if !valid {
            return ToolResult::error(
                "Skill name must use lowercase letters, digits, '-' or '_' (max 64).",
            );
        }

        let directory = match VfsPath::parse(&format!("/skills/{name}")) {
            Ok(path) => path,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        let skill_path = match VfsPath::parse(&format!("/skills/{name}/SKILL.md")) {
            Ok(path) => path,
            Err(error) => return ToolResult::error(error.to_string()),
        };

        let content = match self.vfs.read_lines(&skill_path, None, None) {
            Ok(FileContent::Text { text, .. }) => text,
            Ok(_) => return ToolResult::error("SKILL.md is not a text file."),
            Err(error) => return ToolResult::error(error.to_string()),
        };

        let mut resources = Vec::new();
        if let Ok(entries) = self.vfs.list_recursive(&directory) {
            for entry in entries {
                if entry.kind == crate::vfs::NodeKind::Dir {
                    continue;
                }
                let path = entry.path.to_string();
                if path.ends_with("/SKILL.md") {
                    continue;
                }
                resources.push(path);
            }
        }

        let args_note = arg_opt_str(&args, "args")
            .map(|value| format!("\n\n[skill arguments]\n{value}"))
            .unwrap_or_default();
        let mut out = format!("{content}{args_note}");
        if resources.is_empty() {
            out.push_str("\n\n[resources] none");
        } else {
            out.push_str("\n\n[resources]\n");
            out.push_str(&resources.join("\n"));
        }
        ToolResult::ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::storage::Database;
    use crate::storage::crypto::MasterKey;
    use crate::vfs::Vfs;
    use crate::vfs::quota::Limits;

    fn setup() -> (SkillTool, Arc<VfsStore>) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([10u8; 32]));
        let vfs =
            Arc::new(VfsStore::open(db, key, dir.path().join("data"), Limits::default()).unwrap());
        vfs.mkdir_all(&VfsPath::parse("/skills/demo/scripts").unwrap())
            .unwrap();
        vfs.write_text(
            &VfsPath::parse("/skills/demo/SKILL.md").unwrap(),
            "---\nname: demo\ndescription: demo skill\n---\n\nDo the thing.\n",
            None,
        )
        .unwrap();
        vfs.write_text(
            &VfsPath::parse("/skills/demo/scripts/run.sh").unwrap(),
            "echo hi",
            None,
        )
        .unwrap();
        (SkillTool::new(Arc::clone(&vfs)), vfs)
    }

    fn context() -> ToolContext {
        let dir = tempfile::tempdir().unwrap();
        let vfs = Arc::new(
            VfsStore::open(
                Arc::new(Database::open_in_memory().unwrap()),
                Arc::new(MasterKey::from_bytes([1u8; 32])),
                dir.path().join("data"),
                Limits::default(),
            )
            .unwrap(),
        );
        ToolContext::new(vfs, None)
    }

    #[tokio::test]
    async fn loads_skill_and_lists_resources() {
        let (tool, _vfs) = setup();
        let result = tool.execute(&context(), json!({"name": "demo"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("Do the thing."));
        assert!(result.content.contains("/skills/demo/scripts/run.sh"));
    }

    #[tokio::test]
    async fn rejects_bad_names_and_missing_skills() {
        let (tool, _vfs) = setup();
        let result = tool.execute(&context(), json!({"name": "../etc"})).await;
        assert!(result.is_error);
        let result = tool.execute(&context(), json!({"name": "missing"})).await;
        assert!(result.is_error);
    }
}
