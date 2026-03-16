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
//    - Read 一次后可多次 Edit 同一文件（Claude Code / OpenCode 模式）
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

/// Grep 工具单次搜索返回的最大匹配数
/// 防止搜索结果过多导致上下文爆炸
const MAX_GREP_RESULTS: usize = 50;

/// Grep 工具跳过大于此大小的文件（500 KB）
/// 大文件通常是数据文件或二进制文件，跳过以提高搜索效率
const MAX_GREP_FILE_BYTES: u64 = 500_000;

/// Grep 工具每个匹配行的最大显示字符数
/// 截断过长的行，防止单条匹配占用过多上下文
const MAX_GREP_LINE_DISPLAY: usize = 200;

/// Grep 工具递归搜索的最大目录深度
/// 防止目录结构过深导致搜索卡住
const MAX_GREP_DEPTH: usize = 10;

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
/// 核心规则：Edit 前必须先 Read 同一文件（参考 Claude Code / OpenCode 的约束模式）
/// Read 一次后可对同一文件多次 Edit，直到会话结束
#[derive(Debug, Default)]
pub struct WorkspaceToolSession {
    /// 当前会话中已 Read 过的文件路径集合（规范化后的相对路径）
    /// 只有在此集合中的文件才允许 Edit
    /// 一旦 Read 加入集合，整个会话期间持续有效（不因 Edit 失效）
    readable_for_edit: HashSet<String>,
}

impl WorkspaceToolSession {
    /// 标记一个文件已被 Read
    /// 加入集合后在整个会话期间持续有效，允许后续多次 Edit
    fn note_read(&mut self, relative_path: &str) {
        self.readable_for_edit.insert(relative_path.to_string());
    }

    /// 检查文件是否已被 Read（Edit 前置条件）
    /// 未 Read 过的文件直接 Edit 会被拒绝，返回提示信息
    fn require_read(&self, relative_path: &str) -> anyhow::Result<()> {
        if self.readable_for_edit.contains(relative_path) {
            return Ok(());
        }
        bail!(
            "该文件尚未被 Read。必须先使用 Read 工具读取文件内容，然后才能 Edit: {}",
            relative_path
        );
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

        // 0. 写保护检查：messages/raw/ 和 data/ 目录禁止 LLM 写入
        if is_write_protected(&clean_path) {
            bail!(
                "路径 '{}' 位于受保护目录中（messages/raw/ 和 data/ 为只读），禁止写入",
                relative_path
            );
        }

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
    /// - 必须在同一会话中先 Read 过该文件才能 Edit（Claude Code / OpenCode 模式）
    /// - Read 一次后可多次 Edit，无需每次重新 Read
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

        // 写保护检查：messages/raw/ 和 data/ 目录禁止 LLM 编辑
        if is_write_protected(&clean_path) {
            bail!(
                "路径 '{}' 位于受保护目录中（messages/raw/ 和 data/ 为只读），禁止编辑",
                relative_path
            );
        }

        // 检查 Edit 前置条件：必须先 Read（Claude Code / OpenCode 模式）
        session.require_read(&clean_path)?;

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

        // Read 标记保留，允许后续继续 Edit 同一文件（Claude Code / OpenCode 模式）

        Ok(format!(
            "已编辑文件: {} (替换了 {} 处匹配)",
            clean_path, match_count
        ))
    }

    /// Ls 工具：列出工作区内指定目录的内容
    ///
    /// # 安全保障
    /// - 通过 cap-std Dir 限制，无法列出 workspace 外的目录
    /// - 路径遍历攻击被 normalize + cap-std 双重拒绝
    /// - 特殊处理根目录：传入 "/" 或空字符串时列出工作区根
    ///
    /// # 返回格式
    /// 每行一个条目，目录以 `/` 结尾，文件附带大小
    pub fn list_dir(&self, relative_path: &str) -> anyhow::Result<String> {
        // 特殊处理根目录："/", ".", "" 都指向工作区根
        let clean_path = {
            let trimmed = relative_path.trim();
            if trimmed.is_empty() || trimmed == "/" || trimmed == "." || trimmed == "./" {
                ".".to_string()
            } else {
                normalize_relative_path(trimmed)?
            }
        };

        // 通过 cap-std 打开目录（自动限制在沙箱内）
        let sub_dir = if clean_path == "." {
            // 列出根目录：直接使用沙箱根句柄
            // cap-std 的 open_dir(".") 返回自身的引用
            self.dir.open_dir(".").with_context(|| {
                "无法打开工作区根目录".to_string()
            })?
        } else {
            self.dir.open_dir(&clean_path).with_context(|| {
                format!("目录不存在或无法访问: {}", clean_path)
            })?
        };

        // 收集目录条目
        let mut entries: Vec<String> = Vec::new();
        for entry in sub_dir.entries().with_context(|| {
            format!("无法读取目录内容: {}", clean_path)
        })? {
            let entry = entry.with_context(|| {
                format!("读取目录条目失败: {}", clean_path)
            })?;
            let name = entry.file_name().to_string_lossy().to_string();
            // 获取条目类型（文件 / 目录）
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => {
                    entries.push(format!("{}/", name));
                }
                Ok(_) => {
                    // 文件：附带大小信息
                    let size_str = entry
                        .metadata()
                        .map(|m| format_file_size(m.len()))
                        .unwrap_or_else(|_| "?".to_string());
                    entries.push(format!("{}  ({})", name, size_str));
                }
                Err(_) => {
                    entries.push(name);
                }
            }
        }

