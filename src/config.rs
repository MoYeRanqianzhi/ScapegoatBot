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
}
