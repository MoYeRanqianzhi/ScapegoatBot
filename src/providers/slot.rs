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
    /// 模型上下文窗口（`models.json`）；0 = 未声明。压缩阈值按它计算。
    pub context_window: u64,
    /// 模型是否接受图片输入；只影响入站 QQ 图片是否自动注入。
    pub supports_images: bool,
}

/// 共享的 provider 槽位。
pub type SharedProviderSlot = Arc<std::sync::RwLock<ProviderSlot>>;
