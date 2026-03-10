// src/config.rs
// 配置模块
// 从 TOML 文件加载 NapCat 连接配置、白名单（收/发分离）和消息存储配置
// 启动时校验发白名单是收白名单的子集

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// ============================================================================
// 顶层配置结构
// ============================================================================

/// ScapegoatBot 完整配置
/// 对应 config.toml 的根结构
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    /// NapCat WebSocket 连接配置
    pub napcat: NapCatConfig,
    /// 收发白名单配置
    pub whitelist: WhitelistConfig,
    /// 消息存储配置
    pub storage: StorageConfig,
    /// 身份与提示词配置（可选，缺省使用默认值）
    #[serde(default)]
    pub identity: IdentityConfig,
    /// LLM 提供商配置（可选，缺省需要在启动前手动配置）
    #[serde(default)]
    pub llm: LlmConfig,
    /// 上下文压缩配置（可选，缺省使用默认值）
    #[serde(default)]
    pub compaction: CompactionConfig,
    /// Skill 系统配置（可选，缺省无额外 skill 目录）
    #[serde(default)]
    pub skills: SkillsConfig,
    /// 控制通道配置（可选，缺省使用默认端口 19800）
    #[serde(default)]
    pub control: ControlConfig,
}

// ============================================================================
// NapCat 连接配置
// ============================================================================

/// NapCat WebSocket 连接参数
#[derive(Debug, Clone, Deserialize)]
pub struct NapCatConfig {
    /// WebSocket 服务器地址，如 "ws://127.0.0.1:3001"
    pub ws_url: String,
    /// 认证 Token（可选，与 NapCat 端配置一致）
    #[serde(default)]
    pub token: Option<String>,
}

// ============================================================================
// 白名单配置
// ============================================================================

/// 收发白名单配置
/// 核心约束：发白名单必须是收白名单的子集
/// - 不在收白名单中的会话 → 完全忽略（不存储、不处理）
/// - 在收白名单但不在发白名单 → 消息存储，但不能向该会话发消息
#[derive(Debug, Clone, Deserialize)]
pub struct WhitelistConfig {
    /// 可接收消息的群号列表
    #[serde(default)]
    pub recv_groups: Vec<i64>,
    /// 可接收消息的私聊 QQ 号列表
    #[serde(default)]
    pub recv_privates: Vec<i64>,
    /// 可发送消息的群号列表（必须是 recv_groups 的子集）
    #[serde(default)]
    pub send_groups: Vec<i64>,
    /// 可发送消息的私聊 QQ 号列表（必须是 recv_privates 的子集）
    #[serde(default)]
    pub send_privates: Vec<i64>,
}

// ============================================================================
// 存储配置
// ============================================================================

/// 消息存储配置
#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    /// 工作区根目录路径
    #[serde(default = "default_workspace_dir")]
    pub workspace_dir: String,
    /// 单个文件的最大消息条数，超过则拆分为新文件
    #[serde(default = "default_max_messages")]
    pub max_messages_per_file: u32,
}

/// 默认工作区目录
fn default_workspace_dir() -> String {
    "./workspace".to_string()
}

/// 默认每文件最大消息数
fn default_max_messages() -> u32 {
    500
}

// ============================================================================
// 身份与提示词配置
// ============================================================================

/// 身份与提示词注入配置
/// 控制如何构建系统提示词（身份文件、主人账号等）
#[derive(Debug, Clone, Deserialize)]
pub struct IdentityConfig {
    /// 主人账号（QQ 号字符串 或 "self"）
    /// "self" 表示使用当前登录的 QQ 账号作为主人
    /// 主人的消息将被单独复制用于语言风格总结
    #[serde(default = "default_master")]
    pub master: String,
    /// 额外注入的身份文件列表（相对于 workspace_dir 的路径）
    /// 除默认的 AGENTS.md、SOUL.md 等之外，额外注入的自定义文件
    #[serde(default)]
    pub extra_files: Vec<String>,
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            master: default_master(),
            extra_files: Vec::new(),
        }
    }
}

impl IdentityConfig {
    /// 判断 master 是否为 "self" 模式
    pub fn is_master_self(&self) -> bool {
        self.master.eq_ignore_ascii_case("self")
    }

    /// 获取 master 在系统提示词中的显示文本
    /// "self" → "你当前所用账号"
    /// QQ 号 → 原样返回
    pub fn master_display(&self) -> &str {
        if self.is_master_self() {
            "你当前所用账号"
        } else {
            &self.master
        }
    }

    /// 将 master 配置解析为实际 QQ 号
    /// "self" 模式需要传入 bot 自身的 QQ 号
    /// 返回 None 表示配置无效（非 "self" 且无法解析为数字）
    pub fn resolve_master_qq(&self, self_qq: i64) -> Option<i64> {
        if self.is_master_self() {
            Some(self_qq)
        } else {
            self.master.parse::<i64>().ok()
        }
    }
}

/// 默认主人账号：self（使用当前登录账号）
fn default_master() -> String {
    "self".to_string()
}

// ============================================================================
// LLM 提供商配置
// ============================================================================

