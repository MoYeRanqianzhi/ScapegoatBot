// src/tools/workspace.rs
// 工作区文件工具模块
// 通过 cap-std 库将所有文件操作限制在 workspace 目录内
// 提供 Read / Write / Edit 三个内置工具，确保 QQ Bot 处于受限的沙箱环境
//
// ============================================================================
// 安全模型概述（Security Model）
// ============================================================================
//
// 核心安全目标："即便 Agent 完全被黑客控制，也无法造成任何伤害"
//
// 1. **路径限制**（Path Containment）
//    - 所有文件操作通过 cap-std::fs::Dir 执行，硬性限制在 workspace 目录内
//    - 防御纵深：在 cap-std 之前，由 normalize_relative_path() 拦截:
//      · 路径遍历（..）
//      · 绝对路径（/ 或 \ 开头）
//      · Windows 盘符路径（C:\ 等）
//      · 空字节注入
//      · 超长路径 / 超深嵌套
//
// 2. **写入大小限制**（Write Size Limits）
//    - 单次 Write/Edit 内容上限：MAX_WRITE_CONTENT_BYTES（1 MB）
//    - 单次 Read 文件上限：MAX_READ_FILE_BYTES（200 KB）
//    - Edit 必须先 Read → 文件必须 < 200 KB → Edit 结果必须 < 1 MB
//
// 3. **磁盘空间保护**（Disk Space Protection）
//    - 每次写入前检查磁盘可用空间，低于 MIN_FREE_DISK_SPACE 则拒绝写入
//    - 使用 fs2 crate 进行跨平台磁盘空间查询
//
// 4. **工作区容量限制**（Workspace Capacity Limits）
//    - 创建新文件前检查工作区总文件数 + 总大小
//    - 超过阈值时拒绝创建新文件
//
// 5. **无执行能力**（No Execution）
//    - Agent 没有 Bash / Shell / Exec 等工具
//    - 只能通过 Read / Write / Edit 操作文本文件

use anyhow::{Context as _, bail};
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// ============================================================================
// 安全常量（Security Limits）
// ============================================================================

/// 工作区文件读取的最大字节数（200 KB）
/// 超过此大小的文件不允许读取，防止撑爆上下文窗口
const MAX_READ_FILE_BYTES: u64 = 200_000;

/// 单次 Write 或 Edit 结果允许的最大内容字节数（1 MB）
/// 防止 Agent 通过单次写入消耗大量磁盘空间
/// 配合 max_steps=32 的限制，每轮最多写入 32 MB
const MAX_WRITE_CONTENT_BYTES: usize = 1_048_576; // 1 MB

/// 写入操作要求的最小可用磁盘空间（100 MB）
/// 低于此阈值时拒绝所有写入操作，保护宿主机磁盘
const MIN_FREE_DISK_SPACE: u64 = 100 * 1024 * 1024; // 100 MB

/// 工作区允许的最大总文件大小（2 GB）
/// 防止 Agent 通过反复创建文件逐步填满磁盘
/// 注意：此限制包含系统存储的消息文件，设置较宽松以避免误拦
const MAX_WORKSPACE_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GB

/// 工作区允许的最大文件数量
/// 防止 Agent 创建海量文件导致 inode 耗尽或文件系统性能退化
const MAX_WORKSPACE_FILE_COUNT: usize = 50_000;

/// 路径允许的最大字符长度
/// 防止超长路径导致文件系统异常
const MAX_PATH_LENGTH: usize = 255;

/// 路径允许的最大嵌套深度（按 / 分割后的分量数）
/// 防止创建过深的目录嵌套结构
const MAX_PATH_DEPTH: usize = 16;

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
    /// workspace 根目录的绝对路径（用于磁盘空间检查和日志）
    root_abs: PathBuf,
}

impl WorkspaceTools {
    /// 创建工作区文件工具
    /// 打开 workspace_dir 作为 cap-std 沙箱根，所有后续文件操作都被限制在此目录内
    pub fn new(workspace_dir: &Path) -> anyhow::Result<Self> {
        // 确保目录存在
        std::fs::create_dir_all(workspace_dir).with_context(|| {
            format!("无法创建工作区目录: {}", workspace_dir.display())
        })?;

        // 将路径规范化为绝对路径（用于磁盘空间检查等系统级操作）
        // canonicalize 要求路径已存在，上面 create_dir_all 已确保
        let root_abs = workspace_dir.canonicalize().with_context(|| {
            format!(
                "无法规范化工作区路径: {}",
                workspace_dir.display()
            )
        })?;

        // 获得 ambient authority 后打开沙箱目录
        // 这是 cap-std 的安全模型：authority 只在此处获取一次
        // 之后所有操作都通过 Dir 句柄进行，自动限制在此目录内
        let dir = Dir::open_ambient_dir(workspace_dir, ambient_authority()).with_context(|| {
            format!("无法打开工作区沙箱目录: {}", workspace_dir.display())
        })?;

        Ok(Self { dir, root_abs })
    }

