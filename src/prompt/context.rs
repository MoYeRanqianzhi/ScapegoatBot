//! 会话级注入上下文：会话开始与每次压缩后重建，放在静态前缀之后。
//!
//! 谁决定注入什么，是这里的核心约定（用户 2026-11-07 明确）：
//!
//! - **程序固定注入**（存在即注入，不可配置掉）：
//!   - `/AGENTS.md`：agent 通用标准的“常驻说明”，主人和 agent 都可以写；
//!   - 记忆系统自己的两个面：`/USER.md`（对待主人的方式）与 `/MEMORY.md`（索引）；
//!   - 风格系统的产物：`[style].profile_path`（默认 `/STYLE.md`，`[style].enabled` 控制）；
//!   - 技能索引：`/skills/<name>/SKILL.md` 的 name + description。
//! - **agent 自己决定注入**：`/hooks/context.toml` 里列出的文件，例如人设文件
//!   `/SOUL.md`。程序不预设任何这类文件；要不要有、注入哪些，是 agent 的选择。
//!
//! 所有读取都用“不记录读过”的接口：系统注入不是 agent 在浏览文件。
//! 文件不存在或为空时静默跳过；`/hooks/context.toml` 写错时注入一条说明，
//! 让 agent 自己看到并修正，而不是悄悄失效。

use serde::Deserialize;

use crate::vfs::sqlite::VfsStore;
use crate::vfs::{NodeKind, Vfs, VfsPath};

/// 常驻说明文件。
pub const AGENTS_PATH: &str = "/AGENTS.md";
/// 常驻说明的注入上限（字节）。
const AGENTS_MAX_BYTES: usize = 64 * 1024;
/// 记忆系统的两个面。
const USER_PATH: &str = "/USER.md";
const MEMORY_PATH: &str = "/MEMORY.md";
/// agent 自管的注入清单。
pub const HOOKS_PATH: &str = "/hooks/context.toml";
/// 清单最多几项；每项注入都要在每一轮付 token。
const MAX_CONTEXT_FILES: usize = 8;
/// 单个文件默认 / 最大注入字符数。
const DEFAULT_FILE_CHARS: u32 = 4_000;
const MAX_FILE_CHARS: u32 = 20_000;
/// 技能索引最多列多少个、每条描述最多多少字符。
const MAX_SKILLS: usize = 64;
const MAX_SKILL_DESCRIPTION_CHARS: usize = 200;

/// 注入预算：来自配置。记忆文件由 agent 用文件工具直接写，写的时候没有上限；这里的上限
/// 决定每个会话实际放进上下文多少——超出的部分被截掉，所以它们就是这些文件的有效大小。
#[derive(Debug, Clone)]
pub struct ContextBudget {
    /// `/USER.md` 的字符上限（`[memory].user_max_chars`）。
    pub user_max_chars: u32,
    /// `/MEMORY.md` 的字节上限（`[memory].max_index_bytes`）。
    pub memory_max_bytes: u32,
    /// 风格档案路径与字符上限；`[style].enabled = false` 时为 `None`。
    pub style: Option<(String, u32)>,
}

impl ContextBudget {
    /// 从配置构造。
    pub fn from_config(config: &crate::config::SbotConfig) -> Self {
        Self {
            user_max_chars: config.memory.user_max_chars,
            memory_max_bytes: config.memory.max_index_bytes,
            style: config
                .style
                .enabled
                .then(|| (config.style.profile_path.clone(), config.style.max_chars)),
        }
    }
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self::from_config(&crate::config::SbotConfig::default())
    }
}

/// `/hooks/context.toml` 的形状。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookFile {
    #[serde(default)]
    inject: Vec<InjectEntry>,
}

/// 一项注入。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InjectEntry {
    path: String,
    max_chars: Option<u32>,
}

/// 技能索引的一项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
}

/// 构建好的注入上下文。
#[derive(Debug, Clone, Default)]
pub struct InjectedContext {
    pub agents: Option<String>,
    pub user: Option<String>,
    pub memory: Option<String>,
    pub style: Option<String>,
    /// `(path, content)`，按清单顺序。
    pub files: Vec<(String, String)>,
    /// 清单本身的问题（解析失败、路径无效、超出数量……）。
    pub notices: Vec<String>,
    pub skills: Vec<SkillEntry>,
}

