// src/skills/mod.rs
// Skill 系统模块
// 参考 SA 的 SkillRegistry 模式：
// - Skill 是保存在本地目录中的说明包，每个技能目录至少包含一个 SKILL.md
// - 通过 cap-std 限制文件访问在各 skill 目录内，防止路径遍历
// - LLM 通过内置 Skill 工具按名称加载，不暴露宿主机真实路径

use anyhow::{Context as _, bail};
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Skill 文件读取的最大字节数（200 KB）
/// 防止过大的文件撑爆上下文窗口
const MAX_SKILL_FILE_BYTES: u64 = 200_000;

// ============================================================================
// 数据结构
// ============================================================================

/// SKILL.md 的 YAML Front Matter 结构
/// 只需要 name 和 description 两个字段
#[derive(Debug, Clone)]
struct SkillFrontmatter {
    /// Skill 唯一名称标识符
    name: String,
    /// Skill 的简要描述
    description: String,
}

/// 已注册的 Skill 条目
/// 通过 cap-std Dir 句柄限制所有文件访问在 skill 目录内
pub struct Skill {
    /// Skill 名称
    pub name: String,
    /// Skill 描述
    pub description: String,
    /// cap-std 沙箱目录句柄（限制在此 skill 的目录内）
    dir: Dir,
}

/// Skill 列表项（用于 prompt 注入）
/// 不含文件句柄，可安全地 Clone/Debug
#[derive(Debug, Clone)]
pub struct SkillListItem {
    /// Skill 名称
    pub name: String,
    /// Skill 描述
    pub description: String,
}

// ============================================================================
// SkillRegistry — Skill 注册表
// ============================================================================

/// Skill 注册表
/// 管理所有已发现的 Skill，提供按名称查找和文件加载功能
/// 所有文件访问通过 cap-std 沙箱保护
pub struct SkillRegistry {
    /// 按名称索引的 Skill 映射
    by_name: HashMap<String, Skill>,
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self {
            by_name: HashMap::new(),
        }
    }
}

impl SkillRegistry {
    /// 扫描指定目录列表，发现并注册所有 Skill
    /// 每个目录下的子目录如果包含 SKILL.md 则被视为一个 Skill
    /// 同名 Skill 保留首次发现的实例
    ///
    /// # 参数
    /// - `dirs`: 要扫描的目录路径列表
    pub fn scan(dirs: &[PathBuf]) -> anyhow::Result<Self> {
        let mut by_name = HashMap::new();

        for dir in dirs {
            if !dir.exists() {
                tracing::debug!("Skill 目录不存在，跳过: {}", dir.display());
                continue;
            }

            Self::scan_directory(dir, &mut by_name);
        }

        tracing::info!("已注册 {} 个 Skill", by_name.len());
        Ok(Self { by_name })
    }

    /// 扫描单个目录，查找包含 SKILL.md 的子目录
    fn scan_directory(root: &Path, registry: &mut HashMap<String, Skill>) {
        let Ok(entries) = std::fs::read_dir(root) else {
            tracing::warn!("无法读取 Skill 目录: {}", root.display());
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();

            // 跳过非目录
            if !path.is_dir() {
                continue;
            }

            // 检查目录中是否存在 SKILL.md
            let skill_md = path.join("SKILL.md");
            if !skill_md.exists() {
                continue;
            }

            // 尝试加载 Skill
            match Self::load_skill(&path, &skill_md) {
                Ok(skill) => {
                    if registry.contains_key(&skill.name) {
                        tracing::debug!("Skill 名称重复，保留首个: {}", skill.name);
                    } else {
                        tracing::debug!("已加载 Skill: {}", skill.name);
                        let name = skill.name.clone();
                        registry.insert(name, skill);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "加载 Skill 失败 {}: {}",
                        skill_md.display(),
                        e
                    );
                }
            }
        }
    }

    /// 从 SKILL.md 加载单个 Skill
    /// 解析 YAML frontmatter 获取元数据，打开 cap-std 沙箱目录
    fn load_skill(skill_dir: &Path, skill_md: &Path) -> anyhow::Result<Skill> {
        // 读取 SKILL.md 内容
        let content = std::fs::read_to_string(skill_md)
            .with_context(|| format!("读取 SKILL.md 失败: {}", skill_md.display()))?;

        // 解析 frontmatter
        let fm = parse_frontmatter(&content)
            .with_context(|| format!("解析 SKILL.md frontmatter 失败: {}", skill_md.display()))?;

        // 打开 cap-std 沙箱目录句柄
        // 后续所有文件读取都通过此句柄进行，自动限制在目录内
        let dir = Dir::open_ambient_dir(skill_dir, ambient_authority())
            .with_context(|| format!("打开 Skill 目录失败: {}", skill_dir.display()))?;

        Ok(Skill {
            name: fm.name,
            description: fm.description,
            dir,
        })
    }

