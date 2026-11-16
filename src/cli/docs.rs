//! `sbot docs`：编在二进制里的使用文档。
//!
//! 文档就是仓库 `skills/` 下的那几个 skill（`SKILL.md` 加各自的参考文件），原样编进可执行
//! 文件。这样文档与程序永远是同一个版本：使用者——多半是替主人部署、运维的 agent——拿到
//! 一个二进制就拿到了与它对应的说明，不会照着另一个版本的文档操作。`--export` 把整套文件
//! 写成目录，可以直接放进 agent 的 skills 目录。
//!
//! 这条命令不读配置、不连守护进程、不碰 `~/.sbot`：还没安装好的机器上也能用，部署文档
//! 正是在那时候需要的。

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// 一份内嵌的文档。
struct Doc {
    /// 所属 skill，也是导出时的目录名。
    skill: &'static str,
    /// 在 skill 目录里的相对路径，总是用 `/` 分隔：`SKILL.md` 或 `references/<文件名>`。
    path: &'static str,
    /// 文件内容。
    text: &'static str,
}

/// 调试输出只写是哪一份：把整篇正文打出来对谁都没有用。
impl std::fmt::Debug for Doc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Doc({}/{})", self.skill, self.path)
    }
}

/// 把 `skills/<skill>/<path>` 编进来。
macro_rules! doc {
    ($skill:literal, $path:literal) => {
        Doc {
            skill: $skill,
            path: $path,
            text: include_str!(concat!("../../skills/", $skill, "/", $path)),
        }
    };
}

/// 全部内嵌文档，按“先入口、再部署、使用、运维”的阅读顺序排列；每个 skill 的 `SKILL.md`
/// 排在它的参考文件前面。
///
/// 这张表必须与 `skills/` 目录一一对应：测试会把两边互相比对，新增或删除文件而没有改这里
/// 会失败。
static DOCS: &[Doc] = &[
    doc!("sbot", "SKILL.md"),
    doc!("sbot-deploy", "SKILL.md"),
    doc!("sbot-deploy", "references/configuration.md"),
    doc!("sbot-deploy", "references/sbot.example.toml"),
    doc!("sbot-use", "SKILL.md"),
    doc!("sbot-use", "references/persona-memory.md"),
    doc!("sbot-use", "references/tools-and-vfs.md"),
    doc!("sbot-operate", "SKILL.md"),
    doc!("sbot-operate", "references/limits.md"),
    doc!("sbot-operate", "references/security.md"),
    doc!("sbot-operate", "references/troubleshooting.md"),
];

/// skill 正文的文件名。
const SKILL_FILE: &str = "SKILL.md";
/// 参考文件所在的子目录（带结尾的 `/`）。
const REFERENCES: &str = "references/";

impl Doc {
    /// 是不是 skill 的正文。
    fn is_skill(&self) -> bool {
        self.path == SKILL_FILE
    }

    /// 参考文件在命令行上的名字：Markdown 去掉 `.md`，其余文件用完整文件名。正文没有名字。
    fn reference_name(&self) -> Option<&'static str> {
        let file = self.path.strip_prefix(REFERENCES)?;
        Some(file.strip_suffix(".md").unwrap_or(file))
    }

    /// 一行标题，给清单用：Markdown 取第一个一级标题，其余文件取第一行注释。
    fn title(&self) -> &'static str {
        self.text
            .lines()
            .find_map(|line| line.strip_prefix("# "))
            .map(str::trim)
            .unwrap_or("")
    }
}

/// 执行 `sbot docs`。
///
/// - 带 `export`：把全部文件写进那个目录；
/// - 不带参数：打印清单；
/// - 一个参数：打印那个 skill 的正文；
/// - 两个参数：打印那个 skill 的一份参考文件。
pub fn run(skill: Option<&str>, reference: Option<&str>, export_to: Option<&Path>) -> Result<()> {
    let text = match (export_to, skill) {
        (Some(dir), _) => {
            let written = export(dir)?;
            format!(
                "wrote {} file(s) for sbot {} under {}\n",
                written.len(),
                env!("CARGO_PKG_VERSION"),
                dir.display()
            )
        }
        (None, None) => listing(),
        (None, Some(skill)) => find(skill, reference)?.text.to_owned(),
    };
    print(&text)
}

/// 写到标准输出。读的一方提前关了管道（`sbot docs … | head`）不算错误：要的内容对方已经
/// 拿到了。
fn print(text: &str) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => {
            Err(error).context("cannot write to stdout")
        }
        _ => Ok(()),
    }
}

