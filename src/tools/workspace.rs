// src/tools/workspace.rs
// 工作区文件工具模块
// 通过 cap-std 库将所有文件操作限制在 workspace 目录内
// 提供 Read / Write / Edit 三个内置工具，确保 QQ Bot 处于受限的沙箱环境

use anyhow::{Context as _, bail};
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 工作区文件读取的最大字节数
/// 超过此大小的文件不允许读取，防止撑爆上下文窗口
const MAX_READ_FILE_BYTES: u64 = 200_000;

// ============================================================================
// WorkspaceToolSession — 每次 agent 会话的文件操作状态
// ============================================================================

/// 每次 agent 会话的工具状态
/// 核心规则：Edit 前必须先 Read，Edit 后必须重新 Read 才能再次 Edit
#[derive(Debug, Default)]
pub struct WorkspaceToolSession {
    /// 当前会话中已 Read 过的文件路径集合（规范化后的相对路径）
    /// 只有在此集合中的文件才允许 Edit
    readable_for_edit: HashSet<String>,
}

impl WorkspaceToolSession {
    /// 标记一个文件已被 Read
    fn note_read(&mut self, relative_path: &str) {
        self.readable_for_edit.insert(relative_path.to_string());
    }

    /// 检查文件是否已被 Read（Edit 前置条件）
    fn require_fresh_read(&self, relative_path: &str) -> anyhow::Result<()> {
        if self.readable_for_edit.contains(relative_path) {
            return Ok(());
        }
        bail!(
            "Edit is not allowed until the file has been Read in this session: {}",
            relative_path
        );
    }

    /// Edit 后清除 Read 标记，要求重新 Read 才能再次 Edit
    fn invalidate_after_edit(&mut self, relative_path: &str) {
        self.readable_for_edit.remove(relative_path);
    }
}

// ============================================================================
// WorkspaceTools — cap-std 沙箱文件工具
// ============================================================================

/// 工作区文件工具
/// 通过 cap-std 的 Dir 将所有文件操作限制在 workspace 目录内
/// LLM 无法通过路径遍历（如 `../`）逃逸到工作区外
pub struct WorkspaceTools {
    /// cap-std 沙箱目录句柄（限制在 workspace_dir 内）
    dir: Dir,
    /// workspace 根目录的绝对路径（用于日志和错误消息）
    root_display: PathBuf,
}

impl WorkspaceTools {
    /// 创建工作区文件工具
    /// 打开 workspace_dir 作为 cap-std 沙箱根，所有后续文件操作都被限制在此目录内
    pub fn new(workspace_dir: &Path) -> anyhow::Result<Self> {
        // 确保目录存在
        std::fs::create_dir_all(workspace_dir).with_context(|| {
            format!("无法创建工作区目录: {}", workspace_dir.display())
        })?;

        // 获得 ambient authority 后打开沙箱目录
        // 这是 cap-std 的安全模型：authority 只在此处获取一次
        // 之后所有操作都通过 Dir 句柄进行，自动限制在此目录内
        let dir = Dir::open_ambient_dir(workspace_dir, ambient_authority()).with_context(|| {
            format!("无法打开工作区沙箱目录: {}", workspace_dir.display())
        })?;

        Ok(Self {
            dir,
            root_display: workspace_dir.to_path_buf(),
        })
    }

    /// Read 工具：读取工作区内的 UTF-8 文本文件
    ///
    /// # 安全保障
    /// - 通过 cap-std Dir 限制，无法读取 workspace 外的文件
    /// - 路径遍历攻击（如 `../../etc/passwd`）会被 cap-std 自动拒绝
    /// - 文件大小限制在 MAX_READ_FILE_BYTES 以内
    pub fn read_file(
        &self,
        session: &mut WorkspaceToolSession,
        relative_path: &str,
    ) -> anyhow::Result<String> {
        // 规范化路径（去除前导 ./ 等）
        let clean_path = normalize_relative_path(relative_path)?;

        // 检查文件大小（防止读取过大文件撑爆上下文）
        let metadata = self.dir.metadata(&clean_path).with_context(|| {
            format!("文件不存在或无法访问: {}", clean_path)
        })?;
        let file_size = metadata.len();
        if file_size > MAX_READ_FILE_BYTES {
            bail!(
                "文件过大（{} 字节，上限 {} 字节）: {}",
                file_size,
                MAX_READ_FILE_BYTES,
                clean_path
            );
        }

        // 读取文件内容
        let content = self.dir.read_to_string(&clean_path).with_context(|| {
            format!("读取文件失败: {}", clean_path)
        })?;

        // 标记此文件已被 Read（允许后续 Edit）
        session.note_read(&clean_path);

        Ok(content)
    }

