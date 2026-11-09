//! provider 装配：模型清单（内置 + 用户覆盖）、预算计量下的主模型 / 回退链，以及周期审查、
//! 记忆复盘、压缩摘要、LLM 分类器各自的模型。

use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::warn;

use crate::config::{AuthFile, LoadedConfig, ModelsFile, SbotConfig};
use crate::paths::SbotPaths;
use crate::providers::metered::UsageSink;
use crate::providers::{Provider, ThinkingOptions, build_metered};
use crate::security::gate::Gate;
use crate::storage::Database;

/// 内置模型清单：随二进制发布，用户 `models.json` 覆盖/追加（设计 §9.3）。
const EMBEDDED_MODELS: &str = include_str!("../../../assets/models.default.json");

/// 主模型、回退链与 thinking 选项；所有 provider 都经同一个预算计量。
pub(super) struct Providers {
    /// 主模型。
    pub(super) provider: Arc<dyn Provider>,
    /// 主模型失败后依次尝试的回退链。
    pub(super) fallbacks: Vec<crate::agent::FallbackProvider>,
    /// 主模型的 thinking / effort。
    pub(super) thinking: ThinkingOptions,
    /// 每日 / 每回合预算；也是所有 provider 的计量端。
    pub(super) budget: Arc<crate::budget::Budget>,
    /// `budget` 作为 [`UsageSink`]，给辅助模型（审查、记忆、压缩）复用。
    pub(super) meter: Arc<dyn UsageSink>,
}

/// 建预算与主模型 / 回退链的 provider；主模型建不出来时返回 `None`（守护进程照常运行，
/// 只是不能回答）。
pub(super) fn build_providers(
    config: &SbotConfig,
    models: &ModelsFile,
    auth: &AuthFile,
    db: &Arc<Database>,
    model_ref: crate::config::models::ModelRef<'_>,
) -> Option<Providers> {
    let guard_cost = match config.security.guard.backend {
        crate::config::GuardBackend::Off => 0.0,
        crate::config::GuardBackend::Jev => crate::budget::JEV_CALL_USD,
        crate::config::GuardBackend::Llm => {
            // 粗略：一次判定约 3k 输入 + 100 输出 token。
            let input = 3_000.0 * model_ref.model.cost.input;
            let output = 100.0 * model_ref.model.cost.output;
            (input + output) / 1_000_000.0
        }
    };
    let budget = crate::budget::Budget::new(Arc::clone(db), &config.budget, guard_cost);
    let meter: Arc<dyn UsageSink> = budget.clone();
    let provider: Arc<dyn Provider> = match build_metered(models, auth, &config.agent.model, &meter)
    {
        Ok(provider) => provider,
        Err(error) => {
            warn!(%error, model = %config.agent.model, "cannot build a provider");
            return None;
        }
    };
    // 回退链：每个模型单独构造 provider（协议/密钥可能不同）。
    let fallbacks: Vec<crate::agent::FallbackProvider> = config
        .agent
        .fallback_models
        .iter()
        .filter_map(|reference| {
            let Some(def) = models.find(reference) else {
                warn!(model = %reference, "fallback model is not defined in models.json");
                return None;
            };
            match build_metered(models, auth, reference, &meter) {
                Ok(provider) => Some(crate::agent::FallbackProvider {
                    model: def.model.id.clone(),
                    provider,
                    max_tokens: def.model.max_tokens.min(config.agent.max_output_tokens),
                }),
                Err(error) => {
                    warn!(%error, model = %reference, "cannot build a fallback provider");
                    None
                }
            }
        })
        .collect();
    let thinking = ThinkingOptions {
        enabled: model_ref.model.reasoning,
        effort: if model_ref.model.reasoning {
            thinking_effort(&config.agent.thinking_level, model_ref.model)
        } else {
            None
        },
    };

    Some(Providers {
        provider,
        fallbacks,
        thinking,
        budget,
        meter,
    })
}

/// `[security.guard].backend = "llm"`：用主 provider 当分类器，装进内容闸。
pub(super) fn install_llm_judge(
    config: &SbotConfig,
    gate: &Arc<Gate>,
    provider: &Arc<dyn Provider>,
    model_ref: crate::config::models::ModelRef<'_>,
) {
    if config.security.guard.enabled
        && config.security.guard.backend == crate::config::GuardBackend::Llm
    {
        let model = if config.security.guard.model.is_empty() {
            model_ref.model.id.clone()
        } else {
            config.security.guard.model.clone()
        };
        let judge = crate::security::gate::LlmJudge::new(
            Arc::clone(provider),
            model,
            std::time::Duration::from_millis(config.security.guard.timeout_ms),
        );
        if gate.install_judge(Arc::new(judge)).is_err() {
            warn!("a guard judge was already installed");
        }
    }
}

