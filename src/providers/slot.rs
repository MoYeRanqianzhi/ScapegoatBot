//! 当前生效的 provider 快照。
//!
//! `sbot model set` / `sbot effort set` 通过共享的 `RwLock` 原地替换，
//! 下一回合立即生效；正在进行的回合继续使用它开始时的快照。

use std::sync::Arc;

use super::{Provider, ThinkingOptions};

/// 当前生效的 provider 快照。
#[derive(Clone)]
pub struct ProviderSlot {
    /// `provider/id` 形式的引用，用于日志与运行时上下文。
    pub reference: String,
    /// 发给 provider 的 API model id。
    pub api_model: String,
    pub provider: Arc<dyn Provider>,
    pub max_tokens: u32,
    pub thinking: ThinkingOptions,
}

/// 共享的 provider 槽位。
pub type SharedProviderSlot = Arc<std::sync::RwLock<ProviderSlot>>;