/// 按名字找一份文档；`reference` 为 `None` 时是 skill 的正文。
///
/// 找不到时错误里列出可选的名字：打错一个字母不必再跑一次清单。
fn find(skill: &str, reference: Option<&str>) -> Result<&'static Doc> {
    let in_skill: Vec<&Doc> = DOCS.iter().filter(|doc| doc.skill == skill).collect();
    if in_skill.is_empty() {
        bail!(
            "unknown skill {skill:?}; available: {}",
            skill_names().join(", ")
        );
    }
    let found = match reference {
        None => in_skill.iter().find(|doc| doc.is_skill()),
        // `.md` 可写可不写：`configuration` 与 `configuration.md` 是同一份。
        Some(name) => in_skill.iter().find(|doc| {
            doc.reference_name().is_some_and(|known| {
                known == name || name.strip_suffix(".md").is_some_and(|stem| stem == known)
            })
        }),
    };
    match (found, reference) {
        (Some(doc), _) => Ok(doc),
        (None, Some(name)) => {
            let known: Vec<&str> = in_skill
                .iter()
                .filter_map(|doc| doc.reference_name())
                .collect();
            if known.is_empty() {
                bail!("skill {skill:?} has no reference files");
            }
            bail!(
                "skill {skill:?} has no reference {name:?}; available: {}",
                known.join(", ")
            );
        }
        (None, None) => unreachable!("every embedded skill has a SKILL.md (checked by tests)"),
    }
}

/// skill 的名字，按 [`DOCS`] 里的顺序，不重复。
fn skill_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    for doc in DOCS {
        if !names.contains(&doc.skill) {
            names.push(doc.skill);
        }
    }
    names
}

/// 清单：每个 skill 一行，下面缩进列出它的参考文件；末尾是用法。
fn listing() -> String {
    // 名字列对齐到最长的名字（参考文件比 skill 多缩进两格）。
    let width = DOCS
        .iter()
        .map(|doc| match doc.reference_name() {
            Some(name) => name.len() + 2,
            None => doc.skill.len(),
        })
        .max()
        .unwrap_or(0);

    let mut out = format!(
        "sbot {} 内嵌文档（与这个二进制同一个版本）\n\n",
        env!("CARGO_PKG_VERSION")
    );
    for doc in DOCS {
        let name = match doc.reference_name() {
            Some(name) => format!("  {name}"),
            None => doc.skill.to_owned(),
        };
        out.push_str(&format!("{name:<width$}  {}\n", doc.title()));
    }
    out.push_str(
        "\n\
         sbot docs <skill>              打印一个 skill\n\
         sbot docs <skill> <reference>  打印它的一份参考文件（上面缩进的名字）\n\
         sbot docs --export <DIR>       把整套文件写进 DIR（每个 skill 一个子目录）\n\
         \n\
         不知道从哪里开始：sbot docs sbot\n",
    );
    out
}