    /// Write 工具：创建全新的 UTF-8 文本文件
    ///
    /// # 安全保障
    /// - 通过 cap-std Dir 限制，无法写入 workspace 外的文件
    /// - 如果目标文件已存在，则拒绝覆盖（必须使用 Edit 修改已有文件）
    /// - 自动创建必要的父目录
    pub fn write_file(
        &self,
        relative_path: &str,
        content: &str,
    ) -> anyhow::Result<String> {
        let clean_path = normalize_relative_path(relative_path)?;

        // 检查文件是否已存在（Write 只能创建新文件）
        if self.dir.exists(&clean_path) {
            bail!(
                "文件已存在，无法覆盖。如需修改请使用 Edit 工具: {}",
                clean_path
            );
        }

        // 创建父目录（如果不存在）
        if let Some(parent) = Path::new(&clean_path).parent() {
            if !parent.as_os_str().is_empty() {
                self.dir.create_dir_all(parent).with_context(|| {
                    format!("创建父目录失败: {}", parent.display())
                })?;
            }
        }

        // 写入文件
        self.dir.write(&clean_path, content).with_context(|| {
            format!("写入文件失败: {}", clean_path)
        })?;

        Ok(format!("已创建文件: {} ({} 字节)", clean_path, content.len()))
    }

    /// Edit 工具：编辑已存在的 UTF-8 文本文件
    ///
    /// # 安全保障
    /// - 通过 cap-std Dir 限制，无法编辑 workspace 外的文件
    /// - 必须在同一会话中先 Read 过该文件才能 Edit
    /// - Edit 后必须重新 Read 才能再次 Edit（确保模型看到最新内容）
    /// - 使用精确字符串匹配进行替换，避免误操作
    pub fn edit_file(
        &self,
        session: &mut WorkspaceToolSession,
        relative_path: &str,
        old_text: &str,
        new_text: &str,
        replace_all: bool,
    ) -> anyhow::Result<String> {
        let clean_path = normalize_relative_path(relative_path)?;

        // 检查 Edit 前置条件：必须先 Read
        session.require_fresh_read(&clean_path)?;

        // old_text 不能为空
        if old_text.is_empty() {
            bail!("old_text 不能为空");
        }

        // 读取当前文件内容
        let content = self.dir.read_to_string(&clean_path).with_context(|| {
            format!("读取文件失败: {}", clean_path)
        })?;

        // 执行替换
        let (new_content, match_count) = if replace_all {
            let count = content.matches(old_text).count();
            if count == 0 {
                bail!("未找到匹配的文本");
            }
            (content.replace(old_text, new_text), count)
        } else {
            // 非 replace_all 模式：要求恰好匹配一次
            let count = content.matches(old_text).count();
            if count == 0 {
                bail!("未找到匹配的文本");
            }
            if count > 1 {
                bail!(
                    "找到 {} 处匹配，但 replace_all 为 false。请提供更具体的上下文或设置 replace_all=true",
                    count
                );
            }
            (content.replacen(old_text, new_text, 1), 1)
        };

        // 写回文件
        self.dir.write(&clean_path, &new_content).with_context(|| {
            format!("写回文件失败: {}", clean_path)
        })?;

        // 清除 Read 标记，要求重新 Read 才能再次 Edit
        session.invalidate_after_edit(&clean_path);

        Ok(format!(
            "已编辑文件: {} (替换了 {} 处匹配)",
            clean_path, match_count
        ))
    }

    /// 获取工作区根目录显示路径（用于日志）
    pub fn root_display(&self) -> &Path {
        &self.root_display
    }
}

// ============================================================================
// 路径规范化
// ============================================================================

