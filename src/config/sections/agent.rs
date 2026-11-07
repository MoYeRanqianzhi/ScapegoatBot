use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::{ConfigError, ModelsFile, check_available, non_empty, non_zero, validate_scopes};

// ---------------------------------------------------------------------------
// [agent]
// ---------------------------------------------------------------------------

/// 代理运行模式：替身或伙伴。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    /// 使用主人自己的 QQ，替主人说话。
    #[default]
    StandIn,
    /// 使用另一个账号，作为有独立人格的伙伴。
    Companion,
}

/// Provider 请求失败后的重试间隔策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RetryMode {
    /// 阶梯退避：base、2×base、4×base……直到 max。
    #[default]
    Stepped,
    /// 固定间隔：每次都用 base。
    Fixed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    pub mode: AgentMode,
    /// companion 必填；stand_in 由登录账号决定。
    pub owner_qq: u64,
    pub owner_name: String,
    pub available_models: Vec<String>,
    pub model: String,
    pub thinking_level: String,
    pub compaction_model: String,
    pub max_output_tokens: u32,
    pub fallback_models: Vec<String>,
    pub retry_attempts: u32,
    pub retry_base_ms: u64,
    pub retry_max_ms: u64,
    /// `stepped`（默认，阶梯退避）或 `fixed`（固定间隔）。
    pub retry_mode: RetryMode,
    /// 主人的私线：`owner`（默认，等于 `owner_qq`）、`self`（本账号）或 QQ 号。
    ///
    /// 报告与审批通知发到这里；另一个账号时，它发来的私聊也算主人在说话
    /// （见 `crate::principals`）。
    pub master: String,
    pub receive_scopes: Vec<String>,
    pub send_scopes: Vec<String>,
    pub max_steps: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: "ScapegoatBot".to_owned(),
            mode: AgentMode::StandIn,
            owner_qq: 0,
            owner_name: String::new(),
            available_models: vec![
                "anthropic/claude-opus-5-5".to_owned(),
                "anthropic/claude-haiku-5-5".to_owned(),
                "deepseek/deepseek-chat".to_owned(),
            ],
            model: "anthropic/claude-opus-5-5".to_owned(),
            thinking_level: "high".to_owned(),
            // 空 = 用主模型；默认值不能把用户没列进白名单的模型塞进来。
            compaction_model: String::new(),
            max_output_tokens: 8192,
            fallback_models: Vec::new(),
            retry_attempts: 3,
            retry_base_ms: 400,
            retry_max_ms: 30_000,
            retry_mode: RetryMode::Stepped,
            master: "owner".to_owned(),
            receive_scopes: vec!["*".to_owned()],
            send_scopes: vec!["*".to_owned()],
            max_steps: 64,
        }
    }
}

impl AgentConfig {
    pub(in crate::config) fn validate(&self, errors: &mut Vec<ConfigError>) {
        non_empty(errors, "agent.name", &self.name);
        if self.owner_name.chars().count() > 64 {
            errors.push(ConfigError::new(
                "agent.owner_name",
                "must be at most 64 characters",
            ));
        }
        if !matches!(self.master.as_str(), "owner" | "self")
            && self.master.parse::<u64>().map_or(true, |qq| qq == 0)
        {
            errors.push(ConfigError::new(
                "agent.master",
                "must be \"owner\", \"self\" or a non-zero QQ number",
            ));
        }
        if self.available_models.is_empty() {
            errors.push(ConfigError::new(
                "agent.available_models",
                "must list at least one model",
            ));
        }
        let mut seen = HashSet::new();
        for model in &self.available_models {
            if model.trim().is_empty() || !model.contains('/') {
                errors.push(ConfigError::new(
                    "agent.available_models",
                    format!("{model:?} must be in the form provider/model"),
                ));
            }
            if !seen.insert(model.as_str()) {
                errors.push(ConfigError::new(
                    "agent.available_models",
                    format!("{model:?} is listed more than once"),
                ));
            }
        }
        if !self.available_models.contains(&self.model) {
            errors.push(ConfigError::new(
                "agent.model",
                format!("{:?} is not in agent.available_models", self.model),
            ));
        }
        // 空 compaction_model = 用主模型；默认值必须是合法的。
        if !self.compaction_model.is_empty()
            && !self.available_models.contains(&self.compaction_model)
        {
            errors.push(ConfigError::new(
                "agent.compaction_model",
                format!(
                    "{:?} is not in agent.available_models",
                    self.compaction_model
                ),
            ));
        }
        for model in &self.fallback_models {
            if !self.available_models.contains(model) {
                errors.push(ConfigError::new(
                    "agent.fallback_models",
                    format!("{model:?} is not in agent.available_models"),
                ));
            }
            if model == &self.model {
                errors.push(ConfigError::new(
                    "agent.fallback_models",
                    "must not contain the primary model",
                ));
            }
        }
        non_zero(
            errors,
            "agent.retry_attempts",
            u64::from(self.retry_attempts),
        );
        non_zero(
            errors,
            "agent.retry_base_ms / agent.retry_max_ms",
            self.retry_base_ms.min(self.retry_max_ms),
        );
        non_zero(errors, "agent.max_steps", u64::from(self.max_steps));
        non_zero(
            errors,
            "agent.max_output_tokens",
            u64::from(self.max_output_tokens),
        );
        validate_scopes("agent.receive_scopes", &self.receive_scopes, errors);
        validate_scopes("agent.send_scopes", &self.send_scopes, errors);
    }

    /// 需要模型清单的校验。
    pub(in crate::config) fn validate_with_models(
        &self,
        models: &ModelsFile,
        errors: &mut Vec<ConfigError>,
    ) {
        let Some(model) = models.find(&self.model) else {
            errors.push(ConfigError::new(
                "agent.model",
                format!("{:?} is not defined in models.json", self.model),
            ));
            return;
        };

        if !model.model.supports("tools") {
            // 没有工具能力的模型无法驱动 agent；显式拒绝，避免运行时才暴露。
            errors.push(ConfigError::new(
                "agent.model",
                format!("{:?} does not declare the \"tools\" capability", self.model),
            ));
        }

        if !model.model.thinking_levels.is_empty()
            && !model.model.thinking_levels.contains(&self.thinking_level)
        {
            errors.push(ConfigError::new(
                "agent.thinking_level",
                format!(
                    "{:?} is not supported by {:?}; supported: {:?}",
                    self.thinking_level, self.model, model.model.thinking_levels
                ),
            ));
        }

        // 空 compaction_model 表示“用主模型”，不参与白名单校验。
        if !self.compaction_model.is_empty() {
            check_available(
                models,
                &self.available_models,
                &self.compaction_model,
                "agent.compaction_model",
                errors,
            );
        }
        for name in &self.fallback_models {
            check_available(
                models,
                &self.available_models,
                name,
                "agent.fallback_models",
                errors,
            );
        }

        if self.max_output_tokens > model.model.max_tokens {
            errors.push(ConfigError::new(
                "agent.max_output_tokens",
                format!(
                    "{} exceeds the model's max_tokens ({})",
                    self.max_output_tokens, model.model.max_tokens
                ),
            ));
        }
    }
}