/// 把全部文档写进 `dir`：`<dir>/<skill>/SKILL.md` 与 `<dir>/<skill>/references/…`，返回写下的
/// 文件。
///
/// 同名文件直接覆盖——升级之后重新导出就是为了换掉旧内容。只写清单里的文件，目录里别的
/// 东西（使用者自己的 skill、旧版本多出来的文件）一概不动。
fn export(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut written = Vec::with_capacity(DOCS.len());
    for doc in DOCS {
        // `path` 用 `/` 分隔；逐段拼接，在 Windows 上得到本地的分隔符。
        let mut target = dir.join(doc.skill);
        target.extend(doc.path.split('/'));
        let parent = target
            .parent()
            .expect("an exported file always has a parent");
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        std::fs::write(&target, doc.text)
            .with_context(|| format!("cannot write {}", target.display()))?;
        written.push(target);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// 仓库里的 `skills/` 目录。
    fn skills_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("skills")
    }

    /// 递归收集 `dir` 下的文件，键是相对 `root` 的路径（`/` 分隔）。
    fn collect(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(root, &path, out);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.insert(relative, std::fs::read_to_string(&path).unwrap());
            }
        }
    }

    /// 内嵌清单按同样的键排好。
    fn embedded() -> BTreeMap<String, String> {
        DOCS.iter()
            .map(|doc| (format!("{}/{}", doc.skill, doc.path), doc.text.to_owned()))
            .collect()
    }

    /// 清单与 `skills/` 目录一一对应：磁盘上多一个文件（忘了加进清单）或少一个都失败。
    #[test]
    fn the_embedded_set_is_exactly_the_skills_directory() {
        let mut on_disk = BTreeMap::new();
        collect(&skills_dir(), &skills_dir(), &mut on_disk);
        let embedded = embedded();
        assert_eq!(
            embedded.keys().collect::<Vec<_>>(),
            on_disk.keys().collect::<Vec<_>>(),
            "DOCS and skills/ list different files"
        );
        assert_eq!(embedded.len(), DOCS.len(), "DOCS lists a file twice");
        for (path, text) in &on_disk {
            assert_eq!(&embedded[path], text, "{path}: embedded content differs");
        }
    }

    /// 每个 skill 都有正文，正文的 frontmatter 里 `name` 等于目录名、`description` 非空：
    /// 导出的目录要能被 agent 直接当作 skill 加载。
    #[test]
    fn every_skill_has_a_loadable_skill_file() {
        for skill in skill_names() {
            let doc = find(skill, None).unwrap();
            let mut lines = doc.text.lines();
            assert_eq!(lines.next(), Some("---"), "{skill}: no frontmatter");
            let front: Vec<&str> = lines.take_while(|line| *line != "---").collect();
            assert!(
                front.contains(&format!("name: {skill}").as_str()),
                "{skill}: frontmatter name does not match the directory"
            );
            let description = front
                .iter()
                .find_map(|line| line.strip_prefix("description: "))
                .unwrap_or_else(|| panic!("{skill}: no description"));
            assert!(description.len() > 80, "{skill}: description is too thin");
            assert!(!doc.title().is_empty(), "{skill}: no title");
        }
    }

    /// 只有两种路径：正文，或 `references/` 下的一个文件。别的布局 `reference_name` 认不出来，
    /// 文件就成了打印不出来的死文件。
    #[test]
    fn every_file_is_a_skill_or_a_reference() {
        for doc in DOCS {
            let reference = doc.reference_name();
            assert!(
                doc.is_skill() || reference.is_some_and(|name| !name.contains('/')),
                "{}/{}: neither SKILL.md nor references/<file>",
                doc.skill,
                doc.path
            );
            if let Some(name) = reference {
                assert!(std::ptr::eq(find(doc.skill, Some(name)).unwrap(), doc));
                assert!(!doc.title().is_empty(), "{}: no title", doc.path);
            }
        }
    }

    /// 内容一律是 LF：同一个版本在哪个平台上构建，打印和导出的都是同样的字节。
    #[test]
    fn embedded_text_uses_unix_line_endings() {
        for doc in DOCS {
            assert!(
                !doc.text.contains('\r'),
                "{}/{}: contains CR",
                doc.skill,
                doc.path
            );
        }
    }

    /// 文档里写的每一条 `sbot docs <skill> [<reference>]` 都真的打印得出来。
    #[test]
    fn every_docs_command_in_the_text_resolves() {
        let mut checked = 0;
        for doc in DOCS {
            for (index, _) in doc.text.match_indices("sbot docs ") {
                let rest = &doc.text[index + "sbot docs ".len()..];
                // 命令到反引号、换行或注释为止。
                let end = rest.find(['`', '\n', '#']).unwrap_or(rest.len());
                let mut words = rest[..end].split_whitespace();
                let Some(skill) = words.next() else { continue };
                // 占位符与选项不是具体的名字。
                if skill.starts_with(['<', '[', '-', '…']) {
                    continue;
                }
                // 第二个词只有看起来像名字时才算参考文件（后面可能跟着正文）。
                let reference = words.next().filter(|word| {
                    word.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
                });
                find(skill, reference).unwrap_or_else(|error| {
                    panic!("{}/{}: `sbot docs {skill} …`: {error}", doc.skill, doc.path)
                });
                checked += 1;
            }
        }
        assert!(checked > 20, "only {checked} commands were checked");
    }

    #[test]
    fn find_reports_what_is_available() {
        assert!(find("sbot-deploy", None).unwrap().is_skill());
        assert!(std::ptr::eq(
            find("sbot-deploy", Some("configuration")).unwrap(),
            find("sbot-deploy", Some("configuration.md")).unwrap()
        ));
        assert_eq!(
            find("sbot-deploy", Some("sbot.example.toml")).unwrap().path,
            "references/sbot.example.toml"
        );

        let error = find("deploy", None).unwrap_err().to_string();
        assert!(error.contains("sbot-deploy"), "{error}");
        let error = find("sbot-deploy", Some("config")).unwrap_err().to_string();
        assert!(error.contains("configuration"), "{error}");
        // 入口 skill 没有参考文件。
        let error = find("sbot", Some("anything")).unwrap_err().to_string();
        assert!(error.contains("no reference files"), "{error}");
        // 一个 skill 的参考文件不能用另一个 skill 的名字取。
        assert!(find("sbot-use", Some("configuration")).is_err());
    }

    #[test]
    fn the_listing_names_every_document() {
        let listing = listing();
        for doc in DOCS {
            let name = doc.reference_name().unwrap_or(doc.skill);
            assert!(listing.contains(name), "listing lacks {name}");
            assert!(
                listing.contains(doc.title()),
                "listing lacks the title of {name}"
            );
        }
        assert!(listing.contains(env!("CARGO_PKG_VERSION")));
    }

    /// 导出的目录与 `skills/` 一模一样；再导出一次覆盖旧内容，目录里别的文件不动。
    #[test]
    fn export_writes_the_whole_set_and_leaves_other_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("my-skill").join("SKILL.md");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, "mine").unwrap();
        let stale = dir.path().join("sbot").join("SKILL.md");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, "from an older version").unwrap();

        let written = export(dir.path()).unwrap();
        assert_eq!(written.len(), DOCS.len());

        let mut exported = BTreeMap::new();
        collect(dir.path(), dir.path(), &mut exported);
        assert_eq!(
            exported.remove("my-skill/SKILL.md").as_deref(),
            Some("mine")
        );
        assert_eq!(exported, embedded());
    }

    /// 写不进去是错误，并且说得出是哪个路径。
    #[test]
    fn export_reports_the_path_it_cannot_write() {
        let dir = tempfile::tempdir().unwrap();
        // 目标位置被一个普通文件占着，建不出子目录。
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "a file, not a directory").unwrap();
        let error = export(&blocked).unwrap_err().to_string();
        assert!(error.contains("blocked"), "{error}");
    }
}