    /// 返回按名称排序的 Skill 列表（用于 prompt 注入）
    /// 排序确保不同会话间 prompt 一致
    pub fn list(&self) -> Vec<SkillListItem> {
        let mut items: Vec<_> = self
            .by_name
            .values()
            .map(|s| SkillListItem {
                name: s.name.clone(),
                description: s.description.clone(),
            })
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        items
    }

    /// 从指定 Skill 中读取一个文件
    /// 默认加载 SKILL.md，可通过 path 参数指定其他文件
    ///
    /// # 安全保障
    /// - 通过 cap-std Dir 限制，无法读取 skill 目录之外的文件
    /// - 路径遍历攻击（如 `../../etc/passwd`）会被 cap-std 自动拒绝
    /// - 文件大小限制在 MAX_SKILL_FILE_BYTES 以内
    pub fn load_skill_file(
        &self,
        name: &str,
        path: Option<&str>,
    ) -> anyhow::Result<String> {
        // 查找 Skill
        let skill = self
            .by_name
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Skill 不存在: {}", name))?;

        // 规范化相对路径
        let relative = normalize_skill_path(path.unwrap_or("SKILL.md"))?;

        // 检查文件元数据
        let meta = skill.dir.metadata(&relative).with_context(|| {
            format!("文件不存在或无法访问: {} (skill: {})", relative, name)
        })?;

        if !meta.is_file() {
            bail!("路径必须指向文件: {} (skill: {})", relative, name);
        }

        if meta.len() > MAX_SKILL_FILE_BYTES {
            bail!(
                "文件过大（{} 字节，上限 {} 字节）: {} (skill: {})",
                meta.len(),
                MAX_SKILL_FILE_BYTES,
                relative,
                name
            );
        }

        // 读取文件内容
        let content = skill.dir.read_to_string(&relative).with_context(|| {
            format!("读取文件失败: {} (skill: {})", relative, name)
        })?;

        Ok(content)
    }

    /// 按名称获取 Skill 引用
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.by_name.get(name)
    }

    /// 已注册的 Skill 数量
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// 是否没有任何已注册的 Skill
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

// ============================================================================
// YAML Frontmatter 解析
// ============================================================================

/// 解析 SKILL.md 的 YAML frontmatter
/// frontmatter 由 `---` 行包围，内部为 `key: value` 格式
/// 必须包含 `name` 和 `description` 字段
fn parse_frontmatter(content: &str) -> anyhow::Result<SkillFrontmatter> {
    // 处理 BOM（字节序标记）
    let text = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut lines = text.lines();

    // 第一行必须是 "---"
    let Some(first) = lines.next() else {
        bail!("SKILL.md 不能为空");
    };
    if first.trim() != "---" {
        bail!("SKILL.md 必须以 YAML frontmatter 开头（---）");
    }

    // 收集 frontmatter 行直到遇到结束 "---"
    let mut name = None;
    let mut description = None;
    let mut found_end = false;

    for line in lines {
        if line.trim() == "---" {
            found_end = true;
            break;
        }

        // 解析 "key: value" 格式
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = strip_quotes(value.trim());
            if value.is_empty() {
                continue;
            }
            match key {
                "name" => name = Some(value.to_string()),
                "description" => description = Some(value.to_string()),
                _ => {} // 忽略未知字段
            }
        }
    }

    if !found_end {
        bail!("SKILL.md frontmatter 未闭合（缺少结束 ---）");
    }

    let name = name.ok_or_else(|| anyhow::anyhow!("frontmatter 缺少 name 字段"))?;
    let description =
        description.ok_or_else(|| anyhow::anyhow!("frontmatter 缺少 description 字段"))?;

    Ok(SkillFrontmatter { name, description })
}

