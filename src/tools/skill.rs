// src/tools/skill.rs
// Skill 内置工具执行模块
// LLM 通过此工具按名称加载技能文件
// 不暴露宿主机上的真实路径，只使用技能名和技能内相对路径

use super::{ToolContext, ToolResult};

/// 执行 Skill 工具：加载指定技能的文件内容
///
/// # 参数
/// - `ctx`: 工具执行上下文（包含 SkillRegistry）
/// - `name`: 技能名称（必须在已注册列表中）
/// - `path`: 技能内相对路径（可选，默认为 "SKILL.md"）
///
/// # 安全保障
/// - 技能名必须在注册表中存在
/// - 文件路径通过 cap-std 沙箱限制在技能目录内
/// - 路径遍历攻击会被 cap-std 和前置检查双重拒绝
pub fn execute_skill(
    ctx: &ToolContext,
    name: &str,
    path: Option<&str>,
) -> ToolResult {
    // 显示路径（返回给 LLM 的路径，不含真实目录）
    let display_path = path.unwrap_or("SKILL.md");

    match ctx.skills.load_skill_file(name, path) {
        Ok(content) => ToolResult::ok(format!(
            "技能: {}\n路径: {}\n\n{}",
            name, display_path, content
        )),
        Err(e) => ToolResult::err(format!(
            "加载技能失败 (name={}, path={}): {}",
            name, display_path, e
        )),
    }
}