/// 规范化相对路径
/// - 去除前导 `./`
/// - 去除前导/尾随空白
/// - 拒绝空路径
/// - 不做路径遍历检查（cap-std 自身会处理）
fn normalize_relative_path(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("路径不能为空");
    }

    // 去除前导 "./"
    let cleaned = trimmed.strip_prefix("./").unwrap_or(trimmed);

    // 去除前导 "/" — cap-std 会阻止绝对路径，但我们提前给出更友好的错误
    if cleaned.starts_with('/') || cleaned.starts_with('\\') {
        bail!("不允许使用绝对路径，请使用相对于工作区根目录的路径: {}", raw);
    }

    Ok(cleaned.to_string())
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试：路径规范化
    #[test]
    fn normalize_paths() {
        assert_eq!(normalize_relative_path("SOUL.md").unwrap(), "SOUL.md");
        assert_eq!(normalize_relative_path("./SOUL.md").unwrap(), "SOUL.md");
        assert_eq!(
            normalize_relative_path("messages/group_123.md").unwrap(),
            "messages/group_123.md"
        );
        assert!(normalize_relative_path("").is_err());
        assert!(normalize_relative_path("  ").is_err());
        assert!(normalize_relative_path("/etc/passwd").is_err());
    }

    /// 测试：Read → Edit 会话规则
    #[test]
    fn session_read_before_edit() {
        let mut session = WorkspaceToolSession::default();

        // 未 Read 过的文件不能 Edit
        assert!(session.require_fresh_read("test.md").is_err());

        // Read 后可以 Edit
        session.note_read("test.md");
        assert!(session.require_fresh_read("test.md").is_ok());

        // Edit 后需要重新 Read
        session.invalidate_after_edit("test.md");
        assert!(session.require_fresh_read("test.md").is_err());
    }

    /// 测试：cap-std 沙箱 Read/Write/Edit
    #[test]
    fn sandbox_read_write_edit() {
        let temp = std::env::temp_dir().join("scapegoat_workspace_test");
        let _ = std::fs::create_dir_all(&temp);

        let tools = WorkspaceTools::new(&temp).unwrap();
        let mut session = WorkspaceToolSession::default();

        // Write 创建新文件
        let result = tools.write_file("test_file.md", "hello world");
        assert!(result.is_ok(), "Write 应该成功: {:?}", result);

        // Write 不能覆盖已存在的文件
        let result = tools.write_file("test_file.md", "overwrite");
        assert!(result.is_err(), "Write 不应该覆盖已有文件");

        // Read 读取文件
        let content = tools.read_file(&mut session, "test_file.md").unwrap();
        assert_eq!(content, "hello world");

        // Edit 修改文件（已 Read 过）
        let result = tools.edit_file(
            &mut session,
            "test_file.md",
            "hello",
            "goodbye",
            false,
        );
        assert!(result.is_ok(), "Edit 应该成功: {:?}", result);

        // Edit 后需要重新 Read
        let result = tools.edit_file(
            &mut session,
            "test_file.md",
            "goodbye",
            "hello",
            false,
        );
        assert!(result.is_err(), "Edit 后未重新 Read 应该失败");

        // 重新 Read 后可以再次 Edit
        let content = tools.read_file(&mut session, "test_file.md").unwrap();
        assert_eq!(content, "goodbye world");

        // 清理
        let _ = std::fs::remove_dir_all(temp);
    }

    /// 测试：cap-std 沙箱阻止路径遍历
    #[test]
    fn sandbox_blocks_path_traversal() {
        let temp = std::env::temp_dir().join("scapegoat_sandbox_test");
        let _ = std::fs::create_dir_all(&temp);

        let tools = WorkspaceTools::new(&temp).unwrap();
        let mut session = WorkspaceToolSession::default();

        // 路径遍历应被 cap-std 拒绝
        let result = tools.read_file(&mut session, "../../../etc/passwd");
        assert!(result.is_err(), "路径遍历应该被阻止");

        let _ = std::fs::remove_dir_all(temp);
    }

    /// 测试：Write 自动创建父目录
    #[test]
    fn write_creates_parent_dirs() {
        let temp = std::env::temp_dir().join("scapegoat_mkdir_test");
        let _ = std::fs::remove_dir_all(&temp);
        let _ = std::fs::create_dir_all(&temp);

        let tools = WorkspaceTools::new(&temp).unwrap();

        let result = tools.write_file("sub/dir/new_file.md", "content");
        assert!(result.is_ok(), "Write 应该自动创建父目录: {:?}", result);

        let _ = std::fs::remove_dir_all(temp);
    }
}