/// 读取全部注入内容。
pub fn load(vfs: &VfsStore, budget: &ContextBudget) -> InjectedContext {
    let mut context = InjectedContext {
        agents: read_file(vfs, AGENTS_PATH, None, Some(AGENTS_MAX_BYTES)),
        user: read_file(vfs, USER_PATH, Some(budget.user_max_chars as usize), None),
        memory: read_file(
            vfs,
            MEMORY_PATH,
            None,
            Some(budget.memory_max_bytes as usize),
        ),
        style: budget
            .style
            .as_ref()
            .and_then(|(path, max_chars)| read_file(vfs, path, Some(*max_chars as usize), None)),
        ..InjectedContext::default()
    };
    load_hook_files(vfs, budget, &mut context);
    context.skills = skill_index(vfs);
    context
}

/// 读 agent 自管的注入清单。
fn load_hook_files(vfs: &VfsStore, budget: &ContextBudget, context: &mut InjectedContext) {
    let Ok(path) = VfsPath::parse(HOOKS_PATH) else {
        return;
    };
    let Ok(text) = vfs.read_text_unrecorded(&path) else {
        // 没有清单是常态：程序不替 agent 决定注入什么。
        return;
    };
    let file: HookFile = match toml::from_str(&text) {
        Ok(file) => file,
        Err(error) => {
            context.notices.push(format!(
                "{HOOKS_PATH} is not valid ({}); nothing from it was injected.",
                error.message()
            ));
            return;
        }
    };
    // 这些已经由程序固定注入，重复列出只会浪费 token。
    let mut built_in = vec![AGENTS_PATH, USER_PATH, MEMORY_PATH];
    if let Some((style, _)) = &budget.style {
        built_in.push(style.as_str());
    }
    let mut seen = Vec::new();
    for (index, entry) in file.inject.iter().enumerate() {
        if index >= MAX_CONTEXT_FILES {
            context.notices.push(format!(
                "{HOOKS_PATH} lists {} files; only the first {MAX_CONTEXT_FILES} were injected.",
                file.inject.len()
            ));
            break;
        }
        if built_in.contains(&entry.path.as_str()) {
            context.notices.push(format!(
                "{HOOKS_PATH}: {} is always injected; remove it from the list.",
                entry.path
            ));
            continue;
        }
        if seen.contains(&entry.path) {
            continue;
        }
        seen.push(entry.path.clone());
        let max_chars = entry.max_chars.unwrap_or(DEFAULT_FILE_CHARS);
        if max_chars == 0 || max_chars > MAX_FILE_CHARS {
            context.notices.push(format!(
                "{HOOKS_PATH}: max_chars for {} must be between 1 and {MAX_FILE_CHARS}.",
                entry.path
            ));
            continue;
        }
        let Ok(path) = VfsPath::parse(&entry.path) else {
            context.notices.push(format!(
                "{HOOKS_PATH}: {:?} is not a valid VFS path.",
                entry.path
            ));
            continue;
        };
        match vfs.read_text_unrecorded(&path) {
            Ok(text) => {
                let text = truncate(&text, Some(max_chars as usize), None);
                if !text.trim().is_empty() {
                    context.files.push((entry.path.clone(), text));
                }
            }
            Err(error) => context.notices.push(format!(
                "{HOOKS_PATH}: cannot inject {}: {error}",
                entry.path
            )),
        }
    }
}