/// `[agent].compaction_model` → 压缩摘要用的计量 provider；空或构造失败时返回 `None`
/// （摘要用主模型，失败记警告）。
pub(super) fn compaction_model(
    config: &SbotConfig,
    models: &ModelsFile,
    auth: &AuthFile,
    meter: &Arc<dyn UsageSink>,
) -> Option<Arc<crate::agent::runner::CompactionModel>> {
    if config.agent.compaction_model.trim().is_empty() {
        None
    } else {
        match build_metered(models, auth, config.agent.compaction_model.trim(), meter) {
            Ok(provider) => models
                .find(config.agent.compaction_model.trim())
                .map(|found| {
                    Arc::new(crate::agent::runner::CompactionModel {
                        provider,
                        model: found.model.id.clone(),
                    })
                }),
            Err(error) => {
                warn!(%error, model = %config.agent.compaction_model, "cannot build the compaction provider");
                None
            }
        }
    }
}

/// 按 `[security]` 构造周期检查器；`review_model` 为空时用主模型。
pub(super) fn security_reviewer(
    security: &crate::config::SecurityConfig,
    models: &ModelsFile,
    auth: &AuthFile,
    provider: &Arc<dyn Provider>,
    model_ref: crate::config::models::ModelRef<'_>,
    meter: &Arc<dyn UsageSink>,
) -> Option<Arc<dyn crate::security::review::SecurityCheck>> {
    if security.check_every_tokens == 0 && !security.check_on_compaction {
        return None;
    }
    let (reviewer, model) = if security.review_model.trim().is_empty() {
        (Arc::clone(provider), model_ref.model.id.clone())
    } else {
        let reference = security.review_model.trim();
        let Some(found) = models.find(reference) else {
            warn!(model = %reference, "security.review_model is not defined in models.json");
            return None;
        };
        match build_metered(models, auth, reference, meter) {
            Ok(provider) => (provider, found.model.id.clone()),
            Err(error) => {
                warn!(%error, model = %reference, "cannot build the security review provider");
                return None;
            }
        }
    };
    Some(Arc::new(crate::security::review::SecurityReviewer::new(
        reviewer,
        model,
        std::time::Duration::from_secs(security.check_timeout_secs.max(1)),
    )))
}

/// `[memory].review_model` → 独立的计量 provider；不设或构造失败时返回 `None`
/// （复盘与压缩前记忆提取跟随主模型，构造失败记警告）。
pub(super) fn memory_review_model(
    memory: &crate::config::MemoryConfig,
    models: &ModelsFile,
    auth: &AuthFile,
    meter: &Arc<dyn UsageSink>,
) -> Option<crate::memory::ReviewModel> {
    let reference = memory.review_model.trim();
    if reference.is_empty() {
        return None;
    }
    // 存在性已由 `validate_with_models` 保证；这里仍防御一次。
    let found = models.find(reference)?;
    match build_metered(models, auth, reference, meter) {
        Ok(provider) => Some(crate::memory::ReviewModel {
            provider,
            model: found.model.id.clone(),
        }),
        Err(error) => {
            warn!(%error, model = %reference, "cannot build the memory review provider; using the main model");
            None
        }
    }
}

/// 显式配置优先；否则用模型声明的默认 effort；都没有则关闭 thinking。
fn thinking_effort(configured: &str, model: &crate::config::models::ModelDef) -> Option<String> {
    if !configured.is_empty() {
        Some(configured.to_owned())
    } else if !model.default_thinking_level.is_empty() {
        Some(model.default_thinking_level.clone())
    } else {
        None
    }
}

/// 读取模型清单：内置清单打底，用户 `models.json` 按 provider + id 覆盖 / 追加，再整体校验。
pub(in crate::app) fn load_models(paths: &SbotPaths, loaded: &LoadedConfig) -> Result<ModelsFile> {
    let mut models =
        ModelsFile::parse(EMBEDDED_MODELS).context("embedded models.default.json is invalid")?;
    let user_path = paths.resolve(&loaded.config.models_file, &loaded.dir);
    if user_path.exists() {
        models.merge(ModelsFile::load(&user_path)?);
    }
    if let Err(errors) = models.validate() {
        anyhow::bail!(
            "models.json validation failed:\n  - {}",
            errors.join("\n  - ")
        );
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_effort_prefers_explicit_then_model_default() {
        let mut model = crate::config::models::ModelDef {
            reasoning: true,
            ..crate::config::models::ModelDef::default()
        };
        model.default_thinking_level = "high".to_owned();
        assert_eq!(thinking_effort("", &model), Some("high".to_owned()));
        assert_eq!(thinking_effort("low", &model), Some("low".to_owned()));
        model.default_thinking_level.clear();
        assert_eq!(thinking_effort("", &model), None);
    }
}
