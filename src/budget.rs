//! 运行预算：每日 token/USD、单回合 token/墙钟、每 chat 频率与 guard 花费。
//!
//! 设计依据：设计 §5.12 与 `[budget]`。
//!
//! 计数器按本地日期存放在 `meta` 表，进程重启不清零；写入串行化，避免多个
//! chat 并发回合丢更新。guard 花费是估算值：Jev 按固定每次调用成本，LLM
//! 分类器按输入长度与输出预留估算（见 `guard_call_usd`）。

use std::sync::Arc;

use anyhow::Result;
use chrono::Local;
use tokio::sync::Mutex;

use crate::config::BudgetConfig;
use crate::config::models::CostDef;
use crate::storage::Database;

const MICROS_PER_USD: f64 = 1_000_000.0;

/// Jev 分类器每次调用的估算成本（美元）。Jev 响应不带计费字段，
/// 这里只用于让 `guard_usd_per_day` 有一个可执行的近似值。
pub const JEV_CALL_USD: f64 = 0.000_02;

/// 预算跟踪器；由 runtime 持有，跨 chat 共享。
pub struct Budget {
    db: Arc<Database>,
    config: BudgetConfig,
    input_cost: f64,
    output_cost: f64,
    /// 单次 guard 调用的估算成本。
    guard_cost: f64,
    lock: Mutex<()>,
}

impl Budget {
    pub fn new(
        db: Arc<Database>,
        config: &BudgetConfig,
        cost: &CostDef,
        guard_cost: f64,
    ) -> Arc<Self> {
        Arc::new(Self {
            db,
            config: config.clone(),
            input_cost: cost.input,
            output_cost: cost.output,
            guard_cost,
            lock: Mutex::new(()),
        })
    }

    /// 单回合 token 上限；0 = 不限制。
    pub fn turn_tokens(&self) -> u64 {
        self.config.turn_tokens
    }

    /// 单回合墙钟上限（秒）；0 = 不限制。
    pub fn wall_secs(&self) -> u64 {
        self.config.round_trip_wall_secs
    }

    /// 每 chat 每小时发送上限；0 = 不限制。
    pub fn chat_sends_per_hour(&self) -> u32 {
        self.config.chat_sends_per_hour
    }

    /// 每 chat 每小时回合上限；0 = 不限制。
    pub fn chat_turns_per_hour(&self) -> u32 {
        self.config.chat_turns_per_hour
    }

    /// 新回合开始前的每日检查；返回拒绝原因。
    pub async fn check_daily(&self) -> Option<String> {
        let (tokens, usd) = self.today().await;
        if self.config.daily_tokens > 0 && tokens >= self.config.daily_tokens {
            return Some(format!(
                "daily token budget exhausted ({tokens}/{}); next turn after local midnight",
                self.config.daily_tokens
            ));
        }
        if self.config.daily_usd > 0.0 && usd >= self.config.daily_usd {
            return Some(format!(
                "daily USD budget exhausted (${:.4}/${:.4}); next turn after local midnight",
                usd, self.config.daily_usd
            ));
        }
        None
    }

    /// 记录一次模型调用（或一个回合累计）的输入/输出 token。
    pub async fn record_usage(&self, input: u64, output: u64) {
        let _guard = self.lock.lock().await;
        let tokens = input.saturating_add(output);
        let micros = (input as f64 * self.input_cost + output as f64 * self.output_cost)
            .round()
            .max(0.0) as u64;
        let _ = self.add("tokens", tokens).await;
        let _ = self.add("usd", micros).await;
    }

    /// guard 预算是否还允许调用分类器。
    pub async fn guard_allowed(&self) -> bool {
        if self.config.guard_usd_per_day <= 0.0 || self.guard_cost <= 0.0 {
            return true;
        }
        let key = format!("budget:{}:guard", self.day());
        let spent = self
            .db
            .meta_get(&key)
            .ok()
            .flatten()
            .and_then(|raw| String::from_utf8(raw).ok())
            .and_then(|text| text.parse::<u64>().ok())
            .unwrap_or(0);
        spent as f64 / MICROS_PER_USD + self.guard_cost <= self.config.guard_usd_per_day
    }

    /// 记录一次 guard 调用估算成本。
    pub async fn record_guard(&self) {
        let _guard = self.lock.lock().await;
        let micros = (self.guard_cost * MICROS_PER_USD).round().max(0.0) as u64;
        if micros > 0 {
            let _ = self.add("guard", micros).await;
        }
    }

    /// 今日已用 token 与美元。
    pub async fn today(&self) -> (u64, f64) {
        let tokens = self
            .read("tokens")
            .await
            .and_then(|text| text.parse::<u64>().ok())
            .unwrap_or(0);
        let micros = self
            .read("usd")
            .await
            .and_then(|text| text.parse::<u64>().ok())
            .unwrap_or(0);
        (tokens, micros as f64 / MICROS_PER_USD)
    }

    async fn read(&self, suffix: &str) -> Option<String> {
        let key = format!("budget:{}:{suffix}", self.day());
        self.db
            .meta_get(&key)
            .ok()
            .flatten()
            .and_then(|raw| String::from_utf8(raw).ok())
    }

    async fn add(&self, suffix: &str, delta: u64) -> Result<()> {
        if delta == 0 {
            return Ok(());
        }
        let key = format!("budget:{}:{suffix}", self.day());
        let current = self
            .db
            .meta_get(&key)?
            .and_then(|raw| String::from_utf8(raw).ok())
            .and_then(|text| text.parse::<u64>().ok())
            .unwrap_or(0);
        self.db
            .meta_set(&key, current.saturating_add(delta).to_string().as_bytes())?;
        Ok(())
    }

    fn day(&self) -> String {
        Local::now().format("%Y-%m-%d").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(config: BudgetConfig, cost: CostDef) -> Arc<Budget> {
        let db = Arc::new(Database::open_in_memory().unwrap());
        Budget::new(db, &config, &cost, 0.01)
    }

    #[tokio::test]
    async fn daily_limits_accumulate() {
        let budget = budget(
            BudgetConfig {
                daily_tokens: 100,
                daily_usd: 1.0,
                ..BudgetConfig::default()
            },
            CostDef {
                input: 10.0,
                output: 10.0,
            },
        );
        assert!(budget.check_daily().await.is_none());
        budget.record_usage(50, 30).await;
        assert_eq!(budget.today().await.0, 80);
        budget.record_usage(10, 10).await;
        let reason = budget.check_daily().await.expect("token budget");
        assert!(reason.contains("daily token"));
    }

    #[tokio::test]
    async fn daily_usd_uses_model_cost() {
        let budget = budget(
            BudgetConfig {
                daily_tokens: 0,
                daily_usd: 0.5,
                ..BudgetConfig::default()
            },
            CostDef {
                input: 1_000_000.0,
                output: 1_000_000.0,
            },
        );
        budget.record_usage(1, 0).await;
        assert!(budget.check_daily().await.is_some());
    }

    #[tokio::test]
    async fn guard_budget_stops_spending() {
        let budget = budget(
            BudgetConfig {
                guard_usd_per_day: 0.025,
                ..BudgetConfig::default()
            },
            CostDef::default(),
        );
        assert!(budget.guard_allowed().await);
        budget.record_guard().await;
        assert!(budget.guard_allowed().await);
        budget.record_guard().await;
        assert!(!budget.guard_allowed().await);
    }
}
