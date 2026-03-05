// src/skills/mod.rs
// Skill 系统模块
// 参考 zeroclaw/src/skills/mod.rs，实现 Skill 的定义、加载、解析和 Prompt 注入
// Skill 是用户自定义的能力扩展，可以包含指令文本和工具定义
// 支持 SKILL.toml（结构化清单）和 SKILL.md（Markdown 简化格式）两种格式

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

// ============================================================================
// Skill 数据结构
// ============================================================================

/// Skill（技能）是用户自定义或社区构建的能力扩展
/// 每个 Skill 可以包含系统指令和工具定义，在运行时注入到 LLM 的系统提示中
/// Skill 文件位于 `{workspace}/skills/{name}/SKILL.md` 或 `SKILL.toml`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Skill 名称，用作唯一标识符
    pub name: String,
    /// Skill 的简要描述，在 Compact 模式下作为摘要显示
    pub description: String,
    /// Skill 版本号，遵循 SemVer 格式
    pub version: String,
    /// Skill 作者（可选）
    #[serde(default)]
    pub author: Option<String>,
    /// 标签列表，用于分类和搜索
    #[serde(default)]
    pub tags: Vec<String>,
    /// Skill 定义的工具列表
    #[serde(default)]
    pub tools: Vec<SkillTool>,
    /// Skill 的系统指令文本列表
    /// 这些文本会被注入到 LLM 的系统提示中，指导 LLM 的行为
    #[serde(default)]
    pub prompts: Vec<String>,
    /// SKILL.md 或 SKILL.toml 文件的绝对路径
    #[serde(skip)]
    pub location: Option<PathBuf>,
    /// 是否在 Compact 模式下也完整注入指令
    /// 设为 true 时，即使在 Compact 模式下也会完整注入该 Skill 的所有指令
    #[serde(default)]
    pub always: bool,
}

/// Skill 定义的工具
/// 工具可以是 shell 命令、HTTP 调用或脚本
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillTool {
    /// 工具名称，供 LLM 在工具调用中引用
    pub name: String,
    /// 工具的自然语言描述
    pub description: String,
    /// 工具类型："shell"、"http" 或 "script"
    pub kind: String,
    /// 要执行的命令、URL 或脚本内容
    pub command: String,
    /// 工具的额外参数键值对
    #[serde(default)]
    pub args: HashMap<String, String>,
}

// ============================================================================
// SKILL.toml 清单格式
// ============================================================================

/// SKILL.toml 的完整清单结构
/// 示例:
/// ```toml
/// [skill]
/// name = "my-skill"
/// description = "这个 Skill 做什么"
/// version = "0.1.0"
///
/// [[tools]]
/// name = "my_tool"
/// description = "工具描述"
/// kind = "shell"
/// command = "echo hello"
///
/// prompts = ["你应该按照以下规则行事..."]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillManifest {
    /// Skill 元数据段
    skill: SkillMeta,
    /// 工具定义列表
    #[serde(default)]
    tools: Vec<SkillTool>,
    /// 系统指令列表
    #[serde(default)]
    prompts: Vec<String>,
}

/// SKILL.toml 中 [skill] 段的元数据
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillMeta {
    /// Skill 名称
    name: String,
    /// Skill 描述
    description: String,
    /// 版本号，默认 "0.1.0"
    #[serde(default = "default_version")]
    version: String,
    /// 作者
    #[serde(default)]
    author: Option<String>,
    /// 标签
    #[serde(default)]
    tags: Vec<String>,
}

/// 默认版本号
fn default_version() -> String {
    "0.1.0".to_string()
}

// ============================================================================
// Skill Prompt 注入模式
// ============================================================================

/// 控制 Skill 如何注入到系统提示中的模式
/// 不同模式在上下文大小和功能完整性之间做出权衡
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SkillsPromptInjectionMode {
    /// 完整模式：在系统提示中内联所有 Skill 的指令和工具元数据
    /// 优点：LLM 可以直接访问所有 Skill 信息，无需额外读取
    /// 缺点：占用更多的上下文窗口空间
    #[default]
    Full,
    /// 紧凑模式：只内联 Skill 的名称、描述和位置
    /// LLM 需要按需读取 Skill 文件获取完整指令
    /// 标记为 `always=true` 的 Skill 即使在此模式下也会完整注入
    Compact,
}