/// LLM 提供商配置
/// 控制 Agent 使用的语言模型连接参数
#[derive(Debug, Clone, Deserialize)]
pub struct LlmConfig {
    /// OpenAI 兼容的 API 基础 URL（如 https://api.openai.com/v1）
    #[serde(default)]
    pub base_url: String,
    /// API 密钥，作为 Authorization: Bearer 头部发送
    #[serde(default)]
    pub api_key: String,
    /// 模型名称（如 gpt-4o、deepseek-chat 等）
    #[serde(default = "default_llm_model")]
    pub model: String,
    /// 系统指令的角色名称（默认 "system"，某些提供商要求 "developer"）
    #[serde(default)]
    pub system_role_name: Option<String>,
    /// 推理深度/努力程度（可选，传递给支持该参数的模型）
    /// 常见值：none, minimal, low, medium, high, xhigh
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// 每个任务的最大工具调用步数（安全上限）
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: default_llm_model(),
            system_role_name: None,
            reasoning_effort: None,
            max_steps: default_max_steps(),
        }
    }
}

impl LlmConfig {
    /// 返回系统指令的有效角色名称
    /// 缺失或空白 → "system"
    pub fn effective_system_role_name(&self) -> &str {
        self.system_role_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("system")
    }

    /// 返回有效的推理努力（空白视为 None）
    pub fn effective_reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// 检查 LLM 配置是否已就绪（base_url 和 api_key 非空）
    pub fn is_configured(&self) -> bool {
        !self.base_url.trim().is_empty() && !self.api_key.trim().is_empty()
    }
}

/// 默认模型名称
fn default_llm_model() -> String {
    "unknown".to_string()
}

/// 默认最大工具调用步数
const fn default_max_steps() -> u32 {
    32
}

// ============================================================================
// 上下文压缩配置
// ============================================================================

/// 上下文压缩配置
/// 当对话上下文的估算 token 数超过阈值时，将历史消息压缩为摘要
#[derive(Debug, Clone, Deserialize)]
pub struct CompactionConfig {
    /// 是否启用上下文压缩（总开关）
    #[serde(default = "default_compaction_enabled")]
    pub enabled: bool,
    /// 触发压缩的近似总 token 数阈值
    /// 包括：系统提示 + 已有摘要 + 对话消息 + 工具定义
    #[serde(default = "default_compaction_trigger_tokens")]
    pub trigger_tokens: usize,
    /// 保留为最近后缀的近似 token 预算
    /// 这些最新消息保持原样不被压缩
    #[serde(default = "default_compaction_keep_recent_tokens")]
    pub keep_recent_tokens: usize,
    /// 为摘要生成响应预留的最大 token 数
    #[serde(default = "default_compaction_reserve_summary_tokens")]
    pub reserve_summary_tokens: usize,
    /// 拒绝压缩的最少消息数（过少的对话无需压缩）
    #[serde(default = "default_compaction_min_messages")]
    pub min_messages_to_compact: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: default_compaction_enabled(),
            trigger_tokens: default_compaction_trigger_tokens(),
            keep_recent_tokens: default_compaction_keep_recent_tokens(),
            reserve_summary_tokens: default_compaction_reserve_summary_tokens(),
            min_messages_to_compact: default_compaction_min_messages(),
        }
    }
}

const fn default_compaction_enabled() -> bool {
    true
}

const fn default_compaction_trigger_tokens() -> usize {
    180_000
}

const fn default_compaction_keep_recent_tokens() -> usize {
    8_000
}

const fn default_compaction_reserve_summary_tokens() -> usize {
    4_096
}

const fn default_compaction_min_messages() -> usize {
    8
}

// ============================================================================
// Skill 系统配置
// ============================================================================

/// Skill 系统配置
/// 指定要扫描的 skill 目录列表
/// 每个目录下的子目录如果包含 SKILL.md 则被视为一个 Skill
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SkillsConfig {
    /// 要扫描的 skill 目录路径列表
    /// 支持 `~` 展开为用户主目录
    /// 目录不存在时自动跳过（便于跨机器共享配置）
    #[serde(default)]
    pub dirs: Vec<String>,
}

impl SkillsConfig {
    /// 将配置中的路径字符串解析为 PathBuf
    /// 展开 `~` 为用户主目录
    pub fn resolve_dirs(&self) -> Vec<std::path::PathBuf> {
        self.dirs
            .iter()
            .map(|raw| crate::skills::expand_tilde(raw))
            .collect()
    }
}

// ============================================================================
// 控制通道配置
// ============================================================================

/// 控制通道配置
/// 守护进程在 localhost 上监听此端口，接受来自 CLI 的控制指令
#[derive(Debug, Clone, Deserialize)]
pub struct ControlConfig {
    /// 控制端口（默认 19800）
    #[serde(default = "default_control_port")]
    pub port: u16,
}

/// 默认控制端口
/// 从 control 模块获取 DEFAULT_CONTROL_PORT 常量
fn default_control_port() -> u16 {
    crate::control::DEFAULT_CONTROL_PORT
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self { port: default_control_port() }
    }
}

