// src/config.rs
// 配置模块
// 从 config.toml 加载主配置 + 从 llm.toml 加载 LLM 模型配置
// 所有 LLM 模型（含主代理模型）统一定义在 llm.toml 中
// config.toml 的 [llm] 段仅包含文件路径和主模型名称引用

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
    /// LLM 配置引用（指向 llm.toml 文件路径 + 主模型名称）
    #[serde(default)]
    pub llm: LlmRef,
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
    /// 配置格式: [mcp.<name>]，键为服务器名称（用于工具命名 mcp.<name>.<tool>）
    #[serde(default)]
    pub mcp: HashMap<String, McpServerConfig>,
    /// LLM 模型列表（从 llm.toml 合并 defaults + raw 后加载，不直接从 config.toml 反序列化）
    #[serde(skip)]
    pub models: Vec<ModelConfig>,
    /// 蜂群子代理配置（可选，缺省为默认值）
    /// 控制子代理的并发上限、嵌套深度等安全参数
    #[serde(default)]
    pub swarm: SwarmConfig,
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
// LLM 配置引用（config.toml [llm] 段）
// ============================================================================

/// config.toml 中的 [llm] 段
/// 仅包含 llm.toml 文件路径和主代理使用的模型名称
/// 所有模型定义统一在 llm.toml 中
#[derive(Debug, Clone, Deserialize)]
pub struct LlmRef {
    /// LLM 配置文件路径（相对于 config.toml 所在目录）
    #[serde(default = "default_llm_config_file")]
    pub config_file: String,
    /// 主代理使用的模型名称（必须对应 llm.toml 中某个 [[models]] 的 name）
    #[serde(default = "default_primary_model")]
    pub primary_model: String,
}

impl Default for LlmRef {
    fn default() -> Self {
        Self {
            config_file: default_llm_config_file(),
            primary_model: default_primary_model(),
        }
    }
}

/// 默认 LLM 配置文件名
fn default_llm_config_file() -> String {
    "llm.toml".to_string()
}

/// 默认主模型名称
fn default_primary_model() -> String {
    "main".to_string()
}

// ============================================================================
// LLM 配置文件结构（llm.toml）
// ============================================================================

/// llm.toml 的完整结构
/// 包含全局默认值和模型定义列表
#[derive(Debug, Clone, Deserialize)]
pub struct LlmFileConfig {
    /// 默认值段（所有模型可继承，减少重复配置）
    #[serde(default)]
    pub defaults: ModelDefaults,
    /// 模型定义列表
    pub models: Vec<ModelConfigRaw>,
}

/// 模型默认值（llm.toml [defaults] 段）
/// 所有字段可选——模型定义中未指定的字段优先使用此处的值，
/// 此处也未指定时使用硬编码默认值
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelDefaults {
    /// LLM 提供商类型（默认 "openai"）
    pub provider: Option<String>,
    /// API 基础 URL
    pub base_url: Option<String>,
    /// API 密钥
    pub api_key: Option<String>,
    /// HTTP 请求超时时间（秒）
    pub timeout_secs: Option<u64>,
    /// API 协议类型（仅 OpenAI 提供商使用）
    pub wire_api: Option<String>,
    /// 系统指令的角色名称
    pub system_role_name: Option<String>,
    /// 最大输出 token 数
    pub max_tokens: Option<u32>,
    /// 每个任务的最大工具调用步数
    pub max_steps: Option<u32>,
    /// 上下文窗口大小（token 数）
    pub context_window: Option<usize>,
}