// ============================================================================
// Skill 加载函数
// ============================================================================

/// 从工作区的 skills/ 目录加载所有 Skill
/// 扫描 `{workspace_dir}/skills/` 下的每个子目录
/// 每个子目录应包含 SKILL.toml 或 SKILL.md 文件
///
/// # 参数
/// - `workspace_dir`: 工作区根目录路径
///
/// # 返回
/// 成功加载的 Skill 列表（加载失败的会被跳过并记录警告日志）
pub fn load_skills(workspace_dir: &Path) -> Vec<Skill> {
    let skills_dir = workspace_dir.join("skills");
    load_skills_from_directory(&skills_dir)
}

/// 从指定目录加载所有 Skill
/// 遍历目录中的每个子目录，尝试从中加载 Skill
/// 优先尝试 SKILL.toml，其次尝试 SKILL.md
fn load_skills_from_directory(skills_dir: &Path) -> Vec<Skill> {
    // 如果 skills 目录不存在，返回空列表（这是正常情况）
    if !skills_dir.exists() {
        tracing::debug!("Skill 目录不存在: {}", skills_dir.display());
        return Vec::new();
    }

    let mut skills = Vec::new();

    // 读取目录内容
    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        tracing::warn!("无法读取 Skill 目录: {}", skills_dir.display());
        return skills;
    };

    // 遍历目录中的每个条目
    for entry in entries.flatten() {
        let path = entry.path();

        // 跳过非目录条目
        if !path.is_dir() {
            continue;
        }

        // 优先尝试 SKILL.toml 格式
        let toml_path = path.join("SKILL.toml");
        let md_path = path.join("SKILL.md");

        if toml_path.exists() {
            // 从 SKILL.toml 加载
            match load_skill_toml(&toml_path) {
                Ok(skill) => {
                    tracing::debug!("已加载 Skill (TOML): {}", skill.name);
                    skills.push(skill);
                }
                Err(e) => {
                    tracing::warn!(
                        "加载 SKILL.toml 失败 {}: {}",
                        toml_path.display(),
                        e
                    );
                }
            }
        } else if md_path.exists() {
            // 从 SKILL.md 加载
            match load_skill_md(&md_path, &path) {
                Ok(skill) => {
                    tracing::debug!("已加载 Skill (MD): {}", skill.name);
                    skills.push(skill);
                }
                Err(e) => {
                    tracing::warn!(
                        "加载 SKILL.md 失败 {}: {}",
                        md_path.display(),
                        e
                    );
                }
            }
        }
    }

    skills
}

// ============================================================================
// SKILL.toml 解析
// ============================================================================

/// 从 SKILL.toml 文件加载 Skill
/// 使用标准的 TOML 反序列化，将清单转换为 Skill 结构体
fn load_skill_toml(path: &Path) -> Result<Skill> {
    // 读取文件内容
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取 SKILL.toml 失败: {}", path.display()))?;

    // 解析 TOML 清单
    let manifest: SkillManifest = toml::from_str(&content)
        .with_context(|| format!("解析 SKILL.toml 失败: {}", path.display()))?;

    // 转换为 Skill 结构体
    Ok(Skill {
        name: manifest.skill.name,
        description: manifest.skill.description,
        version: manifest.skill.version,
        author: manifest.skill.author,
        tags: manifest.skill.tags,
        tools: manifest.tools,
        prompts: manifest.prompts,
        location: Some(path.to_path_buf()),
        always: false,
    })
}

// ============================================================================
// SKILL.md 解析
// ============================================================================