/// 扫描 `/skills/<name>/SKILL.md` 的 frontmatter，生成技能索引。
///
/// 名称以目录名为准（`Skill` 工具按目录名加载）；`.archive` 等点目录跳过。
fn skill_index(vfs: &VfsStore) -> Vec<SkillEntry> {
    let Ok(root) = VfsPath::parse("/skills") else {
        return Vec::new();
    };
    let Ok(entries) = vfs.list(&root) else {
        return Vec::new();
    };
    let mut skills = Vec::new();
    for entry in entries {
        if entry.kind != NodeKind::Dir {
            continue;
        }
        let Some(name) = entry.path.file_name() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let Ok(path) = VfsPath::parse(&format!("/skills/{name}/SKILL.md")) else {
            continue;
        };
        let Ok(text) = vfs.read_text_unrecorded(&path) else {
            continue;
        };
        let description = frontmatter_value(&text, "description").unwrap_or_default();
        let description: String = description
            .chars()
            .take(MAX_SKILL_DESCRIPTION_CHARS)
            .collect();
        skills.push(SkillEntry {
            name: name.to_owned(),
            description,
        });
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills.truncate(MAX_SKILLS);
    skills
}

/// 读 `---` 包围的 frontmatter 里的一个单行键。
fn frontmatter_value(text: &str, key: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim() == key
        {
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
            return Some(value.to_owned());
        }
    }
    None
}

/// 读一个文本文件并按预算截断；不存在、非文本或空白都返回 `None`。
fn read_file(
    vfs: &VfsStore,
    path: &str,
    max_chars: Option<usize>,
    max_bytes: Option<usize>,
) -> Option<String> {
    let path = VfsPath::parse(path).ok()?;
    let text = vfs.read_text_unrecorded(&path).ok()?;
    let text = truncate(&text, max_chars, max_bytes);
    (!text.trim().is_empty()).then_some(text)
}