// ============================================================================
// 配置加载与校验
// ============================================================================

impl AppConfig {
    /// 从 TOML 文件加载配置
    /// 加载后自动执行白名单子集约束校验
    pub fn load(path: &Path) -> Result<Self> {
        // 读取配置文件内容
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("无法读取配置文件: {}", path.display()))?;

        // 解析 TOML
        let config: AppConfig = toml::from_str(&content)
            .with_context(|| format!("解析配置文件失败: {}", path.display()))?;

        // 校验白名单约束
        config.validate_whitelist()?;

        Ok(config)
    }

    /// 校验白名单约束：发白名单必须是收白名单的子集
    /// 违反约束时返回错误，列出所有越界的 ID
    fn validate_whitelist(&self) -> Result<()> {
        // 构建收白名单的 HashSet 用于快速查找
        let recv_groups: HashSet<i64> = self.whitelist.recv_groups.iter().copied().collect();
        let recv_privates: HashSet<i64> = self.whitelist.recv_privates.iter().copied().collect();

        // 检查发群白名单中的每个 ID 是否在收群白名单中
        let invalid_send_groups: Vec<i64> = self
            .whitelist
            .send_groups
            .iter()
            .filter(|id| !recv_groups.contains(id))
            .copied()
            .collect();

        // 检查发私聊白名单中的每个 ID 是否在收私聊白名单中
        let invalid_send_privates: Vec<i64> = self
            .whitelist
            .send_privates
            .iter()
            .filter(|id| !recv_privates.contains(id))
            .copied()
            .collect();

        // 如果存在越界 ID，构建详细的错误消息
        if !invalid_send_groups.is_empty() || !invalid_send_privates.is_empty() {
            let mut msg = String::from("发白名单校验失败（发白名单必须是收白名单的子集）：");
            if !invalid_send_groups.is_empty() {
                msg.push_str(&format!(
                    "\n  send_groups 中不在 recv_groups 中的群号: {:?}",
                    invalid_send_groups
                ));
            }
            if !invalid_send_privates.is_empty() {
                msg.push_str(&format!(
                    "\n  send_privates 中不在 recv_privates 中的 QQ 号: {:?}",
                    invalid_send_privates
                ));
            }
            bail!(msg);
        }

        Ok(())
    }

    /// 获取消息存储的根目录路径
    pub fn messages_dir(&self) -> PathBuf {
        PathBuf::from(&self.storage.workspace_dir).join("messages")
    }
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试：合法配置应正常加载
    #[test]
    fn valid_config_parses() {
        let toml_str = r#"
[napcat]
ws_url = "ws://127.0.0.1:3001"
token = "test_token"

[whitelist]
recv_groups = [100, 200, 300]
recv_privates = [1000, 2000]
send_groups = [100, 200]
send_privates = [1000]

[storage]
workspace_dir = "./workspace"
max_messages_per_file = 1000
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(config.validate_whitelist().is_ok());
        assert_eq!(config.napcat.ws_url, "ws://127.0.0.1:3001");
        assert_eq!(config.storage.max_messages_per_file, 1000);
    }

    /// 测试：发群白名单越界应报错
    #[test]
    fn send_group_not_subset_of_recv() {
        let toml_str = r#"
[napcat]
ws_url = "ws://127.0.0.1:3001"

[whitelist]
recv_groups = [100]
send_groups = [100, 999]

[storage]
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let err = config.validate_whitelist().unwrap_err();
        assert!(err.to_string().contains("999"));
    }

    /// 测试：发私聊白名单越界应报错
    #[test]
    fn send_private_not_subset_of_recv() {
        let toml_str = r#"
[napcat]
ws_url = "ws://127.0.0.1:3001"

[whitelist]
recv_privates = [1000]
send_privates = [1000, 8888]

[storage]
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let err = config.validate_whitelist().unwrap_err();
        assert!(err.to_string().contains("8888"));
    }

    /// 测试：默认存储配置
    #[test]
    fn default_storage_config() {
        let toml_str = r#"
[napcat]
ws_url = "ws://127.0.0.1:3001"

[whitelist]

[storage]
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.storage.workspace_dir, "./workspace");
        assert_eq!(config.storage.max_messages_per_file, 500);
    }

    /// 测试：master 默认值为 "self"
    #[test]
    fn default_master_is_self() {
        let toml_str = r#"
[napcat]
ws_url = "ws://127.0.0.1:3001"

[whitelist]

[storage]
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(config.identity.is_master_self());
        assert_eq!(config.identity.master_display(), "你当前所用账号");
        assert_eq!(config.identity.resolve_master_qq(123456), Some(123456));
    }

    /// 测试：master 设置为指定 QQ 号
    #[test]
    fn master_with_explicit_qq() {
        let toml_str = r#"
[napcat]
ws_url = "ws://127.0.0.1:3001"

[whitelist]

[storage]

[identity]
master = "987654321"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.identity.is_master_self());
        assert_eq!(config.identity.master_display(), "987654321");
        assert_eq!(config.identity.resolve_master_qq(0), Some(987654321));
    }
}
