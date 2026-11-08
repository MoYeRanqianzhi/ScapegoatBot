//! 运行预算：每日 token/USD、单回合 token/墙钟、每 chat 频率与 guard 花费。
//!
//! 设计依据：设计 §5.12 与 `[budget]`。
//!
//! 计数器按本地日期存放在 `meta` 表，进程重启不清零；写入串行化，避免多个
//! chat 并发回合丢更新。
//!
//! 模型用量不由调用方上报：每个运行时 provider 都经 `providers::build_metered`
//! 包了一层计量，每次请求结束时按**发出请求的那个模型**的价格记进来——主回合、
//! 回退、子代理、压缩、记忆 flush、安全审查、LLM 门禁都一样，含缓存读写 token。
//!
//! guard 花费另计且是估算值：Jev 按固定每次调用成本，LLM 分类器按一次判定的
//! 典型输入输出估算（`app::assembly` 里的 `guard_cost`）。

use std::sync::Arc;

use anyhow::Result;
use chrono::Local;
use tokio::sync::Mutex;

use crate::config::BudgetConfig;
use crate::config::models::CostDef;
use crate::llm::types::Usage;
use crate::storage::Database;

const MICROS_PER_USD: f64 = 1_000_000.0;

/// Jev 分类器每次调用的估算成本（美元）。Jev 响应不带计费字段，
/// 这里只用于让 `guard_usd_per_day` 有一个可执行的近似值。
pub const JEV_CALL_USD: f64 = 0.000_02;

/// 预算跟踪器；由 runtime 持有，跨 chat 共享。
pub struct Budget {
    db: Arc<Database>,
    config: BudgetConfig,
    /// 单次 guard 调用的估算成本。
    guard_cost: f64,
    lock: Mutex<()>,
    /// 预算耗尽时通知 `internal:budget` monitor；运行时装配后接入。
    events: std::sync::OnceLock<crate::monitor::InternalBus>,
    /// 已经通知过耗尽的那一天（本地日期）；每天最多通知一次。
    exhausted_day: std::sync::Mutex<String>,
}

impl Budget {
    pub fn new(db: Arc<Database>, config: &BudgetConfig, guard_cost: f64) -> Arc<Self> {
        Arc::new(Self {
            db,
            config: config.clone(),
            guard_cost,
            lock: Mutex::new(()),
            events: std::sync::OnceLock::new(),
            exhausted_day: std::sync::Mutex::new(String::new()),
        })
    }

    /// 接入内部事件总线；只接一次。
    pub fn set_events(&self, bus: crate::monitor::InternalBus) {
        let _ = self.events.set(bus);
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
    ///
    /// 当天第一次耗尽时发一个 `budget:exhausted` 内部事件。
    pub async fn check_daily(&self) -> Option<String> {
        let (tokens, usd) = self.today().await;
        let reason = if self.config.daily_tokens > 0 && tokens >= self.config.daily_tokens {
            format!(
                "daily token budget exhausted ({tokens}/{}); next turn after local midnight",
                self.config.daily_tokens
            )
        } else if self.config.daily_usd > 0.0 && usd >= self.config.daily_usd {
            format!(
                "daily USD budget exhausted (${:.4}/${:.4}); next turn after local midnight",
                usd, self.config.daily_usd
            )
        } else {
            return None;
        };
        self.notify_exhausted(&reason);
        Some(reason)
    }

    /// 每天最多通知一次耗尽。
    fn notify_exhausted(&self, reason: &str) {
        let Some(bus) = self.events.get() else {
            return;
        };
        let today = self.day();
        let mut notified = self.exhausted_day.lock().expect("budget day lock");
        if *notified != today {
            *notified = today;
            bus.emit(crate::monitor::InternalKind::Budget, "exhausted", reason);
        }
    }

    /// 记录一次模型请求：全部 token（含缓存读写）计入每日 token，金额按 `cost`。
    pub async fn record(&self, cost: &CostDef, usage: &Usage) {
        let _guard = self.lock.lock().await;
        let micros = (usage.cost_usd(cost) * MICROS_PER_USD).round().max(0.0) as u64;
        let _ = self.add("tokens", usage.total_tokens()).await;
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

#[async_trait::async_trait]
impl crate::providers::metered::UsageSink for Budget {
    async fn record(&self, cost: &CostDef, usage: &Usage) {
        Budget::record(self, cost, usage).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(config: BudgetConfig) -> Arc<Budget> {
        let db = Arc::new(Database::open_in_memory().unwrap());
        Budget::new(db, &config, 0.01)
    }

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::default()
        }
    }

    #[tokio::test]
    async fn daily_limits_accumulate() {
        let budget = budget(BudgetConfig {
            daily_tokens: 100,
            daily_usd: 1.0,
            ..BudgetConfig::default()
        });
        let cost = CostDef {
            input: 10.0,
            output: 10.0,
            ..CostDef::default()
        };
        assert!(budget.check_daily().await.is_none());
        budget.record(&cost, &usage(50, 30)).await;
        assert_eq!(budget.today().await.0, 80);
        // 缓存读写也是 token：计入每日总量。
        budget
            .record(
                &cost,
                &Usage {
                    cache_read_tokens: 15,
                    cache_write_tokens: 5,
                    ..Usage::default()
                },
            )
            .await;
        let reason = budget.check_daily().await.expect("token budget");
        assert!(reason.contains("daily token"));
    }

    #[tokio::test]
    async fn exhaustion_is_announced_once_per_day() {
        let budget = budget(BudgetConfig {
            daily_tokens: 10,
            ..BudgetConfig::default()
        });
        let bus = crate::monitor::InternalBus::new();
        let mut events = bus.subscribe();
        budget.set_events(bus);
        budget.record(&CostDef::default(), &usage(20, 0)).await;
        // 每次唤醒都会检查；同一天只发一次事件。
        for _ in 0..3 {
            assert!(budget.check_daily().await.is_some());
        }
        let event = events.try_recv().unwrap();
        assert_eq!(event.event_type(), "budget:exhausted");
        assert!(event.text.contains("daily token budget exhausted"));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn daily_usd_uses_each_requests_model_cost() {
        let budget = budget(BudgetConfig {
            daily_tokens: 0,
            daily_usd: 0.5,
            ..BudgetConfig::default()
        });
        // 便宜模型的 0.1 美元不触发上限；贵模型的一次请求触发。
        let cheap = CostDef {
            input: 100_000.0,
            ..CostDef::default()
        };
        budget.record(&cheap, &usage(1, 0)).await;
        assert!(budget.check_daily().await.is_none());
        let expensive = CostDef {
            input: 1_000_000.0,
            ..CostDef::default()
        };
        budget.record(&expensive, &usage(1, 0)).await;
        assert!(budget.check_daily().await.is_some());
    }

    #[tokio::test]
    async fn guard_budget_stops_spending() {
        let budget = budget(BudgetConfig {
            guard_usd_per_day: 0.025,
            ..BudgetConfig::default()
        });
        assert!(budget.guard_allowed().await);
        budget.record_guard().await;
        assert!(budget.guard_allowed().await);
        budget.record_guard().await;
        assert!(!budget.guard_allowed().await);
    }
}