/// 从 SKILL.md 文件加载 Skill
/// SKILL.md 使用简化的 Markdown 格式，可选地包含 YAML Front Matter 头
///
/// # SKILL.md 格式示例
/// ```markdown
/// ---
/// name: my-skill
/// version: 1.0.0
/// author: someone
/// always: false
/// ---
/// # Skill 标题
///
/// 这里是 Skill 的指令文本，会被注入到系统提示中。
/// LLM 将按照这些指令行事。
/// ```
fn load_skill_md(path: &Path, dir: &Path) -> Result<Skill> {
    // 读取文件内容
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取 SKILL.md 失败: {}", path.display()))?;

    // 解析可选的 YAML Front Matter
    let (front_matter, body) = parse_front_matter(&content);

    // 从目录名推导默认 Skill 名称
    let mut name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let mut version = "0.1.0".to_string();
    let mut author: Option<String> = None;

    // Front Matter 中的字段优先于默认值
    if let Some(fm_name) = front_matter.get("name") {
        if !fm_name.is_empty() {
            name = fm_name.clone();
        }
    }
    if let Some(fm_version) = front_matter.get("version") {
        if !fm_version.is_empty() {
            version = fm_version.clone();
        }
    }
    if let Some(fm_author) = front_matter.get("author") {
        if !fm_author.is_empty() {
            author = Some(fm_author.clone());
        }
    }

    // 解析 always 标志
    let always = fm_bool(&front_matter, "always");

    // 确定要注入的指令文本
    // 如果有 Front Matter 且 body 非空，使用 body（不含 Front Matter）
    // 否则使用完整内容
    let prompt_body = if body.trim().is_empty() {
        content.clone()
    } else {
        body.to_string()
    };

    Ok(Skill {
        name,
        description: extract_description(&content),
        version,
        author,
        tags: Vec::new(),
        tools: Vec::new(),
        prompts: vec![prompt_body],
        location: Some(path.to_path_buf()),
        always,
    })
}

// ============================================================================
// Front Matter 解析
// ============================================================================

/// 解析 Markdown 文件中可选的 YAML-like Front Matter
/// Front Matter 由 `---` 行包围，内部为 `key: value` 格式
///
/// # 返回
/// - `(front_matter_map, body)`: Front Matter 键值对和去除 Front Matter 后的正文内容
///
/// # 示例
/// ```text
/// ---
/// name: my-skill
/// version: 1.0.0
/// ---
/// 正文内容在这里
/// ```
fn parse_front_matter(content: &str) -> (HashMap<String, String>, &str) {
    // 处理 BOM（字节序标记）
    let text = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut lines = text.lines();

    // 检查第一行是否为 Front Matter 起始标记 "---"
    let Some(first) = lines.next() else {
        return (HashMap::new(), content);
    };
    if first.trim() != "---" {
        // 没有 Front Matter，返回空 map 和完整内容
        return (HashMap::new(), content);
    }

    let mut map = HashMap::new();
    // 起始标记之后的字节偏移
    let start = first.len() + 1;
    let mut end = start;

    // 逐行解析 Front Matter 中的键值对
    for line in lines {
        // 遇到结束标记 "---"
        if line.trim() == "---" {
            // 计算 body 的起始位置（结束标记之后）
            let body_start = end + line.len() + 1;
            let body = if body_start <= text.len() {
                // 跳过 body 开头的空行
                text[body_start..].trim_start_matches(['\n', '\r'])
            } else {
                ""
            };
            return (map, body);
        }

        // 解析 "key: value" 格式
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_lowercase();
            let value = strip_quotes(value).to_string();
            if !key.is_empty() && !value.is_empty() {
                map.insert(key, value);
            }
        }
        // 累加字节偏移（+1 for newline）
        end += line.len() + 1;
    }

    // Front Matter 未闭合：为安全起见，忽略它并返回完整内容
    (HashMap::new(), content)
}

/// 去除字符串两端的匹配单引号或双引号
/// 处理 Front Matter 中值可能带引号的情况
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

/// 从 Front Matter 中解析宽松的布尔值
/// 支持 "true"、"yes"、"1" 作为真值
fn fm_bool(map: &HashMap<String, String>, key: &str) -> bool {
    map.get(key)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1"))
        .unwrap_or(false)
}

/// 从 Skill 内容中提取描述文本
/// 优先使用 Front Matter 中的 description 字段
/// 如果没有，则取正文中第一行非标题、非空的文本行
fn extract_description(content: &str) -> String {
    let (fm, body) = parse_front_matter(content);

    // 优先使用 Front Matter 中的 description
    if let Some(desc) = fm.get("description") {
        if !desc.trim().is_empty() {
            return desc.trim().to_string();
        }
    }

    // 回退：取正文中第一行有意义的文本
    body.lines()
        .find(|line| !line.starts_with('#') && !line.trim().is_empty())
        .unwrap_or("No description")
        .trim()
        .to_string()
}

