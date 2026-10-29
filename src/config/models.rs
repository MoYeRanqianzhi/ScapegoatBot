//! `models.json`：provider 与模型参数。
//!
//! 设计依据：设计 §9.3。
//!
//! 关键约束：
//! - 本文件**不含任何密钥**；密钥只在 `auth.json`。
//! - 模型引用形式为 `provider/id`。
//! - `[agent].available_models` 是白名单；本文件里存在但不在白名单的模型不可用。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// `models.json` 根结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelsFile {
    /// provider 名 → 定义。
    pub providers: BTreeMap<String, ProviderDef>,
}

/// 一个 provider（协议 + 地址 + 模型列表）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderDef {
    pub name: String,
    /// 协议：`anthropic` / `openai` / `chat`。
    pub api: Protocol,
    pub base_url: String,
    pub timeout_secs: u64,
    pub max_retries: u32,
    pub models: Vec<ModelDef>,
}

impl Default for ProviderDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            api: Protocol::Anthropic,
            base_url: String::new(),
            timeout_secs: 120,
            max_retries: 2,
            models: Vec::new(),
        }
    }
}

/// 三种协议，对应设计 §10 的 `providers/` 模块。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// Anthropic Messages（默认）。
    #[default]
    Anthropic,
    /// OpenAI Responses。
    Openai,
    /// OpenAI Chat Completions 兼容协议。
    Chat,
}

/// 单个模型定义。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelDef {
    /// API model id，必填。
    pub id: String,
    pub name: String,
    /// 是否支持推理 / effort。
    pub reasoning: bool,
    /// 支持的 effort 级别；空表示不区分。
    pub thinking_levels: Vec<String>,
    pub default_thinking_level: String,
    /// 输入模态：`text` / `image`。
    pub input: Vec<String>,
    pub context_window: u64,
    pub max_tokens: u32,
    /// 能力声明，如 `tools`、`vision`、`server_tools.web_search`。
    pub capabilities: Vec<String>,
    /// 每百万 token 价格（美元）；缺省按 0 计。
    pub cost: CostDef,
    /// 供应商特有的采样参数，原样透传。
    pub sampling: serde_json::Value,
    /// 兼容性开关，原样透传。
    pub compat: serde_json::Value,
}

impl Default for ModelDef {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            reasoning: false,
            thinking_levels: Vec::new(),
            default_thinking_level: String::new(),
            input: vec!["text".to_owned()],
            context_window: 200_000,
            max_tokens: 8_192,
            capabilities: vec!["tools".to_owned()],
            cost: CostDef::default(),
            sampling: serde_json::Value::Null,
            compat: serde_json::Value::Null,
        }
    }
}

impl ModelDef {
    /// 是否声明了某项能力。
    pub fn supports(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|item| item == capability)
    }

    /// 把统一 effort 映射到供应商实际值；未配置映射时同名透传。
    /// Phase 2 的 provider 适配层使用。
    #[allow(dead_code)]
    pub fn map_thinking_level(&self, level: &str) -> String {
        level.to_owned()
    }
}

/// 价格（每百万 token，美元）。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CostDef {
    pub input: f64,
    pub output: f64,
}

/// 一次解析后的模型引用。
///
/// `provider_name` / `provider` 在 Phase 2 组装请求与 Phase 5 计算成本时使用。
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct ModelRef<'a> {
    pub provider_name: &'a str,
    pub provider: &'a ProviderDef,
    pub model: &'a ModelDef,
}