    /// Read 工具：读取工作区内的 UTF-8 文本文件
    ///
    /// # 安全保障
    /// - 通过 cap-std Dir 限制，无法读取 workspace 外的文件
    /// - 路径遍历攻击（如 `../../etc/passwd`）被 normalize + cap-std 双重拒绝
    /// - 文件大小限制在 MAX_READ_FILE_BYTES 以内
    pub fn read_file(
        &self,
        session: &mut WorkspaceToolSession,
        relative_path: &str,
    ) -> anyhow::Result<String> {
        // 规范化并校验路径（含防御纵深检查）
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
    /// - 内容大小限制在 MAX_WRITE_CONTENT_BYTES 以内
    /// - 写入前检查磁盘可用空间（不低于 MIN_FREE_DISK_SPACE）
    /// - 写入前检查工作区总大小和文件数量
    /// - 自动创建必要的父目录
    pub fn write_file(
        &self,
        relative_path: &str,
        content: &str,
    ) -> anyhow::Result<String> {
        let clean_path = normalize_relative_path(relative_path)?;

        // ======== 安全检查 ========

        // 1. 检查写入内容大小
        let content_bytes = content.len();
        if content_bytes > MAX_WRITE_CONTENT_BYTES {
            bail!(
                "写入内容过大（{} 字节，上限 {} 字节 ≈ {} MB）",
                content_bytes,
                MAX_WRITE_CONTENT_BYTES,
                MAX_WRITE_CONTENT_BYTES / (1024 * 1024)
            );
        }

        // 2. 检查磁盘可用空间
        self.check_disk_space()?;

        // 3. 检查工作区文件数量和总大小（仅 Write 创建新文件时检查）
        self.check_workspace_limits()?;

        // 4. 检查文件是否已存在（Write 只能创建新文件）
        if self.dir.exists(&clean_path) {
            bail!(
                "文件已存在，无法覆盖。如需修改请使用 Edit 工具: {}",
                clean_path
            );
        }

        // ======== 执行写入 ========

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

        Ok(format!("已创建文件: {} ({} 字节)", clean_path, content_bytes))
    }

    /// Edit 工具：编辑已存在的 UTF-8 文本文件
    ///
    /// # 安全保障
    /// - 通过 cap-std Dir 限制，无法编辑 workspace 外的文件
    /// - 必须在同一会话中先 Read 过该文件才能 Edit
    /// - Edit 后必须重新 Read 才能再次 Edit（确保模型看到最新内容）
    /// - 编辑后的文件大小不得超过 MAX_WRITE_CONTENT_BYTES
    /// - 写入前检查磁盘可用空间
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

        // ======== 安全检查 ========

        // 检查编辑后的文件大小不超过写入限制
        if new_content.len() > MAX_WRITE_CONTENT_BYTES {
            bail!(
                "编辑后文件过大（{} 字节，上限 {} 字节 ≈ {} MB）。请减少替换内容的大小。",
                new_content.len(),
                MAX_WRITE_CONTENT_BYTES,
                MAX_WRITE_CONTENT_BYTES / (1024 * 1024)
            );
        }

        // 检查磁盘可用空间（编辑可能增大文件）
        self.check_disk_space()?;

        // ======== 执行写入 ========

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

    /// 检查磁盘可用空间是否满足最低要求
    /// 使用 fs2 crate 进行跨平台磁盘空间查询
    /// 采用 fail-closed 策略：无法获取空间信息时也拒绝写入
    fn check_disk_space(&self) -> anyhow::Result<()> {
        match fs2::available_space(&self.root_abs) {
            Ok(available) if available < MIN_FREE_DISK_SPACE => {
                bail!(
                    "磁盘可用空间不足（当前 {:.1} MB，要求至少 {:.1} MB），写入操作被拒绝",
                    available as f64 / (1024.0 * 1024.0),
                    MIN_FREE_DISK_SPACE as f64 / (1024.0 * 1024.0)
                );
            }
            Ok(_) => Ok(()),
            Err(e) => {
                // 无法获取磁盘空间信息时，安全起见拒绝写入（fail-closed）
                tracing::warn!("无法获取磁盘可用空间: {}", e);
                bail!(
                    "无法验证磁盘可用空间（{}），写入操作被拒绝。请检查工作区路径是否有效。",
                    e
                );
            }
        }
    }

    /// 检查工作区文件数量和总大小是否在安全限制内
    /// 仅在 Write（创建新文件）时调用，Edit 不改变文件数量
    fn check_workspace_limits(&self) -> anyhow::Result<()> {
        let (total_bytes, file_count) = estimate_workspace_usage(&self.root_abs);

        if file_count >= MAX_WORKSPACE_FILE_COUNT {
            bail!(
                "工作区文件数量已达上限（当前 {} 个，上限 {} 个），无法创建新文件",
                file_count,
                MAX_WORKSPACE_FILE_COUNT
            );
        }

        if total_bytes >= MAX_WORKSPACE_TOTAL_BYTES {
            bail!(
                "工作区总大小已达上限（当前 {:.1} MB，上限 {:.1} MB），无法创建新文件",
                total_bytes as f64 / (1024.0 * 1024.0),
                MAX_WORKSPACE_TOTAL_BYTES as f64 / (1024.0 * 1024.0)
            );
        }

        Ok(())
    }

    /// 获取工作区根目录显示路径（用于日志）
    pub fn root_display(&self) -> &Path {
        &self.root_abs
    }
}

// ============================================================================
// 工作区使用量估算
// ============================================================================

/// 估算工作区当前使用量（文件总大小和数量）
/// 使用 std::fs 递归遍历目录（非 cap-std，因为这是系统级安全检查）
/// 遍历条目有上限（MAX_WALK_ENTRIES），防止在超大目录中阻塞
fn estimate_workspace_usage(root: &Path) -> (u64, usize) {
    /// 单次遍历的最大条目数，超过此数量则停止遍历
    /// 此时已收集的统计数据足以判断是否超限
    const MAX_WALK_ENTRIES: usize = 100_000;

    let mut total_bytes: u64 = 0;
    let mut file_count: usize = 0;
    let mut walked: usize = 0;
    let mut dirs_to_visit = vec![root.to_path_buf()];

    while let Some(dir) = dirs_to_visit.pop() {
        // 达到遍历上限时停止
        if walked >= MAX_WALK_ENTRIES {
            break;
        }

        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };

        for entry in entries.flatten() {
            walked += 1;
            if walked >= MAX_WALK_ENTRIES {
                break;
            }

            let Ok(meta) = entry.metadata() else {
                continue;
            };

            if meta.is_file() {
                total_bytes = total_bytes.saturating_add(meta.len());
                file_count += 1;
            } else if meta.is_dir() {
                dirs_to_visit.push(entry.path());
            }
        }
    }