// ============================================================================
// XML 转义与输出
// ============================================================================

/// 将文本内容追加到输出字符串，同时进行 XML 转义
/// 防止 Skill 内容中的特殊字符破坏 XML 结构或引发注入攻击
/// 转义规则：& → &amp;  < → &lt;  > → &gt;  " → &quot;  ' → &apos;
fn append_xml_escaped(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
}

/// 写入一个带缩进的 XML 文本元素
/// 格式: `{indent}<{tag}>{escaped_value}</{tag}>\n`
fn write_xml_text_element(out: &mut String, indent: usize, tag: &str, value: &str) {
    // 写入缩进空格
    for _ in 0..indent {
        out.push(' ');
    }
    // 写入开标签
    out.push('<');
    out.push_str(tag);
    out.push('>');
    // 写入 XML 转义后的值
    append_xml_escaped(out, value);
    // 写入闭标签
    out.push_str("</");
    out.push_str(tag);
    out.push_str(">\n");
}

// ============================================================================
// Skill 位置解析
// ============================================================================

/// 解析 Skill 文件的绝对路径
/// 如果 Skill 记录了 location，直接使用；否则推导默认路径
fn resolve_skill_location(skill: &Skill, workspace_dir: &Path) -> PathBuf {
    skill.location.clone().unwrap_or_else(|| {
        workspace_dir
            .join("skills")
            .join(&skill.name)
            .join("SKILL.md")
    })
}

/// 渲染 Skill 文件的路径字符串，用于 Prompt 中的 <location> 元素
/// 在 Compact 模式下使用相对路径（更紧凑），Full 模式下使用绝对路径
/// 路径分隔符统一为正斜杠（跨平台兼容）
fn render_skill_location(skill: &Skill, workspace_dir: &Path, prefer_relative: bool) -> String {
    let location = resolve_skill_location(skill, workspace_dir);
    let path_str = if prefer_relative {
        // 尝试生成相对路径
        match location.strip_prefix(workspace_dir) {
            Ok(relative) => relative.display().to_string(),
            Err(_) => location.display().to_string(),
        }
    } else {
        location.display().to_string()
    };
    // 统一为正斜杠（Windows 兼容）
    path_str.replace('\\', "/")
}

// ============================================================================
// Skill → Prompt 转换
// ============================================================================

/// 使用默认的 Full 模式将 Skill 列表转换为系统提示文本
/// 这是简单场景下的便捷入口
pub fn skills_to_prompt(skills: &[Skill], workspace_dir: &Path) -> String {
    skills_to_prompt_with_mode(skills, workspace_dir, SkillsPromptInjectionMode::Full)
}

