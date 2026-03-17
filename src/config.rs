// src/config.rs
// 配置模块
// 从 TOML 文件加载 NapCat 连接配置、白名单（收/发分离）和消息存储配置
// 启动时校验发白名单是收白名单的子集

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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
    /// Fetch 工具配置（可选，缺省为禁用）
    /// 启用后 Agent 可以获取网页内容（HTML 自动转为 Markdown）
    #[serde(default)]
    pub fetch: FetchConfig,
    /// MCP 服务器配置映射表（可选，缺省为空 HashMap）
    /// 键为服务器名称（用于工具命名 mcp.<name>.<tool>），值为服务器配置
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
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
    /// LLM 提供商类型（默认 "openai"）
    /// - "openai" — OpenAI 兼容 API（ChatCompletions / Responses 端点）
    /// - "anthropic" — Anthropic Messages API（Claude 系列模型）
    #[serde(default = "default_provider")]
    pub provider: String,
    /// API 基础 URL
    /// OpenAI 兼容：如 https://api.openai.com/v1
    /// Anthropic：如 https://api.anthropic.com（留空时使用此默认值）
    #[serde(default)]
    pub base_url: String,
    /// API 密钥
    /// OpenAI 兼容：作为 Authorization: Bearer 头部发送
    /// Anthropic：作为 x-api-key 头部发送（OAuth token 以 sk-ant-oat01- 开头时使用 Bearer）
    #[serde(default)]
    pub api_key: String,
    /// 模型名称（如 gpt-4o、claude-sonnet-4-20250514 等）
    #[serde(default = "default_llm_model")]
    pub model: String,
    /// 最大输出 token 数（可选）
    /// Anthropic API 强制要求此字段，默认 16384
    /// OpenAI 可选，不设置时由模型自行决定
    #[serde(default)]
    pub max_tokens: Option<u32>,
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
    /// HTTP 请求超时时间（秒），默认 300 秒（5 分钟）
    /// 所有 API 请求均使用流式传输，此超时为最后安全网
    /// 如果 LLM 在此时间内未完成响应，请求将被客户端主动中止
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// API 协议类型，决定使用哪个端点（仅 OpenAI 提供商使用）
    /// - "chat_completions"（默认）：使用 /v1/chat/completions 端点，兼容绝大多数渠道
    /// - "responses"：使用 /v1/responses 端点（OpenAI 新一代 API）
    /// 某些渠道仅支持其中一种端点，通过此参数切换
    /// Anthropic 提供商忽略此字段（始终使用 /v1/messages 端点）
    #[serde(default = "default_wire_api")]
    pub wire_api: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            base_url: String::new(),
            api_key: String::new(),
            model: default_llm_model(),
            max_tokens: None,
            system_role_name: None,
            reasoning_effort: None,
            max_steps: default_max_steps(),
            timeout_secs: default_timeout_secs(),
            wire_api: default_wire_api(),
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

    /// 返回 Anthropic 的有效 max_tokens 值
    /// Anthropic API 强制要求此字段，默认 16384
    pub fn effective_anthropic_max_tokens(&self) -> u32 {
        self.max_tokens.unwrap_or(16384)
    }

    /// 检查 LLM 配置是否已就绪
    /// 不同提供商有不同的必填项：
    /// - OpenAI：base_url 和 api_key 均非空
    /// - Anthropic：仅 api_key 非空（base_url 有内置默认值 https://api.anthropic.com）
    pub fn is_configured(&self) -> bool {
        let has_api_key = !self.api_key.trim().is_empty();
        match self.provider.to_ascii_lowercase().as_str() {
            // Anthropic 有内置默认 base_url，仅需 api_key
            "anthropic" => has_api_key,
            // OpenAI 及其他提供商需要 base_url 和 api_key
            _ => !self.base_url.trim().is_empty() && has_api_key,
        }
    }
}

/// 默认 LLM 提供商类型
fn default_provider() -> String {
    "openai".to_string()
}

/// 默认模型名称
fn default_llm_model() -> String {
    "unknown".to_string()
}