impl ModelsFile {
    /// 从磁盘读取并解析。
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|error| anyhow::anyhow!("cannot read {}: {error}", path.display()))?;
        Self::parse(&text)
            .map_err(|error| anyhow::anyhow!("cannot parse {}: {error}", path.display()))
    }

    /// 解析 JSON 文本。
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// 按 `provider/id` 查找模型。
    pub fn find<'a>(&'a self, reference: &'a str) -> Option<ModelRef<'a>> {
        let (provider_name, model_id) = reference.split_once('/')?;
        let provider = self.providers.get(provider_name)?;
        let model = provider.models.iter().find(|model| model.id == model_id)?;
        Some(ModelRef {
            provider_name,
            provider,
            model,
        })
    }

    /// 把 `overlay` 合并进 `self`：按 provider 名与模型 id 覆盖/追加。
    ///
    /// 设计 §9.3：用户 `~/.sbot/models.json` 按 provider + id 覆盖/追加内置清单。
    pub fn merge(&mut self, overlay: ModelsFile) {
        for (name, provider) in overlay.providers {
            match self.providers.get_mut(&name) {
                None => {
                    self.providers.insert(name, provider);
                }
                Some(existing) => {
                    existing.name = provider.name;
                    existing.api = provider.api;
                    if !provider.base_url.is_empty() {
                        existing.base_url = provider.base_url;
                    }
                    existing.timeout_secs = provider.timeout_secs;
                    existing.max_retries = provider.max_retries;
                    for model in provider.models {
                        match existing.models.iter_mut().find(|item| item.id == model.id) {
                            None => existing.models.push(model),
                            Some(slot) => *slot = model,
                        }
                    }
                }
            }
        }
    }

    /// 校验结构：provider 有地址、模型有 id、同一 provider 内 id 唯一。
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        for (name, provider) in &self.providers {
            if provider.base_url.trim().is_empty() {
                errors.push(format!("providers.{name}.base_url must not be empty"));
            }
            if provider.models.is_empty() {
                errors.push(format!("providers.{name}.models must not be empty"));
            }
            let mut seen = std::collections::HashSet::new();
            for model in &provider.models {
                if model.id.trim().is_empty() {
                    errors.push(format!("providers.{name} contains a model without id"));
                }
                if !seen.insert(model.id.as_str()) {
                    errors.push(format!(
                        "providers.{name} contains duplicate model id {:?}",
                        model.id
                    ));
                }
                if model.max_tokens == 0 {
                    errors.push(format!(
                        "providers.{name}.models[{}].max_tokens must be greater than zero",
                        model.id
                    ));
                }
                if model.context_window == 0 {
                    errors.push(format!(
                        "providers.{name}.models[{}].context_window must be greater than zero",
                        model.id
                    ));
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ModelsFile {
        ModelsFile::parse(
            r#"{
              "providers": {
                "anthropic": {
                  "name": "Anthropic",
                  "api": "anthropic",
                  "base_url": "https://api.anthropic.com",
                  "models": [
                    {"id": "claude-opus-5-5", "name": "Opus", "context_window": 1000000,
                     "max_tokens": 128000, "capabilities": ["tools", "server_tools.web_search"],
                     "thinking_levels": ["low", "high"], "cost": {"input": 4.0, "output": 20.0}}
                  ]
                },
                "local": {
                  "name": "Local",
                  "api": "openai",
                  "base_url": "http://127.0.0.1:43210",
                  "models": [{"id": "test-model", "max_tokens": 8192}]
                }
              }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn finds_model_by_reference() {
        let models = sample();
        let found = models.find("anthropic/claude-opus-5-5").unwrap();
        assert_eq!(found.provider_name, "anthropic");
        assert_eq!(found.model.id, "claude-opus-5-5");
        assert!(found.model.supports("tools"));
        assert!(!found.model.supports("vision"));
        assert!(models.find("anthropic/missing").is_none());
        assert!(models.find("nope/claude-opus-5-5").is_none());
        assert!(models.find("no-slash").is_none());
    }

    #[test]
    fn merge_overrides_and_appends() {
        let mut base = sample();
        let overlay = ModelsFile::parse(
            r#"{
              "providers": {
                "anthropic": {
                  "name": "Anthropic (user)",
                  "api": "anthropic",
                  "base_url": "https://proxy.example.com",
                  "models": [
                    {"id": "claude-opus-5-5", "max_tokens": 64000},
                    {"id": "claude-haiku-5-5", "max_tokens": 16384}
                  ]
                }
              }
            }"#,
        )
        .unwrap();
        base.merge(overlay);

        let anthropic = &base.providers["anthropic"];
        assert_eq!(anthropic.name, "Anthropic (user)");
        assert_eq!(anthropic.base_url, "https://proxy.example.com");
        assert_eq!(anthropic.models.len(), 2);
        let opus = base.find("anthropic/claude-opus-5-5").unwrap();
        assert_eq!(opus.model.max_tokens, 64_000);
        assert!(base.find("anthropic/claude-haiku-5-5").is_some());
        // 未涉及的 provider 保留。
        assert!(base.find("local/test-model").is_some());
    }

    #[test]
    fn validate_reports_empty_provider() {
        let models =
            ModelsFile::parse(r#"{"providers":{"x":{"base_url":"","models":[]}}}"#).unwrap();
        let errors = models.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("base_url")));
        assert!(errors.iter().any(|e| e.contains("models")));
    }
}