/// 将 Skill 列表转换为系统提示文本，支持配置注入模式
///
/// # Full 模式
/// 在系统提示中完整内联所有 Skill 的指令和工具元数据
/// LLM 可以直接使用，无需在运行时读取文件
///
/// # Compact 模式
/// 只内联 Skill 的名称、描述和位置信息
/// LLM 需要按需读取对应文件获取详细指令
/// 标记为 `always=true` 的 Skill 即使在此模式下也会完整注入
///
/// # 输出格式
/// 使用 XML 结构化格式，所有文本内容经过 XML 转义以防注入：
/// ```xml
/// ## Available Skills
///
/// <available_skills>
///   <skill>
///     <name>skill-name</name>
///     <description>skill description</description>
///     <location>skills/skill-name/SKILL.md</location>
///     <instructions>
///       <instruction>instruction text</instruction>
///     </instructions>
///     <tools>
///       <tool>
///         <name>tool-name</name>
///         <description>tool description</description>
///         <kind>shell</kind>
///       </tool>
///     </tools>
///   </skill>
/// </available_skills>
/// ```
pub fn skills_to_prompt_with_mode(
    skills: &[Skill],
    workspace_dir: &Path,
    mode: SkillsPromptInjectionMode,
) -> String {
    // 无 Skill 时返回空字符串
    if skills.is_empty() {
        return String::new();
    }

    // 根据模式生成不同的头部说明
    let mut prompt = match mode {
        SkillsPromptInjectionMode::Full => String::from(
            "## Available Skills\n\n\
             Skill instructions and tool metadata are preloaded below.\n\
             Follow these instructions directly; do not read skill files at runtime unless the user asks.\n\n\
             <available_skills>\n",
        ),
        SkillsPromptInjectionMode::Compact => String::from(
            "## Available Skills\n\n\
             Skill summaries are preloaded below to keep context compact.\n\
             Skill instructions are loaded on demand: read the skill file in `location` when needed. \
             Skills marked `always` include full instructions below even in compact mode.\n\n\
             <available_skills>\n",
        ),
    };

    // 对每个 Skill 生成 XML 块
    for skill in skills {
        let _ = writeln!(prompt, "  <skill>");

        // 写入基础元数据（名称、描述、位置）
        write_xml_text_element(&mut prompt, 4, "name", &skill.name);
        write_xml_text_element(&mut prompt, 4, "description", &skill.description);

        // Compact 模式下使用相对路径，Full 模式下使用绝对路径
        let location = render_skill_location(
            skill,
            workspace_dir,
            matches!(mode, SkillsPromptInjectionMode::Compact),
        );
        write_xml_text_element(&mut prompt, 4, "location", &location);

        // 判断是否需要注入完整指令
        // Full 模式下始终注入；Compact 模式下仅 always=true 的 Skill 注入
        let inject_full =
            matches!(mode, SkillsPromptInjectionMode::Full) || skill.always;

        if inject_full {
            // 注入指令文本
            if !skill.prompts.is_empty() {
                let _ = writeln!(prompt, "    <instructions>");
                for instruction in &skill.prompts {
                    write_xml_text_element(&mut prompt, 6, "instruction", instruction);
                }
                let _ = writeln!(prompt, "    </instructions>");
            }

            // 注入工具定义
            if !skill.tools.is_empty() {
                let _ = writeln!(prompt, "    <tools>");
                for tool in &skill.tools {
                    let _ = writeln!(prompt, "      <tool>");
                    write_xml_text_element(&mut prompt, 8, "name", &tool.name);
                    write_xml_text_element(&mut prompt, 8, "description", &tool.description);
                    write_xml_text_element(&mut prompt, 8, "kind", &tool.kind);
                    let _ = writeln!(prompt, "      </tool>");
                }
                let _ = writeln!(prompt, "    </tools>");
            }
        }

        let _ = writeln!(prompt, "  </skill>");
    }

    // 关闭 XML 根标签
    prompt.push_str("</available_skills>");
    prompt
}