/// 去除字符串两端的匹配引号
/// 处理 frontmatter 中值可能带引号的情况
fn strip_quotes(s: &str) -> &str {
    let trimmed = s.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

// ============================================================================
// 路径工具
// ============================================================================

/// 规范化 skill 内的相对路径
/// - 去除前导 `./`
/// - 将反斜杠替换为正斜杠
/// - 拒绝绝对路径和 `..` 遍历（cap-std 也会拒绝，但提前给出友好错误）
fn normalize_skill_path(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw
        .trim()
        .trim_start_matches("./")
        .trim_start_matches(".\\")
        .replace('\\', "/");

    if trimmed.is_empty() {
        bail!("路径不能为空");
    }

    // 拒绝绝对路径
    if trimmed.starts_with('/') || Path::new(&trimmed).is_absolute() {
        bail!("路径必须是相对路径: {}", raw);
    }

    // 拒绝路径遍历
    if trimmed.split('/').any(|part| part == "..") {
        bail!("路径不能包含 `..`: {}", raw);
    }

    Ok(trimmed)
}

/// 展开路径中的 `~` 为用户主目录
/// 支持 `~/path` 和 `~\path` 格式
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path
        .strip_prefix("~/")
        .or_else(|| path.strip_prefix("~\\"))
    {
        // 优先使用 HOME（Unix），回退到 USERPROFILE（Windows）
        if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试：frontmatter 解析
    #[test]
    fn parse_frontmatter_basic() {
        let content = "---\nname: my-skill\ndescription: A test skill\n---\n# Body";
        let fm = parse_frontmatter(content).unwrap();
        assert_eq!(fm.name, "my-skill");
        assert_eq!(fm.description, "A test skill");
    }

    /// 测试：frontmatter 带引号
    #[test]
    fn parse_frontmatter_quoted() {
        let content = "---\nname: \"quoted-name\"\ndescription: 'single quoted'\n---\nBody";
        let fm = parse_frontmatter(content).unwrap();
        assert_eq!(fm.name, "quoted-name");
        assert_eq!(fm.description, "single quoted");
    }

    /// 测试：缺少 name 应报错
    #[test]
    fn parse_frontmatter_missing_name() {
        let content = "---\ndescription: No name\n---\nBody";
        assert!(parse_frontmatter(content).is_err());
    }

    /// 测试：缺少结束标记应报错
    #[test]
    fn parse_frontmatter_unclosed() {
        let content = "---\nname: test\ndescription: test";
        assert!(parse_frontmatter(content).is_err());
    }

    /// 测试：没有 frontmatter 应报错
    #[test]
    fn parse_frontmatter_no_fence() {
        let content = "Just plain text";
        assert!(parse_frontmatter(content).is_err());
    }

    /// 测试：路径规范化
    #[test]
    fn normalize_paths() {
        assert_eq!(normalize_skill_path("SKILL.md").unwrap(), "SKILL.md");
        assert_eq!(normalize_skill_path("./SKILL.md").unwrap(), "SKILL.md");
        assert_eq!(
            normalize_skill_path("examples/test.md").unwrap(),
            "examples/test.md"
        );
        assert_eq!(
            normalize_skill_path("a\\b\\c.md").unwrap(),
            "a/b/c.md"
        );
    }

    /// 测试：拒绝路径遍历
    #[test]
    fn reject_path_traversal() {
        assert!(normalize_skill_path("../../../etc/passwd").is_err());
        assert!(normalize_skill_path("a/../b").is_err());
        assert!(normalize_skill_path("").is_err());
    }

    /// 测试：拒绝绝对路径
    #[test]
    fn reject_absolute_path() {
        assert!(normalize_skill_path("/etc/passwd").is_err());
    }

    /// 测试：cap-std 沙箱 Skill 加载
    #[test]
    fn sandbox_skill_loading() {
        let temp = std::env::temp_dir().join("scapegoat_skill_test");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        // 创建一个测试 Skill 目录
        let skill_dir = temp.join("test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: A test skill\n---\n# Test Skill\n\nHello!",
        )
        .unwrap();
        std::fs::write(skill_dir.join("extra.md"), "Extra content").unwrap();

        // 扫描并加载
        let registry = SkillRegistry::scan(&[temp.clone()]).unwrap();
        assert_eq!(registry.len(), 1);

        // 读取 SKILL.md（默认）
        let content = registry.load_skill_file("test-skill", None).unwrap();
        assert!(content.contains("# Test Skill"));

        // 读取额外文件
        let extra = registry
            .load_skill_file("test-skill", Some("extra.md"))
            .unwrap();
        assert_eq!(extra, "Extra content");

        // 路径遍历应被拒绝
        let result = registry.load_skill_file("test-skill", Some("../../etc/passwd"));
        assert!(result.is_err());

        // 不存在的 Skill 应报错
        let result = registry.load_skill_file("nonexistent", None);
        assert!(result.is_err());

        // 列表应排序
        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "test-skill");

        let _ = std::fs::remove_dir_all(temp);
    }

    /// 测试：空目录不报错
    #[test]
    fn scan_empty_directory() {
        let temp = std::env::temp_dir().join("scapegoat_skill_empty");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let registry = SkillRegistry::scan(&[temp.clone()]).unwrap();
        assert!(registry.is_empty());

        let _ = std::fs::remove_dir_all(temp);
    }

    /// 测试：不存在的目录不报错
    #[test]
    fn scan_nonexistent_directory() {
        let registry =
            SkillRegistry::scan(&[PathBuf::from("/nonexistent/path")]).unwrap();
        assert!(registry.is_empty());
    }

    /// 测试：去引号
    #[test]
    fn strip_quotes_works() {
        assert_eq!(strip_quotes("\"hello\""), "hello");
        assert_eq!(strip_quotes("'hello'"), "hello");
        assert_eq!(strip_quotes("hello"), "hello");
        assert_eq!(strip_quotes("\"mixed'"), "\"mixed'");
    }

    /// 测试：展开 tilde
    #[test]
    fn expand_tilde_works() {
        // 非 tilde 路径原样返回
        assert_eq!(expand_tilde("./skills"), PathBuf::from("./skills"));
        assert_eq!(expand_tilde("/absolute"), PathBuf::from("/absolute"));
    }
}
