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
use crate::storage::Database;
use crate::vfs::sqlite::VfsStore;
use crate::vfs::{FileContent, VfsPath};

use super::{Tool, ToolContext, arg_opt_str, arg_str, object_schema};

/// `Skill` 工具。
pub struct SkillTool {
    vfs: Arc<VfsStore>,
    /// 技能使用记录；curator 据此判断长期未用。
    db: Option<Arc<Database>>,
}

impl SkillTool {
    pub fn new(vfs: Arc<VfsStore>) -> Self {
        Self { vfs, db: None }
    }

    /// 接入使用记录。
    pub fn with_db(mut self, db: Arc<Database>) -> Self {
        self.db = Some(db);
        self
    }

    /// 记录一次加载；失败只影响 curator 判断，不影响工具结果。
    fn record_use(&self, name: &str) {
        let Some(db) = &self.db else {
            return;
        };
        let now = chrono::Utc::now().to_rfc3339();
        let conn = db.lock();
        let _ = conn.execute(
            "INSERT INTO skill_usage (name, last_used, uses) VALUES (?1, ?2, 1)
             ON CONFLICT(name) DO UPDATE SET last_used = excluded.last_used, uses = uses + 1",
            rusqlite::params![name, now],
        );
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
        self.record_use(name);

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

/// curator 的一次运行结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CuratorReport {
    pub archived: Vec<String>,
    pub kept: usize,
    pub skipped: Option<String>,
}

/// 技能整理：只归档长期未用技能，不删除（设计 §8.5）。
///
/// 触发条件：`[skills].curator = true`，距上次运行 ≥ `curator_interval_hours`，
/// 且新增技能数 ≥ `curator_min_new_skills`。`pinned` 技能永不自动归档。
pub fn curate(
    vfs: &VfsStore,
    db: &Database,
    config: &crate::config::SkillsConfig,
) -> anyhow::Result<CuratorReport> {
    use crate::vfs::Vfs;

    let mut report = CuratorReport::default();
    if !config.curator {
        report.skipped = Some("curator is disabled".to_owned());
        return Ok(report);
    }
    let interval_hours = config.curator_interval_hours.max(1) as i64;
    let interval = chrono::Duration::hours(interval_hours);
    let now = chrono::Utc::now();
    let last_run = db
        .meta_get("skills.curator.last_run")?
        .and_then(|bytes| String::from_utf8(bytes).ok());
    if let Some(last) = &last_run
        && let Ok(last) = chrono::DateTime::parse_from_rfc3339(last)
        && now.signed_duration_since(last.with_timezone(&chrono::Utc)) < interval
    {
        report.skipped = Some("interval has not elapsed".to_owned());
        return Ok(report);
    }

    let root = VfsPath::parse("/skills").expect("valid path");
    let entries = vfs.list(&root)?;
    let skills: Vec<_> = entries
        .into_iter()
        .filter(|entry| {
            entry.kind == crate::vfs::NodeKind::Dir && entry.path.file_name() != Some(".archive")
        })
        .collect();
    let new_count = match &last_run {
        None => skills.len(),
        Some(last) => skills
            .iter()
            .filter(|entry| entry.created_at.as_str() > last.as_str())
            .count(),
    };
    if new_count < config.curator_min_new_skills as usize {
        report.skipped = Some(format!(
            "only {new_count} new skill(s); need {}",
            config.curator_min_new_skills
        ));
        return Ok(report);
    }

    let archive = VfsPath::parse("/skills/.archive").expect("valid path");
    vfs.mkdir_all(&archive)?;
    let cutoff = (now - interval).to_rfc3339();
    for entry in &skills {
        let name = entry.path.file_name().unwrap_or_default().to_owned();
        let pinned = vfs
            .stat(&VfsPath::parse(&format!("/skills/{name}/.pinned")).expect("valid"))
            .is_ok()
            || vfs
                .read_text_unrecorded(
                    &VfsPath::parse(&format!("/skills/{name}/SKILL.md")).expect("valid"),
                )
                .map(|text| text.contains("pinned: true"))
                .unwrap_or(false);
        if pinned {
            report.kept += 1;
            continue;
        }

        let last_used: Option<String> = {
            let conn = db.lock();
            conn.query_row(
                "SELECT last_used FROM skill_usage WHERE name = ?1",
                rusqlite::params![name],
                |row| row.get(0),
            )
            .ok()
        };
        let reference = last_used.unwrap_or_else(|| entry.created_at.clone());
        if reference >= cutoff {
            report.kept += 1;
            continue;
        }
        let target = VfsPath::parse(&format!("/skills/.archive/{name}")).expect("valid");
        match vfs.rename(&entry.path, &target) {
            Ok(()) => report.archived.push(name),
            Err(_) => report.kept += 1,
        }
    }

    db.meta_set("skills.curator.last_run", now.to_rfc3339().as_bytes())?;
    Ok(report)
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

    #[test]
    fn curator_archives_unused_but_keeps_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([11u8; 32]));
        let vfs = Arc::new(
            VfsStore::open(
                Arc::clone(&db),
                key,
                dir.path().join("data"),
                Limits::default(),
            )
            .unwrap(),
        );
        for name in ["old", "pinned"] {
            vfs.write_text(
                &VfsPath::parse(&format!("/skills/{name}/SKILL.md")).unwrap(),
                &format!("---\nname: {name}\n---\n\nbody\n"),
                None,
            )
            .unwrap();
        }
        vfs.write_text(
            &VfsPath::parse("/skills/pinned/.pinned").unwrap(),
            "keep",
            None,
        )
        .unwrap();

        // 距上次运行 200 小时，超过 168 小时的默认间隔。
        let last_run = (chrono::Utc::now() - chrono::Duration::hours(200)).to_rfc3339();
        db.meta_set("skills.curator.last_run", last_run.as_bytes())
            .unwrap();
        // old 技能 500 小时未使用；pinned 未使用但有 pin 标记。
        let old = (chrono::Utc::now() - chrono::Duration::hours(500)).to_rfc3339();
        db.lock()
            .execute(
                "INSERT INTO skill_usage (name, last_used, uses) VALUES ('old', ?1, 1)",
                rusqlite::params![old],
            )
            .unwrap();

        let config = crate::config::SkillsConfig {
            curator: true,
            curator_interval_hours: 168,
            curator_min_new_skills: 1,
            ..crate::config::SkillsConfig::default()
        };
        let report = curate(&vfs, &db, &config).unwrap();
        assert_eq!(report.archived, vec!["old".to_owned()]);
        assert!(vfs.stat(&VfsPath::parse("/skills/old").unwrap()).is_err());
        assert!(
            vfs.stat(&VfsPath::parse("/skills/.archive/old").unwrap())
                .is_ok()
        );
        assert!(vfs.stat(&VfsPath::parse("/skills/pinned").unwrap()).is_ok());
    }
}