/// llm.toml 中的原始模型定义
/// name, display_name, description, model 为必填字段
/// 其余可选字段未指定时依次从 defaults → 硬编码默认值 取值
#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfigRaw {
    /// 内部引用名（SpawnAgent 工具和 primary_model 使用此名称）
    pub name: String,
    /// 人类可读的显示名称（如 "GPT-4o"、"Claude Opus 4.6"）
    pub display_name: String,
    /// 一句话性能描述（帮助 LLM 选择合适的模型）
    pub description: String,
    /// 模型 ID（如 "gpt-4o"、"claude-opus-4-6-20250514"）
    pub model: String,
    /// LLM 提供商类型
    pub provider: Option<String>,
    /// API 基础 URL
    pub base_url: Option<String>,
    /// API 密钥
    pub api_key: Option<String>,
    /// 最大输出 token 数
    pub max_tokens: Option<u32>,
    /// 上下文窗口大小（token 数）
    pub context_window: Option<usize>,
    /// HTTP 请求超时时间（秒）
    pub timeout_secs: Option<u64>,
    /// API 协议类型
    pub wire_api: Option<String>,
    /// 系统指令的角色名称
    pub system_role_name: Option<String>,
    /// 每个任务的最大工具调用步数
    pub max_steps: Option<u32>,
    /// 推理深度/努力程度（可选，传递给支持该参数的模型）
    pub reasoning_effort: Option<String>,
}

impl LlmFileConfig {
    /// 将 defaults + 各 ModelConfigRaw 合并，生成最终的 ModelConfig 列表
    /// 优先级：raw 字段 > defaults 字段 > 硬编码默认值
    pub fn resolve_models(&self) -> Result<Vec<ModelConfig>> {
        let mut result = Vec::with_capacity(self.models.len());
        for raw in &self.models {
            // 合并逻辑：优先 raw，其次 defaults，最后硬编码
            let provider = raw.provider.clone()
                .or_else(|| self.defaults.provider.clone())
                .unwrap_or_else(|| "openai".to_string());
            let base_url = raw.base_url.clone()
                .or_else(|| self.defaults.base_url.clone())
                .unwrap_or_default();
            let api_key = raw.api_key.clone()
                .or_else(|| self.defaults.api_key.clone())
                .unwrap_or_default();
            let max_tokens = raw.max_tokens
                .or(self.defaults.max_tokens);
            let context_window = raw.context_window
                .or(self.defaults.context_window)
                .unwrap_or(128_000);
            let timeout_secs = raw.timeout_secs
                .or(self.defaults.timeout_secs)
                .unwrap_or(120);
            let wire_api = raw.wire_api.clone()
                .or_else(|| self.defaults.wire_api.clone())
                .unwrap_or_else(|| "chat_completions".to_string());
            let system_role_name = raw.system_role_name.clone()
                .or_else(|| self.defaults.system_role_name.clone());
            let max_steps = raw.max_steps
                .or(self.defaults.max_steps)
                .unwrap_or(64);
            let reasoning_effort = raw.reasoning_effort.clone();

            result.push(ModelConfig {
                name: raw.name.clone(),
                display_name: raw.display_name.clone(),
                description: raw.description.clone(),
                provider,
                base_url,
                api_key,
                model: raw.model.clone(),
                max_tokens,
                context_window,
                timeout_secs,
                wire_api,
                system_role_name,
                max_steps,
                reasoning_effort,
            });
        }
        Ok(result)
    }
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
/// [mcp.filesystem]
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
// 已解析的模型配置（从 llm.toml 合并后产出）
// ============================================================================

/// 已解析的 LLM 模型配置
/// 由 LlmFileConfig::resolve_models() 从 ModelConfigRaw + ModelDefaults 合并而来
/// 所有字段均为必填（已完成合并，不再有 Option fallback）
///
/// 主代理和子代理均使用此结构，地位平等
/// config.toml 的 [llm].primary_model 指定主代理使用哪个模型
#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    /// 内部引用名（SpawnAgent 工具的 model 参数和 primary_model 使用此名称）
    /// 必须在所有模型中唯一
    pub name: String,
    /// 人类可读的显示名称（如 "GPT-4o"、"Claude Opus 4.6"）
    /// 用于系统提示中的模型列表展示和日志输出
    pub display_name: String,
    /// 一句话性能描述（如 "最强大的模型，可解决复杂问题"）
    /// 帮助 LLM 选择合适的模型用于子代理任务
    pub description: String,
    /// LLM 提供商类型: "openai" | "anthropic"
    pub provider: String,
    /// API 基础 URL（已解析，不再为 Option）
    /// OpenAI 兼容：如 "https://api.openai.com/v1"
    /// Anthropic：留空时 Provider 内部使用默认 "https://api.anthropic.com"
    pub base_url: String,
    /// API 密钥（已解析，不再为 Option）
    pub api_key: String,
    /// 模型 ID（如 "gpt-4o"、"claude-opus-4-6-20250514"）
    pub model: String,
    /// 最大输出 token 数（可选）
    /// Anthropic API 强制要求此字段
    /// 为 None 时 Anthropic 使用默认 16384，OpenAI 由模型自行决定
    pub max_tokens: Option<u32>,
    /// 该模型的上下文窗口大小（token 数）
    /// 影响上下文压缩触发阈值（context_window * 3/4）
    pub context_window: usize,
    /// HTTP 请求超时时间（秒）
    pub timeout_secs: u64,
    /// API 协议类型（仅 OpenAI 提供商使用）
    /// "chat_completions" | "responses"
    pub wire_api: String,
    /// 系统指令的角色名称（可选，缺省 "system"）
    pub system_role_name: Option<String>,
    /// 每个任务的最大工具调用步数
    pub max_steps: u32,
    /// 推理深度/努力程度（可选，传递给支持该参数的模型）
    /// 常见值：none, minimal, low, medium, high, xhigh
    pub reasoning_effort: Option<String>,
}

