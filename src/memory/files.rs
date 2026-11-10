//! 记忆文件的格式，以及后台复盘怎样把候选写进去。
//!
//! 记忆就是 VFS 里的普通文件：`/MEMORY.md` 是索引（每条一行
//! `- [slug](memory/<slug>.md) — 描述`），`/memory/<slug>.md` 是带 frontmatter 的单条记忆，
//! `/USER.md` 是主人的偏好。agent 用 `Read` / `Write` / `Edit` 直接读写它们，没有专门的
//! 记忆工具；格式是约定（写在 system prompt 里），不是校验。
//!
//! 程序自己只在一处写记忆：后台复盘（[`append_review`]）。它只追加——新建单条文件、
//! 在索引末尾加一行——从不改写或删除已有内容。

use anyhow::{Context, Result};

use crate::config::MemoryConfig;
use crate::vfs::sqlite::VfsStore;
use crate::vfs::{Vfs, VfsError, VfsPath};

/// 索引文件。
pub(crate) const INDEX_PATH: &str = "/MEMORY.md";

/// 复盘模型给出的一条候选记忆（字段都还没校验）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub name: String,
    pub kind: String,
    pub description: String,
    pub content: String,
    pub source: String,
}

/// 单条记忆可用的 `type`。
const TYPES: [&str; 4] = ["user", "feedback", "project", "reference"];

/// 从一行索引里取出它指向的 slug，例如 `- [Title](memory/<slug>.md) — hook`。
///
/// `sbot doctor` 的对账与复盘的去重共用这一个解析，两处规则不会漂移。
pub(crate) fn index_slug(line: &str) -> Option<String> {
    let start = line.find("](memory/")? + "](memory/".len();
    let rest = &line[start..];
    let end = rest.find(".md)")?;
    Some(rest[..end].to_owned())
}