/// 获取 skills 目录路径
pub fn skills_dir(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("skills")
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试：空 Skill 列表应返回空字符串
    #[test]
    fn empty_skills_produce_empty_prompt() {
        let prompt = skills_to_prompt(&[], Path::new("/workspace"));
        assert!(prompt.is_empty());
    }

    /// 测试：Full 模式下应注入完整指令和工具定义
    #[test]
    fn full_mode_injects_instructions_and_tools() {
        let skills = vec![Skill {
            name: "test-skill".to_string(),
            description: "A test skill".to_string(),
            version: "0.1.0".to_string(),
            author: None,
            tags: Vec::new(),
            tools: vec![SkillTool {
                name: "echo_tool".to_string(),
                description: "Echoes input".to_string(),
                kind: "shell".to_string(),
                command: "echo".to_string(),
                args: HashMap::new(),
            }],
            prompts: vec!["Always be helpful.".to_string()],
            location: None,
            always: false,
        }];

        let prompt = skills_to_prompt_with_mode(
            &skills,
            Path::new("/workspace"),
            SkillsPromptInjectionMode::Full,
        );

        // 验证包含 Skill 名称
        assert!(prompt.contains("<name>test-skill</name>"));
        // 验证包含指令
        assert!(prompt.contains("<instruction>Always be helpful.</instruction>"));
        // 验证包含工具定义
        assert!(prompt.contains("<name>echo_tool</name>"));
        assert!(prompt.contains("<kind>shell</kind>"));
    }

    /// 测试：Compact 模式下普通 Skill 不注入指令
    #[test]
    fn compact_mode_skips_instructions() {
        let skills = vec![Skill {
            name: "test-skill".to_string(),
            description: "A test skill".to_string(),
            version: "0.1.0".to_string(),
            author: None,
            tags: Vec::new(),
            tools: Vec::new(),
            prompts: vec!["Secret instruction".to_string()],
            location: None,
            always: false,
        }];

        let prompt = skills_to_prompt_with_mode(
            &skills,
            Path::new("/workspace"),
            SkillsPromptInjectionMode::Compact,
        );

        // 验证包含名称
        assert!(prompt.contains("<name>test-skill</name>"));
        // 验证不包含指令（Compact 模式下不注入）
        assert!(!prompt.contains("<instructions>"));
    }

    /// 测试：Compact 模式下 always=true 的 Skill 仍然注入完整指令
    #[test]
    fn compact_mode_injects_always_skills() {
        let skills = vec![Skill {
            name: "critical-skill".to_string(),
            description: "Must always be present".to_string(),
            version: "0.1.0".to_string(),
            author: None,
            tags: Vec::new(),
            tools: Vec::new(),
            prompts: vec!["Critical instruction".to_string()],
            location: None,
            always: true, // 即使 Compact 模式也完整注入
        }];

        let prompt = skills_to_prompt_with_mode(
            &skills,
            Path::new("/workspace"),
            SkillsPromptInjectionMode::Compact,
        );

        // 验证包含指令（always=true）
        assert!(prompt.contains("<instruction>Critical instruction</instruction>"));
    }

    /// 测试：XML 特殊字符应被正确转义
    #[test]
    fn xml_escaping_works() {
        let skills = vec![Skill {
            name: "code<review>&".to_string(),
            description: "Review \"unsafe\" bits".to_string(),
            version: "0.1.0".to_string(),
            author: None,
            tags: Vec::new(),
            tools: Vec::new(),
            prompts: Vec::new(),
            location: None,
            always: false,
        }];

        let prompt = skills_to_prompt(&skills, Path::new("/workspace"));

        // 验证特殊字符被正确转义
        assert!(prompt.contains("<name>code&lt;review&gt;&amp;</name>"));
        assert!(prompt.contains("Review &quot;unsafe&quot; bits"));
    }

    /// 测试：Front Matter 解析
    #[test]
    fn parse_front_matter_basic() {
        let content = "---\nname: my-skill\nversion: 1.0.0\n---\nBody text here";
        let (fm, body) = parse_front_matter(content);

        assert_eq!(fm.get("name").unwrap(), "my-skill");
        assert_eq!(fm.get("version").unwrap(), "1.0.0");
        assert_eq!(body, "Body text here");
    }

    /// 测试：没有 Front Matter 的内容应返回空 map 和完整内容
    #[test]
    fn parse_front_matter_none() {
        let content = "Just plain markdown\nSecond line";
        let (fm, body) = parse_front_matter(content);

        assert!(fm.is_empty());
        assert_eq!(body, content);
    }

    /// 测试：Front Matter 中带引号的值应被正确去除引号
    #[test]
    fn parse_front_matter_with_quotes() {
        let content = "---\nname: \"quoted-name\"\nauthor: 'single-quoted'\n---\nBody";
        let (fm, body) = parse_front_matter(content);

        assert_eq!(fm.get("name").unwrap(), "quoted-name");
        assert_eq!(fm.get("author").unwrap(), "single-quoted");
        assert_eq!(body, "Body");
    }

    /// 测试：描述提取
    #[test]
    fn extract_description_from_front_matter() {
        let content = "---\ndescription: My cool skill\n---\n# Title\n\nBody";
        let desc = extract_description(content);
        assert_eq!(desc, "My cool skill");
    }

    /// 测试：从正文中提取描述（没有 Front Matter）
    #[test]
    fn extract_description_from_body() {
        let content = "# My Skill\n\nThis is the first paragraph.";
        let desc = extract_description(content);
        assert_eq!(desc, "This is the first paragraph.");
    }
}