impl ModelConfig {
    /// 检查此模型配置是否已就绪（api_key 非空，OpenAI 还需 base_url 非空）
    /// 与原 LlmConfig::is_configured() 逻辑一致
    pub fn is_configured(&self) -> bool {
        let has_api_key = !self.api_key.trim().is_empty();
        match self.provider.to_ascii_lowercase().as_str() {
            // Anthropic 有内置默认 base_url，仅需 api_key
            "anthropic" => has_api_key,
            // OpenAI 及其他提供商需要 base_url 和 api_key
            _ => !self.base_url.trim().is_empty() && has_api_key,
        }
    }

    /// 返回有效的系统角色名称（缺省 "system"）
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

    /// 返回 Anthropic 的有效 max_tokens 值（默认 16384）
    pub fn effective_anthropic_max_tokens(&self) -> u32 {
        self.max_tokens.unwrap_or(16384)
    }
}

// ============================================================================
// 蜂群子代理配置
// ============================================================================

/// 蜂群子代理配置
/// 控制子代理系统的安全参数和资源上限
/// 配置示例（config.toml）：
/// ```toml
/// [swarm]
/// enabled = true
/// max_depth = 32
/// max_concurrency = 1024
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct SwarmConfig {
    /// 是否启用蜂群子代理功能（默认 true）
    /// 禁用时 SpawnAgent 等工具不会注册
    #[serde(default = "default_swarm_enabled")]
    pub enabled: bool,
    /// 最大嵌套深度（默认 32）
    /// 子代理可以递归 spawn 子代理的最深层数
    /// 配合 max_concurrency 形成双重安全网
    #[serde(default = "default_swarm_max_depth")]
    pub max_depth: u32,
    /// 最大并发子代理数（默认 1024）
    /// 所有层级的子代理总数上限，超过时 spawn 被拒绝
    /// tokio 绿色线程极其轻量，1024 并发仅占用约 50MB 内存
    /// 此参数是防止失控扇出的主要安全机制
    #[serde(default = "default_swarm_max_concurrency")]
    pub max_concurrency: u32,
}

/// 蜂群默认启用
fn default_swarm_enabled() -> bool {
    true
}

/// 默认最大嵌套深度：32
fn default_swarm_max_depth() -> u32 {
    32
}

/// 默认最大并发子代理数：1024
fn default_swarm_max_concurrency() -> u32 {
    1024
}

impl Default for SwarmConfig {
    fn default() -> Self {
        Self {
            enabled: default_swarm_enabled(),
            max_depth: default_swarm_max_depth(),
            max_concurrency: default_swarm_max_concurrency(),
        }
    }
}

// ============================================================================
// 配置加载与校验
// ============================================================================

impl AppConfig {
    /// 从 TOML 文件加载配置
    /// 1. 解析 config.toml → AppConfig（models 为空，由 serde(skip) 跳过）
    /// 2. 根据 [llm].config_file 加载 llm.toml → LlmFileConfig
    /// 3. 合并 defaults + raw → Vec<ModelConfig>，写入 config.models
    /// 4. 校验 primary_model 存在、模型名称唯一、白名单子集约束
    pub fn load(path: &Path) -> Result<Self> {
        // 读取配置文件内容
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("无法读取配置文件: {}", path.display()))?;