/// 按字符数、再按字节数截断；字节截断落在字符边界上。
pub(super) fn truncate(
    content: &str,
    max_chars: Option<usize>,
    max_bytes: Option<usize>,
) -> String {
    let mut out = match max_chars {
        Some(max_chars) => content.chars().take(max_chars).collect(),
        None => content.to_owned(),
    };
    if let Some(max_bytes) = max_bytes
        && out.len() > max_bytes
    {
        let mut end = max_bytes;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

impl InjectedContext {
    /// 渲染成 system 文本块；顺序即优先级从高到低。
    pub fn render(&self) -> Vec<String> {
        let mut blocks = Vec::new();
        if let Some(agents) = &self.agents {
            blocks.push(format!(
                "# AGENTS.md\n\n<agents_md path=\"{AGENTS_PATH}\">\n{}\n</agents_md>",
                agents.trim_end()
            ));
        }
        if !self.files.is_empty() || !self.notices.is_empty() {
            let mut block = String::from("# Context files\n");
            for (path, content) in &self.files {
                block.push_str(&format!(
                    "\n<context_file path=\"{path}\">\n{}\n</context_file>\n",
                    content.trim_end()
                ));
            }
            for notice in &self.notices {
                block.push_str(&format!("\n<context_notice>{notice}</context_notice>\n"));
            }
            blocks.push(block.trim_end().to_owned());
        }
        if let Some(user) = &self.user {
            blocks.push(format!(
                "# User model\n\n<user_context>\n{}\n</user_context>",
                user.trim_end()
            ));
        }
        if let Some(style) = &self.style {
            blocks.push(format!(
                "# Style profile\n\n<style_profile>\n{}\n</style_profile>",
                style.trim_end()
            ));
        }
        if let Some(memory) = &self.memory {
            blocks.push(format!(
                "# Memory index\n\n<memory_context>\n{}\n</memory_context>",
                memory.trim_end()
            ));
        }
        if !self.skills.is_empty() {
            let lines: Vec<String> = self
                .skills
                .iter()
                .map(|skill| {
                    if skill.description.is_empty() {
                        format!("- {}", skill.name)
                    } else {
                        format!("- {}: {}", skill.name, skill.description)
                    }
                })
                .collect();
            blocks.push(format!("# Skill index\n\n{}", lines.join("\n")));
        }
        blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, VfsStore) {
        let dir = tempfile::tempdir().unwrap();
        let vfs = VfsStore::open(
            std::sync::Arc::new(crate::storage::Database::open_in_memory().unwrap()),
            std::sync::Arc::new(crate::storage::crypto::MasterKey::from_bytes([3u8; 32])),
            dir.path().join("data"),
            crate::vfs::quota::Limits::default(),
        )
        .unwrap();
        (dir, vfs)
    }

    /// 写文件；已存在时先完整读一次，满足“覆盖前必须读过”的约束。
    fn write(vfs: &VfsStore, path: &str, text: &str) {
        let path = VfsPath::parse(path).unwrap();
        if let Some(parent) = path.parent() {
            vfs.mkdir_all(&parent).unwrap();
        }
        if vfs.stat(&path).is_ok() {
            vfs.read(&path, None, None).unwrap();
        }
        vfs.write_text(&path, text, None).unwrap();
    }

    #[test]
    fn agents_md_is_injected_whenever_it_exists() {
        let (_dir, vfs) = store();
        assert!(load(&vfs, &ContextBudget::default()).agents.is_none());
        write(&vfs, "/AGENTS.md", "house rules");
        let context = load(&vfs, &ContextBudget::default());
        assert_eq!(context.agents.as_deref(), Some("house rules"));
        assert!(context.render()[0].contains("<agents_md path=\"/AGENTS.md\">"));
    }

    #[test]
    fn soul_md_is_only_injected_when_the_agent_lists_it() {
        let (_dir, vfs) = store();
        write(&vfs, "/SOUL.md", "a cheerful persona");
        assert!(load(&vfs, &ContextBudget::default()).files.is_empty());

        write(
            &vfs,
            HOOKS_PATH,
            "[[inject]]\npath = \"/SOUL.md\"\nmax_chars = 8\n",
        );
        let context = load(&vfs, &ContextBudget::default());
        assert_eq!(
            context.files,
            vec![("/SOUL.md".to_owned(), "a cheerf".to_owned())]
        );
        assert!(context.notices.is_empty());
    }

    #[test]
    fn a_broken_hook_list_is_reported_not_ignored() {
        let (_dir, vfs) = store();
        write(&vfs, HOOKS_PATH, "[[inject]]\npaht = \"/SOUL.md\"\n");
        let context = load(&vfs, &ContextBudget::default());
        assert!(context.files.is_empty());
        assert_eq!(context.notices.len(), 1);
        assert!(
            context
                .render()
                .iter()
                .any(|block| block.contains("<context_notice>"))
        );

        write(
            &vfs,
            HOOKS_PATH,
            "[[inject]]\npath = \"/AGENTS.md\"\n[[inject]]\npath = \"/missing.md\"\n",
        );
        let context = load(&vfs, &ContextBudget::default());
        assert_eq!(context.notices.len(), 2);
        assert!(context.notices[0].contains("always injected"));
        assert!(context.notices[1].contains("/missing.md"));
    }

    #[test]
    fn skill_index_lists_names_and_descriptions() {
        let (_dir, vfs) = store();
        write(
            &vfs,
            "/skills/weekly-report/SKILL.md",
            "---\nname: weekly-report\ndescription: 汇总本周群聊\n---\n\n1. step",
        );
        write(
            &vfs,
            "/skills/.archive/old/SKILL.md",
            "---\ndescription: old\n---\n",
        );
        let context = load(&vfs, &ContextBudget::default());
        assert_eq!(
            context.skills,
            vec![SkillEntry {
                name: "weekly-report".to_owned(),
                description: "汇总本周群聊".to_owned(),
            }]
        );
        assert!(
            context
                .render()
                .last()
                .unwrap()
                .contains("- weekly-report: 汇总本周群聊")
        );
    }

    #[test]
    fn style_profile_follows_the_style_config() {
        let (_dir, vfs) = store();
        write(&vfs, "/STYLE.md", "terse");
        let mut budget = ContextBudget::default();
        assert_eq!(load(&vfs, &budget).style.as_deref(), Some("terse"));
        budget.style = None;
        assert!(load(&vfs, &budget).style.is_none());
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let text = "你好世界";
        assert_eq!(truncate(text, Some(2), None), "你好");
        // UTF-8 下每个汉字 3 字节；4 字节只能容纳一个。
        assert_eq!(truncate(text, None, Some(4)), "你");
        assert_eq!(truncate(text, None, Some(6)), "你好");
    }
}
