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

/// 用量。
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

    /// 按 `models.json` 的价格估算美元金额。
    pub fn cost_usd(&self, input_per_mtok: f64, output_per_mtok: f64) -> f64 {
        let input =
            (self.input_tokens + self.cache_write_tokens) as f64 * input_per_mtok / 1_000_000.0;
        let output = self.output_tokens as f64 * output_per_mtok / 1_000_000.0;
        input + output
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