    (total_bytes, file_count)
}

// ============================================================================
// 路径规范化与安全校验
// ============================================================================

/// 规范化并安全校验相对路径
///
/// 执行以下防御纵深检查（在 cap-std 沙箱之前拦截）：
/// 1. 拒绝空路径
/// 2. 拒绝包含空字节的路径（防止 null 字节注入）
/// 3. 去除前导 `./` 和 `.\`
/// 4. 拒绝绝对路径（`/`、`\` 开头、Windows 盘符）
/// 5. 拒绝路径遍历（包含 `..` 分量）
/// 6. 拒绝超长路径（> MAX_PATH_LENGTH 字符）
/// 7. 拒绝过深嵌套（> MAX_PATH_DEPTH 层）
fn normalize_relative_path(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim();

    // 1. 拒绝空路径
    if trimmed.is_empty() {
        bail!("路径不能为空");
    }

    // 2. 拒绝包含空字节的路径（防止 null 字节注入攻击）
    if trimmed.bytes().any(|b| b == 0) {
        bail!("路径包含非法字符（null 字节）");
    }

    // 3. 去除前导 "./" 或 ".\\"
    let cleaned = trimmed
        .strip_prefix("./")
        .or_else(|| trimmed.strip_prefix(".\\"))
        .unwrap_or(trimmed);

    // 4a. 拒绝 Unix 风格绝对路径（/ 开头）和 Windows UNC 路径（\ 开头）
    if cleaned.starts_with('/') || cleaned.starts_with('\\') {
        bail!("不允许使用绝对路径，请使用相对于工作区根目录的路径: {}", raw);
    }

    // 4b. 拒绝 Windows 盘符绝对路径（如 C:\ 或 D:/）
    if cleaned.len() >= 2
        && cleaned.as_bytes()[0].is_ascii_alphabetic()
        && cleaned.as_bytes()[1] == b':'
    {
        bail!("不允许使用绝对路径（Windows 盘符）: {}", raw);
    }

    // 5. 防御纵深：拒绝路径遍历（.. 分量）
    //    按 / 和 \ 分割路径，检查每个分量是否为 ".."
    //    注意：cap-std 自身也会拒绝路径遍历，这里是额外的前置防御
    let normalized_for_check = cleaned.replace('\\', "/");
    if normalized_for_check.split('/').any(|part| part == "..") {
        bail!("路径不能包含 `..`（禁止路径遍历）: {}", raw);
    }

    // 6. 拒绝超长路径
    if cleaned.len() > MAX_PATH_LENGTH {
        bail!(
            "路径过长（{} 字符，上限 {} 字符）: {}",
            cleaned.len(),
            MAX_PATH_LENGTH,
            raw
        );
    }

    // 7. 拒绝过深嵌套
    let depth = normalized_for_check.split('/').count();
    if depth > MAX_PATH_DEPTH {
        bail!(
            "路径嵌套过深（{} 层，上限 {} 层）: {}",
            depth,
            MAX_PATH_DEPTH,
            raw
        );
    }

    Ok(cleaned.to_string())
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 路径规范化测试 ----

    /// 测试：正常路径规范化
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

    /// 测试：路径遍历被防御纵深拒绝（不依赖 cap-std）
    #[test]
    fn path_traversal_defense_in_depth() {
        // 基础遍历
        assert!(normalize_relative_path("../secret").is_err());
        assert!(normalize_relative_path("../../etc/passwd").is_err());
        // 嵌套遍历
        assert!(normalize_relative_path("foo/../../etc/passwd").is_err());
        assert!(normalize_relative_path("a/../b").is_err());
        // Windows 风格遍历
        assert!(normalize_relative_path("..\\secret").is_err());
        assert!(normalize_relative_path("foo\\..\\..\\secret").is_err());
        // 混合风格
        assert!(normalize_relative_path("a\\..\\..\\b").is_err());
    }

    /// 测试：Windows 盘符路径被拒绝
    #[test]
    fn windows_drive_letter_rejected() {
        assert!(normalize_relative_path("C:\\Windows\\System32").is_err());
        assert!(normalize_relative_path("D:/secret/file.txt").is_err());
        assert!(normalize_relative_path("c:/test").is_err());
    }

    /// 测试：空字节路径被拒绝
    #[test]
    fn null_byte_in_path_rejected() {
        assert!(normalize_relative_path("file\0.txt").is_err());
        assert!(normalize_relative_path("test\0/../../../etc/passwd").is_err());
    }

    /// 测试：超长路径被拒绝
    #[test]
    fn overlong_path_rejected() {
        let long_path = "a".repeat(MAX_PATH_LENGTH + 1);
        assert!(normalize_relative_path(&long_path).is_err());
    }

    /// 测试：过深嵌套路径被拒绝
    #[test]
    fn deep_nesting_rejected() {
        // 构造超过 MAX_PATH_DEPTH 层的路径
        let deep: String = (0..MAX_PATH_DEPTH + 1)
            .map(|i| format!("d{i}"))
            .collect::<Vec<_>>()
            .join("/");
        assert!(normalize_relative_path(&deep).is_err());

        // 刚好在限制内的应该通过
        let ok: String = (0..MAX_PATH_DEPTH)
            .map(|i| format!("d{i}"))
            .collect::<Vec<_>>()
            .join("/");
        assert!(normalize_relative_path(&ok).is_ok());
    }

    // ---- Read/Edit 会话规则测试 ----

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

    // ---- 沙箱集成测试 ----

    /// 测试：cap-std 沙箱 Read/Write/Edit
    #[test]
    fn sandbox_read_write_edit() {
        let temp = std::env::temp_dir().join("scapegoat_workspace_test");
        let _ = std::fs::remove_dir_all(&temp);
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
        let _ = std::fs::remove_dir_all(&temp);
        let _ = std::fs::create_dir_all(&temp);

        let tools = WorkspaceTools::new(&temp).unwrap();
        let mut session = WorkspaceToolSession::default();

        // 路径遍历应被 normalize_relative_path 拦截（在 cap-std 之前）
        let result = tools.read_file(&mut session, "../../../etc/passwd");
        assert!(result.is_err(), "路径遍历应该被阻止");

        // 确认错误消息包含遍历相关提示
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(".."),
            "错误消息应提及路径遍历: {err_msg}"
        );

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

    // ---- 写入安全限制测试 ----

    /// 测试：Write 内容大小限制
    #[test]
    fn write_content_size_limit() {
        let temp = std::env::temp_dir().join("scapegoat_write_limit_test");
        let _ = std::fs::remove_dir_all(&temp);
        let _ = std::fs::create_dir_all(&temp);

        let tools = WorkspaceTools::new(&temp).unwrap();

        // 正常大小应该成功
        let result = tools.write_file("small.txt", "hello");
        assert!(result.is_ok());

        // 超过 MAX_WRITE_CONTENT_BYTES 应该失败
        let oversized = "x".repeat(MAX_WRITE_CONTENT_BYTES + 1);
        let result = tools.write_file("big.txt", &oversized);
        assert!(result.is_err(), "超大内容应该被拒绝");
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("过大"), "错误应提及内容过大: {err_msg}");

        // 确认超大文件没有被创建
        assert!(
            !temp.join("big.txt").exists(),
            "超大文件不应该被创建"
        );

        let _ = std::fs::remove_dir_all(temp);
    }

    /// 测试：Edit 结果大小限制
    #[test]
    fn edit_result_size_limit() {
        let temp = std::env::temp_dir().join("scapegoat_edit_limit_test");
        let _ = std::fs::remove_dir_all(&temp);
        let _ = std::fs::create_dir_all(&temp);

        let tools = WorkspaceTools::new(&temp).unwrap();
        let mut session = WorkspaceToolSession::default();

        // 创建一个小文件
        tools.write_file("test.txt", "REPLACE_ME").unwrap();

        // Read 它
        tools.read_file(&mut session, "test.txt").unwrap();

        // 尝试将小文本替换为超大文本
        let huge_replacement = "x".repeat(MAX_WRITE_CONTENT_BYTES + 1);
        let result = tools.edit_file(
            &mut session,
            "test.txt",
            "REPLACE_ME",
            &huge_replacement,
            false,
        );
        assert!(result.is_err(), "超大编辑结果应该被拒绝");
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("过大"), "错误应提及文件过大: {err_msg}");

        // 确认原文件内容没有被修改
        let content = std::fs::read_to_string(temp.join("test.txt")).unwrap();
        assert_eq!(content, "REPLACE_ME", "被拒绝的编辑不应修改文件");

        let _ = std::fs::remove_dir_all(temp);
    }

    /// 测试：workspace 使用量估算
    #[test]
    fn workspace_usage_estimation() {
        let temp = std::env::temp_dir().join("scapegoat_usage_test");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        // 创建几个文件
        std::fs::write(temp.join("a.txt"), "aaaa").unwrap(); // 4 bytes
        std::fs::write(temp.join("b.txt"), "bbbbbb").unwrap(); // 6 bytes
        std::fs::create_dir_all(temp.join("sub")).unwrap();
        std::fs::write(temp.join("sub/c.txt"), "cc").unwrap(); // 2 bytes

        let (total_bytes, file_count) = estimate_workspace_usage(&temp);
        assert_eq!(file_count, 3, "应有 3 个文件");
        assert_eq!(total_bytes, 12, "总大小应为 12 字节");

        let _ = std::fs::remove_dir_all(temp);
    }

    /// 测试：磁盘空间检查在合理系统上通过
    #[test]
    fn disk_space_check_passes_on_normal_system() {
        let temp = std::env::temp_dir().join("scapegoat_disk_test");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let tools = WorkspaceTools::new(&temp).unwrap();

        // 在正常系统上，磁盘空间应该充足
        // 此测试仅验证 check_disk_space 不会因为 API 调用本身而崩溃
        let result = tools.check_disk_space();
        // 不断言 is_ok（在极端情况下可能磁盘确实满了），只确保不 panic
        drop(result);

        let _ = std::fs::remove_dir_all(temp);
    }
}