        // 按名称排序（目录优先）
        entries.sort_by(|a, b| {
            let a_is_dir = a.ends_with('/');
            let b_is_dir = b.ends_with('/');
            match (a_is_dir, b_is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.to_lowercase().cmp(&b.to_lowercase()),
            }
        });

        if entries.is_empty() {
            Ok("(空目录)".to_string())
        } else {
            Ok(entries.join("\n"))
        }
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

    /// Grep 工具：在工作区内搜索文件内容
    /// 通过 cap-std Dir 句柄递归遍历目录，仅执行读取操作
    ///
    /// # 安全保障
    /// - 所有文件访问通过 cap-std Dir 句柄，自动限制在工作区内
    /// - 仅执行读操作（read_to_string / entries / metadata），不暴露任何写能力
    /// - 跳过二进制文件（非 UTF-8）和大文件（> MAX_GREP_FILE_BYTES）
    /// - 递归深度限制（MAX_GREP_DEPTH）和结果数量限制（MAX_GREP_RESULTS）
    ///
    /// # 参数
    /// - `pattern`: 搜索文本（子串匹配）
    /// - `path`: 搜索起始目录（相对于工作区根，空字符串或 "." 表示根）
    /// - `file_ext`: 可选的文件扩展名过滤（如 "md"、"jsonl"，不含前导点号）
    /// - `case_insensitive`: 是否忽略大小写
    pub fn grep(
        &self,
        pattern: &str,
        path: &str,
        file_ext: Option<&str>,
        case_insensitive: bool,
    ) -> anyhow::Result<String> {
        // 不允许空搜索模式
        if pattern.is_empty() {
            bail!("搜索模式不能为空");
        }

        // 规范化搜索路径（支持根目录的多种写法）
        let base_path = {
            let trimmed = path.trim();
            if trimmed.is_empty() || trimmed == "/" || trimmed == "." || trimmed == "./" {
                ".".to_string()
            } else {
                normalize_relative_path(trimmed)?
            }
        };

        // 打开搜索起始目录
        let base_dir = if base_path == "." {
            self.dir.open_dir(".").with_context(|| "无法打开工作区根目录")?
        } else {
            self.dir.open_dir(&base_path).with_context(|| {
                format!("目录不存在或无法访问: {}", base_path)
            })?
        };

        // 大小写不敏感时，将搜索模式转为小写
        let match_pattern = if case_insensitive {
            pattern.to_lowercase()
        } else {
            pattern.to_string()
        };

        // 收集匹配结果
        let mut matches: Vec<GrepMatch> = Vec::new();
        let display_prefix = if base_path == "." {
            String::new()
        } else {
            format!("{}/", base_path)
        };

        // 递归搜索目录树
        grep_dir_recursive(
            &base_dir,
            &display_prefix,
            &match_pattern,
            case_insensitive,
            file_ext,
            &mut matches,
            MAX_GREP_RESULTS,
            0, // 当前深度
        )?;

        // 格式化输出
        if matches.is_empty() {
            Ok(format!("未找到匹配 \"{}\" 的内容", pattern))
        } else {
            let count = matches.len();
            let lines: Vec<String> = matches
                .iter()
                .map(|m| format!("{}:{}: {}", m.file, m.line_number, m.content))
                .collect();
            let mut output = lines.join("\n");
            output.push_str(&format!(
                "\n\n共找到 {} 处匹配{}",
                count,
                if count >= MAX_GREP_RESULTS {
                    format!("（已达上限 {} 条，可能还有更多）", MAX_GREP_RESULTS)
                } else {
                    String::new()
                }
            ));
            Ok(output)
        }
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
// 文件大小格式化
// ============================================================================

/// 将字节数格式化为人类可读的大小字符串
fn format_file_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

// ============================================================================
// 写保护检查
// ============================================================================

/// 检查路径是否位于写保护目录
/// messages/raw/ 目录存储消息原始数据（JSONL），禁止 LLM 篡改
/// data/ 目录存储 SQLite 数据库文件（图片注册表、消息索引），禁止 LLM 篡改
///
/// 注意：messages/all/、messages/master/、messages/self/ 等 MD 格式目录
/// 允许 LLM 自由修改（如编辑摘要、更新标注等），不在写保护范围内。
///
/// # 安全设计
/// - 这是 cap-std 沙箱之上的额外防御层
/// - cap-std 确保 LLM 无法访问工作区外的文件
/// - is_write_protected 确保 LLM 无法修改工作区内的关键数据文件
/// - 大小写不敏感比较（防止 Windows 上的大小写绕过）
fn is_write_protected(normalized_path: &str) -> bool {
    // 统一为小写和正斜杠进行比较（跨平台兼容）
    let lower = normalized_path.to_lowercase().replace('\\', "/");
    // messages/raw/ 目录：消息原始 JSONL 数据（仅此子目录受保护）
    lower == "messages/raw"
        || lower.starts_with("messages/raw/")
        // data/ 目录：SQLite 数据库文件
        || lower == "data"
        || lower.starts_with("data/")
        // sessions/ 目录：会话 JSONL 记录文件（系统管理，AI 只读）
        || lower == "sessions"
        || lower.starts_with("sessions/")
}

// ============================================================================
// Grep 辅助结构和函数
// ============================================================================

/// Grep 单条匹配结果
struct GrepMatch {
    /// 文件路径（相对于工作区根）
    file: String,
    /// 匹配行号（1-based）
    line_number: usize,
    /// 匹配行内容（截断后）
    content: String,
}

/// 递归搜索目录树中的文件内容
/// 通过 cap-std Dir 句柄遍历，仅执行读取操作
///
/// # 参数
/// - `dir`: 当前 cap-std 目录句柄
/// - `path_prefix`: 当前路径前缀（用于结果显示）
/// - `pattern`: 搜索模式（大小写不敏感时已转为小写）
/// - `case_insensitive`: 是否忽略大小写
/// - `file_ext`: 可选的文件扩展名过滤
/// - `results`: 匹配结果收集器
/// - `max_results`: 最大匹配数
/// - `depth`: 当前递归深度
fn grep_dir_recursive(
    dir: &Dir,
    path_prefix: &str,
    pattern: &str,
    case_insensitive: bool,
    file_ext: Option<&str>,
    results: &mut Vec<GrepMatch>,
    max_results: usize,
    depth: usize,
) -> anyhow::Result<()> {
    // 达到最大结果数或最大深度时停止
    if results.len() >= max_results || depth > MAX_GREP_DEPTH {
        return Ok(());
    }

    // 遍历当前目录的所有条目
    let entries = dir.entries().with_context(|| {
        format!("无法读取目录: {}", path_prefix)
    })?;

    for entry in entries {
        // 达到最大结果数时提前退出
        if results.len() >= max_results {
            return Ok(());
        }

        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue, // 跳过无法读取的条目
        };

        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // 跳过隐藏文件和目录（以 . 开头）
        if name_str.starts_with('.') {
            continue;
        }

        // 构造完整显示路径
        let full_path = format!("{}{}", path_prefix, name_str);

        // 根据文件类型处理
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if file_type.is_dir() {
            // 递归进入子目录
            if let Ok(sub_dir) = dir.open_dir(&*name) {
                let sub_prefix = format!("{}/", full_path);
                grep_dir_recursive(
                    &sub_dir,
                    &sub_prefix,
                    pattern,
                    case_insensitive,
                    file_ext,
                    results,
                    max_results,
                    depth + 1,
                )?;
            }
        } else if file_type.is_file() {
            // 文件扩展名过滤
            if let Some(ext) = file_ext {
                let file_ext_match = name_str
                    .rsplit('.')
                    .next()
                    .is_some_and(|e| e.eq_ignore_ascii_case(ext));
                if !file_ext_match {
                    continue;
                }
            }

            // 检查文件大小（跳过过大文件）
            if let Ok(meta) = dir.metadata(&*name) {
                if meta.len() > MAX_GREP_FILE_BYTES {
                    continue;
                }
            }

            // 读取文件内容（跳过非 UTF-8 文件）
            let Ok(content) = dir.read_to_string(&*name) else {
                continue;
            };

            // 逐行搜索匹配
            for (i, line) in content.lines().enumerate() {
                if results.len() >= max_results {
                    return Ok(());
                }

                let is_match = if case_insensitive {
                    line.to_lowercase().contains(pattern)
                } else {
                    line.contains(pattern)
                };

                if is_match {
                    // 截断过长的行
                    let display_content = if line.len() > MAX_GREP_LINE_DISPLAY {
                        format!("{}...", &line[..MAX_GREP_LINE_DISPLAY])
                    } else {
                        line.to_string()
                    };

                    results.push(GrepMatch {
                        file: full_path.clone(),
                        line_number: i + 1, // 1-based
                        content: display_content,
                    });
                }
            }
        }
    }

    Ok(())
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

    /// 测试：Read → Edit 会话规则（Claude Code / OpenCode 模式）
    #[test]
    fn session_read_before_edit() {
        let mut session = WorkspaceToolSession::default();

        // 未 Read 过的文件不能 Edit
        assert!(session.require_read("test.md").is_err());

        // Read 后可以 Edit
        session.note_read("test.md");
        assert!(session.require_read("test.md").is_ok());

        // Read 一次后持续有效，允许多次 Edit（不再 invalidate）
        assert!(session.require_read("test.md").is_ok());

        // 其他文件仍然不能 Edit
        assert!(session.require_read("other.md").is_err());
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

        // Edit 后无需重新 Read，可直接再次 Edit（Claude Code / OpenCode 模式）
        let result = tools.edit_file(
            &mut session,
            "test_file.md",
            "goodbye",
            "hello",
            false,
        );
        assert!(result.is_ok(), "Read 一次后应能多次 Edit: {:?}", result);

        // 验证最终文件内容正确（hello → goodbye → hello）
        let content = tools.read_file(&mut session, "test_file.md").unwrap();
        assert_eq!(content, "hello world");

        // 清理
        let _ = std::fs::remove_dir_all(temp);
    }

    /// 测试：Ls 列出目录内容
    #[test]
    fn sandbox_ls() {
        let temp = std::env::temp_dir().join("scapegoat_ls_test");
        let _ = std::fs::remove_dir_all(&temp);
        let _ = std::fs::create_dir_all(&temp);

        let tools = WorkspaceTools::new(&temp).unwrap();

        // 创建测试文件和目录
        tools.write_file("hello.md", "hello").unwrap();
        tools.write_file("sub/nested.txt", "nested").unwrap();

        // Ls 根目录
        let listing = tools.list_dir("/").unwrap();
        assert!(listing.contains("sub/"), "应包含子目录 sub/");
        assert!(listing.contains("hello.md"), "应包含文件 hello.md");

        // Ls 子目录
        let listing = tools.list_dir("sub").unwrap();
        assert!(listing.contains("nested.txt"), "应包含文件 nested.txt");

        // Ls 空字符串 = 根目录
        let listing = tools.list_dir("").unwrap();
        assert!(listing.contains("sub/"), "空路径应列出根目录");

        // Ls 不存在的目录
        let result = tools.list_dir("nonexistent");
        assert!(result.is_err(), "不存在的目录应返回错误");

        // Ls 路径遍历被拒绝
        let result = tools.list_dir("../");
        assert!(result.is_err(), "路径遍历应被拒绝");

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

    // ---- 写保护测试 ----

    /// 测试：is_write_protected 正确识别受保护目录
    #[test]
    fn write_protected_paths() {
        // messages/raw/ 目录受保护（JSONL 原始数据）
        assert!(is_write_protected("messages/raw"));
        assert!(is_write_protected("messages/raw/2026-03-10_001.jsonl"));
        assert!(is_write_protected("Messages/Raw/test.jsonl")); // 大小写不敏感

        // data/ 目录受保护
        assert!(is_write_protected("data"));
        assert!(is_write_protected("data/images.db"));
        assert!(is_write_protected("data/messages.db"));
        assert!(is_write_protected("Data/test.db")); // 大小写不敏感

        // sessions/ 目录受保护（会话 JSONL 记录文件）
        assert!(is_write_protected("sessions"));
        assert!(is_write_protected("sessions/2026-03-13_001.jsonl"));
        assert!(is_write_protected("Sessions/test.jsonl")); // 大小写不敏感

        // messages/ 下的其他子目录不受保护（MD 格式允许自由修改）
        assert!(!is_write_protected("messages"));
        assert!(!is_write_protected("messages/all/2026-03-10_001.md"));
        assert!(!is_write_protected("messages/master/data.md"));
        assert!(!is_write_protected("messages/self/2026-03-10_001.md"));

        // 其他路径不受保护
        assert!(!is_write_protected("MEMORY.md"));
        assert!(!is_write_protected("AGENTS.md"));
        assert!(!is_write_protected("skills/test/SKILL.md"));
        assert!(!is_write_protected("my_data.txt")); // "data" 是前缀但不是目录
        assert!(!is_write_protected("messages.md")); // "messages" 是前缀但不是目录
    }

    /// 测试：Write 和 Edit 被写保护拦截
    #[test]
    fn write_edit_blocked_on_protected_dirs() {
        let temp = std::env::temp_dir().join("scapegoat_write_protect_test");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let tools = WorkspaceTools::new(&temp).unwrap();
        let mut session = WorkspaceToolSession::default();

        // Write 到 messages/raw/ 应被拒绝
        let result = tools.write_file("messages/raw/test.jsonl", "hacked");
        assert!(result.is_err(), "Write messages/raw/ 应被写保护拒绝");
        assert!(
            result.unwrap_err().to_string().contains("受保护目录"),
            "错误消息应提及受保护目录"
        );

        // Write 到 messages/all/ 应允许（MD 格式目录不受保护）
        let result = tools.write_file("messages/all/test.md", "ok content");
        assert!(result.is_ok(), "Write messages/all/ 不应被写保护拒绝");

        // Write 到 data/ 应被拒绝
        let result = tools.write_file("data/test.db", "hacked");
        assert!(result.is_err(), "Write data/ 应被写保护拒绝");

        // 先创建一个文件用于测试 Edit
        tools.write_file("normal.txt", "hello").unwrap();
        let _ = tools.read_file(&mut session, "normal.txt").unwrap();
        // Edit 正常文件应该成功
        let result = tools.edit_file(&mut session, "normal.txt", "hello", "world", false);
        assert!(result.is_ok(), "Edit 正常文件应成功");

        // 清理
        let _ = std::fs::remove_dir_all(temp);
    }

    // ---- Grep 工具测试 ----

    /// 测试：Grep 在工作区中搜索文本
    #[test]
    fn grep_basic_search() {
        let temp = std::env::temp_dir().join("scapegoat_grep_test");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let tools = WorkspaceTools::new(&temp).unwrap();

        // 创建测试文件
        tools.write_file("readme.md", "Hello World\nThis is a test\nHello Again").unwrap();
        tools.write_file("notes/todo.txt", "Buy groceries\nHello from notes\nDone").unwrap();

        // 搜索 "Hello"
        let result = tools.grep("Hello", ".", None, false).unwrap();
        assert!(result.contains("readme.md:1:"), "应找到 readme.md 第 1 行");
        assert!(result.contains("readme.md:3:"), "应找到 readme.md 第 3 行");
        assert!(result.contains("notes/todo.txt:2:"), "应找到 notes/todo.txt 第 2 行");
        assert!(result.contains("共找到 3 处匹配"), "应报告 3 处匹配");

        // 大小写不敏感搜索
        let result = tools.grep("hello", ".", None, true).unwrap();
        assert!(result.contains("共找到 3 处匹配"), "大小写不敏感应找到 3 处");

        // 搜索不存在的文本
        let result = tools.grep("nonexistent_text_xyz", ".", None, false).unwrap();
        assert!(result.contains("未找到匹配"), "搜索不到应返回未找到");

        // 清理
        let _ = std::fs::remove_dir_all(temp);
    }

    /// 测试：Grep 文件扩展名过滤
    #[test]
    fn grep_file_extension_filter() {
        let temp = std::env::temp_dir().join("scapegoat_grep_ext_test");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let tools = WorkspaceTools::new(&temp).unwrap();

        // 创建不同扩展名的文件
        tools.write_file("data.md", "search target here").unwrap();
        tools.write_file("data.txt", "search target here too").unwrap();
        tools.write_file("data.jsonl", "search target in jsonl").unwrap();

        // 只搜索 .md 文件
        let result = tools.grep("search target", ".", Some("md"), false).unwrap();
        assert!(result.contains("data.md"), "应找到 data.md");
        assert!(!result.contains("data.txt"), "不应找到 data.txt");
        assert!(!result.contains("data.jsonl"), "不应找到 data.jsonl");

        // 只搜索 .jsonl 文件
        let result = tools.grep("search target", ".", Some("jsonl"), false).unwrap();
        assert!(result.contains("data.jsonl"), "应找到 data.jsonl");
        assert!(!result.contains("data.md"), "不应找到 data.md");

        // 清理
        let _ = std::fs::remove_dir_all(temp);
    }
}