        // 解析 config.toml（models 字段被 #[serde(skip)] 跳过，初始为空 Vec）
        let mut config: AppConfig = toml::from_str(&content)
            .with_context(|| format!("解析配置文件失败: {}", path.display()))?;

        // 解析 llm.toml（路径相对于 config.toml 所在目录）
        let config_dir = path.parent().unwrap_or(Path::new("."));
        let llm_path = config_dir.join(&config.llm.config_file);
        let llm_content = std::fs::read_to_string(&llm_path)
            .with_context(|| format!("无法读取 LLM 配置文件: {}", llm_path.display()))?;
        let llm_file: LlmFileConfig = toml::from_str(&llm_content)
            .with_context(|| format!("解析 LLM 配置文件失败: {}", llm_path.display()))?;

        // 合并 defaults + raw → Vec<ModelConfig>
        config.models = llm_file.resolve_models()
            .with_context(|| "合并 LLM 模型配置失败")?;

        // 校验 primary_model 存在
        if config.find_model(&config.llm.primary_model).is_none() {
            bail!(
                "主模型 '{}' 未在 {} 中定义",
                config.llm.primary_model,
                llm_path.display()
            );
        }

        // 校验模型名称唯一
        let mut seen_names = HashSet::new();
        for m in &config.models {
            if !seen_names.insert(&m.name) {
                bail!(
                    "模型名称 '{}' 在 {} 中重复定义",
                    m.name,
                    llm_path.display()
                );
            }
        }

        // 校验白名单约束
        config.validate_whitelist()?;

