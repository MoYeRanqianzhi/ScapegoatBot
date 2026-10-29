//! `auth.json`：所有凭据的唯一存放处。
//!
//! 设计依据：设计 §9.4。
//!
//! 三条约束：
//! - **明文优先**：值是字面量，直接写 `"sk-..."`；`"$ENV"` 只是可选插值。
//! - **不进 VFS**：模型无法读取本文件。
//! - **零泄漏**：`Debug` 已脱敏（见 [`Secret`]）；日志与 `status` 只显示掩码。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::secret::Secret;

/// `auth.json` 根结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthFile {
    /// provider 名 → 凭据。
    pub providers: BTreeMap<String, ProviderAuth>,
    pub napcat: NapcatAuth,
    pub tools: ToolsAuth,
    pub security: SecurityAuth,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderAuth {
    pub api_key: Secret,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NapcatAuth {
    /// NapCat WebSocket 的 access token（可选）。
    pub access_token: Secret,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsAuth {
    pub web_search: ProviderAuth,
    pub web_fetch: ProviderAuth,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityAuth {
    pub guard: ProviderAuth,
}

impl AuthFile {
    /// 读取 `auth.json`；文件不存在时返回空凭据（允许全部来自环境变量）。
    pub fn load_optional(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text)
                .map_err(|error| anyhow::anyhow!("cannot parse {}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(anyhow::anyhow!("cannot read {}: {error}", path.display())),
        }
    }

    /// 解析 JSON 文本。
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// 取某个 provider 的 key。
    pub fn provider_key(&self, provider: &str) -> Option<Secret> {
        self.providers
            .get(provider)
            .map(|entry| entry.api_key.clone())
            .filter(Secret::is_present)
    }

    /// 安全分类器的 key（`security.guard.api_key`）；Phase 5 的输入/输出闸使用。
    #[allow(dead_code)]
    pub fn guard_key(&self) -> Option<Secret> {
        Some(self.security.guard.api_key.clone()).filter(Secret::is_present)
    }

    /// 启动时列出缺失凭据，便于一次性提示用户。
    ///
    /// 只对 `available_models` 引用到的 provider 检查（设计 §9.4）。
    pub fn missing_providers(&self, referenced: impl IntoIterator<Item = String>) -> Vec<String> {
        let mut missing: Vec<String> = referenced
            .into_iter()
            .filter(|name| self.provider_key(name).is_none())
            .collect();
        missing.sort();
        missing.dedup();
        missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plaintext_and_env_secret() {
        let auth = AuthFile::parse(
            r#"{
              "providers": {"anthropic": {"api_key": "sk-plain"}},
              "security": {"guard": {"api_key": "$SBOT_TEST_GUARD"}}
            }"#,
        )
        .unwrap();
        assert_eq!(
            auth.provider_key("anthropic").unwrap().value().as_deref(),
            Some("sk-plain")
        );
        // 未设置环境变量时视为未配置，而不是空字符串。
        assert!(auth.guard_key().is_none());
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let auth = AuthFile::load_optional(dir.path().join("absent.json")).unwrap();
        assert!(auth.providers.is_empty());
        if auth.guard_key().is_some() {
            panic!("guard key should be absent");
        }
    }

    #[test]
    fn reports_missing_providers_once() {
        let auth = AuthFile::parse(r#"{"providers": {"a": {"api_key": "x"}}}"#).unwrap();
        let missing = auth.missing_providers(["a".to_owned(), "b".to_owned(), "b".to_owned()]);
        assert_eq!(missing, vec!["b".to_owned()]);
    }

    #[test]
    fn debug_output_is_redacted() {
        let auth =
            AuthFile::parse(r#"{"providers": {"a": {"api_key": "sk-secret-value"}}}"#).unwrap();
        let rendered = format!("{auth:?}");
        assert!(!rendered.contains("sk-secret-value"));
    }
}