/// 默认最大工具调用步数
/// codex 没有硬性上限，但我们保留安全网防止无限循环
/// 128 步足够完成复杂的多文件读取 + 回复 + 记忆更新流程
const fn default_max_steps() -> u32 {
    128
}

/// 默认 HTTP 请求超时时间（秒）
/// 300 秒 = 5 分钟，足够复杂的工具调用请求完成
const fn default_timeout_secs() -> u64 {
    300
}

/// 默认 API 协议类型：chat_completions
fn default_wire_api() -> String {
    "chat_completions".to_string()
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
// Fetch 工具配置
// ============================================================================

/// Fetch 工具配置
/// 控制 Agent 的网络访问能力（获取网页内容并转为 Markdown）
#[derive(Debug, Clone, Deserialize)]
pub struct FetchConfig {
    /// 是否启用 Fetch 工具（默认 false — 需要显式启用以允许 Agent 访问网络）
    #[serde(default)]
    pub enabled: bool,
    /// 域名过滤模式：
    /// - "whitelist"：仅允许访问 domains 列表中的域名
    /// - "blacklist"：禁止访问 domains 列表中的域名（默认）
    #[serde(default = "default_fetch_mode")]
    pub mode: String,
    /// 域名列表（与 mode 配合使用）
    /// 白名单模式：仅这些域名可访问
    /// 黑名单模式：这些域名被禁止
    #[serde(default)]
    pub domains: Vec<String>,
    /// HTTP 请求超时时间（秒，默认 30）
    #[serde(default = "default_fetch_timeout")]
    pub timeout_secs: u64,
    /// 最大响应体大小（字节，默认 524288 = 512 KB）
    #[serde(default = "default_fetch_max_size")]
    pub max_response_bytes: usize,
    /// 最大输出字符数（截断后返回给 LLM，默认 50000）
    #[serde(default = "default_fetch_max_output")]
    pub max_output_chars: usize,
    /// 自定义 User-Agent（可选，默认为 Chrome-like UA）
    #[serde(default)]
    pub user_agent: Option<String>,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            enabled: false,                     // 默认禁用，需要显式启用
            mode: "blacklist".to_string(),      // 默认黑名单模式
            domains: Vec::new(),                // 默认空域名列表
            timeout_secs: 30,                   // 默认 30 秒超时
            max_response_bytes: 524288,         // 默认 512 KB 响应体上限
            max_output_chars: 50000,            // 默认 50000 字符输出上限
            user_agent: None,                   // 默认使用内置 Chrome UA
        }
    }
}

/// 默认域名过滤模式：黑名单
fn default_fetch_mode() -> String {
    "blacklist".to_string()
}

/// 默认 HTTP 请求超时时间：30 秒
fn default_fetch_timeout() -> u64 {
    30
}

/// 默认最大响应体大小：512 KB
fn default_fetch_max_size() -> usize {
    524288
}

/// 默认最大输出字符数：50000
fn default_fetch_max_output() -> usize {
    50000
}

// ============================================================================
// MCP 服务器配置
// ============================================================================

/// MCP 服务器配置
/// 每个 MCP 服务器作为子进程启动，通过 stdin/stdout 进行 JSON-RPC 2.0 通信
/// 配置示例（config.toml）：
/// ```toml
/// [mcp_servers.filesystem]
/// command = "npx"
/// args = ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/dir"]
/// enabled = true
/// tool_timeout_secs = 30
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// 服务器启动命令（如 "npx", "python", "node" 等）
    pub command: String,
    /// 命令行参数列表
    #[serde(default)]
    pub args: Vec<String>,
    /// 额外环境变量（传递给子进程）
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// 是否启用此服务器（默认 true）
    #[serde(default = "default_mcp_enabled")]
    pub enabled: bool,
    /// 工具调用超时时间（秒，默认 30）
    #[serde(default = "default_mcp_tool_timeout")]
    pub tool_timeout_secs: u64,
}

/// MCP 服务器默认启用
fn default_mcp_enabled() -> bool {
    true
}

/// MCP 工具调用默认超时时间：30 秒
fn default_mcp_tool_timeout() -> u64 {
    30
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