        Ok(config)
    }

    /// 获取主代理模型配置
    /// primary_model 在 load() 中已校验存在，此处使用 expect 安全解包
    pub fn primary_model(&self) -> &ModelConfig {
        self.find_model(&self.llm.primary_model)
            .expect("primary_model 在 load() 中已校验存在")
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

    /// 按名称查找模型配置
    /// 返回 None 表示未找到该名称的模型
    pub fn find_model(&self, name: &str) -> Option<&ModelConfig> {
        self.models.iter().find(|m| m.name == name)
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

    /// 测试：LlmFileConfig::resolve_models() 合并逻辑
    /// raw 优先于 defaults，defaults 优先于硬编码默认值
    #[test]
    fn resolve_models_merge_priority() {
        let llm_toml = r#"
[defaults]
provider = "anthropic"
api_key = "default-key"
timeout_secs = 300
context_window = 200000

[[models]]
name = "main"
display_name = "Test Model"
description = "测试模型"
model = "test-model"
api_key = "override-key"
"#;
        let llm_file: LlmFileConfig = toml::from_str(llm_toml).unwrap();
        let models = llm_file.resolve_models().unwrap();
        assert_eq!(models.len(), 1);
        let m = &models[0];
        // raw.api_key 覆盖 defaults.api_key
        assert_eq!(m.api_key, "override-key");
        // raw.provider 为 None → 取 defaults.provider
        assert_eq!(m.provider, "anthropic");
        // raw.timeout_secs 为 None → 取 defaults.timeout_secs
        assert_eq!(m.timeout_secs, 300);
        // raw.context_window 为 None → 取 defaults.context_window
        assert_eq!(m.context_window, 200000);
        // raw.wire_api 和 defaults.wire_api 都为 None → 硬编码默认 "chat_completions"
        assert_eq!(m.wire_api, "chat_completions");
        // raw.max_steps 和 defaults.max_steps 都为 None → 硬编码默认 64
        assert_eq!(m.max_steps, 64);
        // raw.base_url 和 defaults.base_url 都为 None → 空字符串
        assert_eq!(m.base_url, "");
    }

    /// 测试：ModelConfig::is_configured() 各提供商逻辑
    #[test]
    fn model_config_is_configured() {
        // OpenAI：需要 base_url 和 api_key
        let openai = ModelConfig {
            name: "test".into(), display_name: "T".into(), description: "d".into(),
            provider: "openai".into(), base_url: "https://api.openai.com/v1".into(),
            api_key: "sk-xxx".into(), model: "gpt-4o".into(), max_tokens: None,
            context_window: 128000, timeout_secs: 120, wire_api: "chat_completions".into(),
            system_role_name: None, max_steps: 64, reasoning_effort: None,
        };
        assert!(openai.is_configured());

        // OpenAI：缺 api_key
        let no_key = ModelConfig { api_key: "".into(), ..openai.clone() };
        assert!(!no_key.is_configured());

        // OpenAI：缺 base_url
        let no_url = ModelConfig { base_url: "".into(), ..openai.clone() };
        assert!(!no_url.is_configured());

        // Anthropic：仅需 api_key
        let anthropic = ModelConfig {
            provider: "anthropic".into(), base_url: "".into(),
            api_key: "sk-ant-xxx".into(), ..openai.clone()
        };
        assert!(anthropic.is_configured());

        // Anthropic：缺 api_key
        let anthropic_no_key = ModelConfig { api_key: "".into(), ..anthropic.clone() };
        assert!(!anthropic_no_key.is_configured());
    }

    /// 测试：AppConfig::load() 完整流程（临时文件）
    #[test]
    fn load_with_llm_toml() {
        // 创建临时目录
        let dir = std::env::temp_dir().join("sbot_test_load_llm");
        let _ = std::fs::create_dir_all(&dir);

        // 写入 config.toml
        let config_toml = r#"
[napcat]
ws_url = "ws://127.0.0.1:3001"

[whitelist]

[storage]

[llm]
config_file = "llm.toml"
primary_model = "main"
"#;
        std::fs::write(dir.join("config.toml"), config_toml).unwrap();

        // 写入 llm.toml
        let llm_toml = r#"
[defaults]
provider = "openai"

[[models]]
name = "main"
display_name = "GPT-4o"
description = "主力模型"
model = "gpt-4o"
base_url = "https://api.openai.com/v1"
api_key = "sk-test"
"#;
        std::fs::write(dir.join("llm.toml"), llm_toml).unwrap();

        // 加载并验证
        let config = AppConfig::load(&dir.join("config.toml")).unwrap();
        assert_eq!(config.models.len(), 1);
        let pm = config.primary_model();
        assert_eq!(pm.name, "main");
        assert_eq!(pm.model, "gpt-4o");
        assert_eq!(pm.provider, "openai");
        assert!(pm.is_configured());

        // 清理
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 测试：primary_model 不存在时 load() 应报错
    #[test]
    fn load_missing_primary_model_fails() {
        let dir = std::env::temp_dir().join("sbot_test_missing_primary");
        let _ = std::fs::create_dir_all(&dir);

        let config_toml = r#"
[napcat]
ws_url = "ws://127.0.0.1:3001"
[whitelist]
[storage]
[llm]
primary_model = "nonexistent"
"#;
        std::fs::write(dir.join("config.toml"), config_toml).unwrap();

        let llm_toml = r#"
[[models]]
name = "main"
display_name = "M"
description = "d"
model = "m"
"#;
        std::fs::write(dir.join("llm.toml"), llm_toml).unwrap();

        let err = AppConfig::load(&dir.join("config.toml")).unwrap_err();
        assert!(err.to_string().contains("nonexistent"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 测试：模型名称重复时 load() 应报错
    #[test]
    fn load_duplicate_model_names_fails() {
        let dir = std::env::temp_dir().join("sbot_test_dup_names");
        let _ = std::fs::create_dir_all(&dir);

        let config_toml = r#"
[napcat]
ws_url = "ws://127.0.0.1:3001"
[whitelist]
[storage]
[llm]
primary_model = "main"
"#;
        std::fs::write(dir.join("config.toml"), config_toml).unwrap();

        let llm_toml = r#"
[[models]]
name = "main"
display_name = "M1"
description = "d"
model = "m1"

[[models]]
name = "main"
display_name = "M2"
description = "d"
model = "m2"
"#;
        std::fs::write(dir.join("llm.toml"), llm_toml).unwrap();

        let err = AppConfig::load(&dir.join("config.toml")).unwrap_err();
        assert!(err.to_string().contains("重复"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