/// 合法的记忆文件名：小写字母、数字与 `-`，最长 64。
fn valid_slug(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 64
        && raw
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// 把候选整理成“可以落盘的一条”；不合格的候选返回 `None`（复盘模型的输出不可信，
/// 宁可丢一条也不写半成品）。
///
/// 不合格：slug 不合法、`type` 不认识、正文为空或超过 `max_chars`、描述为空。描述与来源
/// 只取第一行——它们各占 frontmatter 的一行，多行会破坏文件格式。
fn checked(candidate: &Candidate, max_chars: usize) -> Option<Entry> {
    let slug = candidate.name.trim();
    if !valid_slug(slug) {
        return None;
    }
    let kind = TYPES.into_iter().find(|kind| *kind == candidate.kind)?;
    let content = candidate.content.trim();
    if content.is_empty() || content.chars().count() > max_chars {
        return None;
    }
    let first_line = |text: &str| text.lines().next().unwrap_or_default().trim().to_owned();
    let description = first_line(&candidate.description);
    if description.is_empty() {
        return None;
    }
    let source = first_line(&candidate.source);
    Some(Entry {
        slug: slug.to_owned(),
        kind,
        description,
        content: content.to_owned(),
        source: if source.is_empty() {
            "self".to_owned()
        } else {
            source
        },
    })
}

/// 校验过的一条记忆。
struct Entry {
    slug: String,
    kind: &'static str,
    description: String,
    content: String,
    source: String,
}

impl Entry {
    /// `/memory/<slug>.md` 的全文。
    fn file(&self, today: &str) -> String {
        format!(
            "---\nname: {}\ndescription: {}\ntype: {}\nobserved_at: {today}\nsource: {}\n---\n\n{}\n",
            self.slug, self.description, self.kind, self.source, self.content
        )
    }

    /// 索引里的一行（不含换行）。
    fn index_line(&self) -> String {
        format!(
            "- [{}](memory/{}.md) — {}",
            self.slug, self.slug, self.description
        )
    }
}

/// 把复盘候选追加进记忆：每条写一个新文件，并在 `/MEMORY.md` 末尾加一行。返回新增条数。
///
/// 只追加：
/// - 索引原文原样保留（包括 agent 自己写的标题、分组与不指向 `/memory/` 的行），新行接在
///   后面；
/// - slug 已在索引里、或 `/memory/<slug>.md` 已经存在的候选跳过：复盘不覆盖 agent 写的东西；
/// - 索引会超过 `[memory].max_index_lines` / `max_index_bytes` 时停止追加——满了由 agent
///   自己合并取舍，复盘不替它删。
///
/// 索引读不出来（解密失败、数据库错误）时返回错误而不是当成空索引：当成空的会让这次
/// 写入把整个索引替换成只有新条目。索引不存在则新建。
///
/// 写入用系统写入口（[`VfsStore::write_text_system`]）：不要求、也不伪造“读过”。agent
/// 之后想改索引必须先读到这次追加的内容。
pub(crate) fn append_review(
    vfs: &VfsStore,
    limits: &MemoryConfig,
    candidates: &[Candidate],
) -> Result<usize> {
    let index_path = VfsPath::parse(INDEX_PATH).expect("valid path");
    let mut index = match vfs.read_text_unrecorded(&index_path) {
        Ok(text) => text,
        Err(VfsError::NotFound(_)) => String::new(),
        Err(error) => return Err(error).context("cannot read /MEMORY.md"),
    };
    let mut taken: std::collections::HashSet<String> =
        index.lines().filter_map(index_slug).collect();
    let mut lines = index.lines().filter(|line| !line.trim().is_empty()).count();
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();

    let mut added = 0;
    for candidate in candidates {
        let Some(entry) = checked(candidate, limits.max_entry_chars as usize) else {
            continue;
        };
        let path = VfsPath::parse(&format!("/memory/{}.md", entry.slug)).expect("valid slug");
        if taken.contains(&entry.slug) || vfs.stat(&path).is_ok() {
            continue;
        }
        let line = entry.index_line();
        // 接在原文后面：原文不以换行结尾时先补一个。
        let separator = if index.is_empty() || index.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        if lines + 1 > limits.max_index_lines as usize
            || index.len() + separator.len() + line.len() + 1 > limits.max_index_bytes as usize
        {
            break;
        }
        // 先写单条文件、再改索引：中途失败留下的是一个没进索引的文件（`sbot doctor` 会报），
        // 而不是一条指向不存在文件的索引行。
        vfs.write_text_system(&path, &entry.file(&today))
            .with_context(|| format!("cannot write {path}"))?;
        index.push_str(separator);
        index.push_str(&line);
        index.push('\n');
        vfs.write_text_system(&index_path, &index)
            .context("cannot update /MEMORY.md")?;
        taken.insert(entry.slug);
        lines += 1;
        added += 1;
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn vfs() -> (VfsStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = VfsStore::open(
            Arc::new(crate::storage::Database::open_in_memory().unwrap()),
            Arc::new(crate::storage::crypto::MasterKey::from_bytes([8u8; 32])),
            dir.path().join("data"),
            crate::vfs::quota::Limits::default(),
        )
        .unwrap();
        (store, dir)
    }

    fn candidate(name: &str) -> Candidate {
        Candidate {
            name: name.to_owned(),
            kind: "project".to_owned(),
            description: format!("about {name}"),
            content: format!("fact about {name}"),
            source: "self".to_owned(),
        }
    }

    fn read(vfs: &VfsStore, path: &str) -> String {
        vfs.read_text_unrecorded(&VfsPath::parse(path).unwrap())
            .unwrap()
    }

    #[test]
    fn index_slug_reads_only_memory_links() {
        assert_eq!(
            index_slug("- [A](memory/a-1.md) — first").as_deref(),
            Some("a-1")
        );
        assert_eq!(index_slug("# Memory"), None);
        assert_eq!(index_slug("- [Doc](notes/x.md) — elsewhere"), None);
    }

    /// 追加写出单条文件与索引行；同一个 slug 第二次跳过。
    #[test]
    fn review_appends_a_file_and_an_index_line() {
        let (vfs, _dir) = vfs();
        let limits = MemoryConfig::default();
        assert_eq!(
            append_review(&vfs, &limits, &[candidate("phase-seven")]).unwrap(),
            1
        );
        assert_eq!(
            read(&vfs, INDEX_PATH),
            "- [phase-seven](memory/phase-seven.md) — about phase-seven\n"
        );
        let file = read(&vfs, "/memory/phase-seven.md");
        assert!(
            file.starts_with(
                "---\nname: phase-seven\ndescription: about phase-seven\ntype: project\n"
            ),
            "{file}"
        );
        assert!(
            file.ends_with("source: self\n---\n\nfact about phase-seven\n"),
            "{file}"
        );

        assert_eq!(
            append_review(&vfs, &limits, &[candidate("phase-seven")]).unwrap(),
            0
        );
    }

    /// agent 自己写的索引内容（标题、分组、没有结尾换行）原样保留；复盘不覆盖 agent 已经
    /// 写好、但还没进索引的文件。
    #[test]
    fn review_never_rewrites_what_the_agent_wrote() {
        let (vfs, _dir) = vfs();
        let limits = MemoryConfig::default();
        let index = VfsPath::parse(INDEX_PATH).unwrap();
        let original = "# Memory\n\n## People\n- [li](memory/li.md) — 小李\n\nfree-form note";
        vfs.write_text_system(&index, original).unwrap();
        let mine = VfsPath::parse("/memory/mine.md").unwrap();
        vfs.write_text(&mine, "the agent's own words", None)
            .unwrap();

        let added = append_review(
            &vfs,
            &limits,
            &[candidate("li"), candidate("mine"), candidate("new-one")],
        )
        .unwrap();
        assert_eq!(added, 1);
        assert_eq!(
            read(&vfs, INDEX_PATH),
            format!("{original}\n- [new-one](memory/new-one.md) — about new-one\n")
        );
        assert_eq!(read(&vfs, "/memory/mine.md"), "the agent's own words");
    }

    /// 不合格的候选被丢弃：slug、类型、空正文、超长正文、空描述；描述与来源只取第一行。
    #[test]
    fn malformed_candidates_are_dropped() {
        let (vfs, _dir) = vfs();
        let mut limits = MemoryConfig::default();
        limits.max_entry_chars = 20;
        let with = |edit: fn(&mut Candidate)| {
            let mut candidate = candidate("ok");
            edit(&mut candidate);
            candidate
        };
        let bad = [
            with(|c| c.name = "Has Space".to_owned()),
            with(|c| c.name = "../escape".to_owned()),
            with(|c| c.kind = "instruction".to_owned()),
            with(|c| c.content = "   ".to_owned()),
            with(|c| c.content = "x".repeat(21)),
            with(|c| c.description = "\n".to_owned()),
        ];
        assert_eq!(append_review(&vfs, &limits, &bad).unwrap(), 0);
        assert_eq!(read(&vfs, INDEX_PATH), "");

        let multi = with(|c| {
            c.description = "first line\nsecond: injected".to_owned();
            c.source = "owner\ntype: user".to_owned();
        });
        assert_eq!(append_review(&vfs, &limits, &[multi]).unwrap(), 1);
        let file = read(&vfs, "/memory/ok.md");
        assert!(file.contains("description: first line\n"), "{file}");
        assert!(file.contains("source: owner\n---"), "{file}");
        assert!(!file.contains("injected"), "{file}");
    }

    /// 索引到了行数或字节上限就停止追加，已有内容不动。
    #[test]
    fn review_stops_at_the_index_budget() {
        let (vfs, _dir) = vfs();
        let mut by_lines = MemoryConfig::default();
        by_lines.max_index_lines = 2;
        let three = [candidate("a"), candidate("b"), candidate("c")];
        assert_eq!(append_review(&vfs, &by_lines, &three).unwrap(), 2);
        assert_eq!(read(&vfs, INDEX_PATH).lines().count(), 2);
        assert!(vfs.stat(&VfsPath::parse("/memory/c.md").unwrap()).is_err());

        let (vfs, _dir) = self::vfs();
        let one_line = "- [a](memory/a.md) — about a\n".len() as u32;
        let mut by_bytes = MemoryConfig::default();
        by_bytes.max_index_bytes = one_line;
        assert_eq!(append_review(&vfs, &by_bytes, &three).unwrap(), 1);
        assert_eq!(read(&vfs, INDEX_PATH).len() as u32, one_line);
    }

    /// 索引读不出来时报错、什么都不写（以前当成空索引，会把整个索引替换成只有新条目）。
    #[test]
    fn an_unreadable_index_is_an_error_not_an_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::storage::Database::open_in_memory().unwrap());
        let open = |key: u8| {
            VfsStore::open(
                Arc::clone(&db),
                Arc::new(crate::storage::crypto::MasterKey::from_bytes([key; 32])),
                dir.path().join("data"),
                crate::vfs::quota::Limits::default(),
            )
            .unwrap()
        };
        let index = VfsPath::parse(INDEX_PATH).unwrap();
        open(8)
            .write_text_system(&index, "- [old](memory/old.md) — keep me\n")
            .unwrap();
        // 换一把密钥打开同一个数据库：索引解不开。
        let wrong = open(9);
        let error =
            append_review(&wrong, &MemoryConfig::default(), &[candidate("new")]).unwrap_err();
        assert!(format!("{error:#}").contains("/MEMORY.md"), "{error:#}");
        assert!(
            wrong
                .stat(&VfsPath::parse("/memory/new.md").unwrap())
                .is_err()
        );
        // 原索引还在，用对的密钥仍然读得出。
        assert_eq!(
            read(&open(8), INDEX_PATH),
            "- [old](memory/old.md) — keep me\n"
        );
    }

    /// 复盘的写入不替 agent 记“已读”：agent 之前读过索引，复盘追加之后它必须重读才能改。
    #[test]
    fn the_agent_must_reread_the_index_after_a_review() {
        let (vfs, _dir) = vfs();
        let index = VfsPath::parse(INDEX_PATH).unwrap();
        vfs.read(&index, None, None).unwrap();
        append_review(&vfs, &MemoryConfig::default(), &[candidate("a")]).unwrap();
        assert_eq!(
            vfs.write_text(&index, "agent overwrite", None),
            Err(VfsError::ModifiedSinceRead)
        );
        vfs.read(&index, None, None).unwrap();
        assert!(vfs.write_text(&index, "agent rewrite", None).is_ok());
    }
}
