//! provider 与 agent 共享的 LLM 类型。

use serde::{Deserialize, Serialize};

/// 停止原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    #[default]
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    /// provider 安全分类器拒绝；不重试、不换模型。
    Refusal,
    /// 服务端工具暂停，需要继续请求。
    PauseTurn,
    Error,
    Aborted,
}

/// 用量：一次请求，或一个回合的累计。
///
/// 统一口径（各协议解析时换算成这一口径）：
/// - `input_tokens`：既没命中缓存、也不是本次写入缓存的输入；
/// - `cache_read_tokens`：命中缓存的输入；
/// - `cache_write_tokens`：本次写入缓存的输入（只有 Anthropic 单列，其余为 0）；
/// - `output_tokens`：输出（含思考）。
///
/// 三项输入之和才是这次请求的完整提示（[`Usage::prompt_tokens`]）。Anthropic 原生就是
/// 这个口径；OpenAI 的 `input_tokens` / `prompt_tokens` 包含命中缓存的部分，解析时减掉。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl Usage {
    /// 累加（一次回合会有多次请求）。
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }

    /// 这次请求的完整提示大小：三项输入之和。
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    /// 全部 token：完整提示加输出。
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens().saturating_add(self.output_tokens)
    }

    /// 按 `models.json` 的价格（每百万 token，美元）估算金额。
    ///
    /// 缓存读写各按自己的单价；`models.json` 没声明时按完整输入价计（见
    /// [`crate::config::models::CostDef`]）。
    pub fn cost_usd(&self, cost: &crate::config::models::CostDef) -> f64 {
        let per_token = |tokens: u64, per_mtok: f64| tokens as f64 * per_mtok / 1_000_000.0;
        per_token(self.input_tokens, cost.input)
            + per_token(
                self.cache_read_tokens,
                cost.cache_read.unwrap_or(cost.input),
            )
            + per_token(
                self.cache_write_tokens,
                cost.cache_write.unwrap_or(cost.input),
            )
            + per_token(self.output_tokens, cost.output)
    }
}

/// provider 拒绝的细节。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RefusalDetails {
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub explanation: String,
}
