//! `internal` 源：运行时状态变化。
//!
//! 生产者（安全闸、预算、子代理管理器、存储巡检）只持有一个 [`InternalBus`]，调用
//! [`InternalBus::emit`]——同步、不阻塞、没人订阅时直接丢弃，可以在任何上下文里调用
//! （包括持锁的同步代码）。`MonitorManager::listen` 订阅总线，把事件交给 `source` 为
//! `internal:<kind>` 的 monitor。
//!
//! 事件种类（`event.type`）：
//!
//! | kind | `event.type` | 何时 |
//! |------|--------------|------|
//! | `security` | `security:hold` / `security:released` | 进入 / 解除 hold（只在状态变化时） |
//! | `budget` | `budget:exhausted` | 每日 token 或金额预算当天第一次耗尽 |
//! | `agent` | `agent:done` / `agent:failed` | 子代理一次运行结束 |
//! | `storage` | `storage:quota` / `storage:disk` | VFS 用量越过 90%、磁盘剩余低于下限（越线时一次） |

use tokio::sync::broadcast;

/// 总线容量：生产者很少，满了说明没人消费，旧事件被覆盖即可。
const CAPACITY: usize = 64;

/// 事件来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternalKind {
    Security,
    Budget,
    Agent,
    Storage,
}

impl InternalKind {
    /// 全部种类，按文档顺序。
    pub const ALL: [Self; 4] = [Self::Security, Self::Budget, Self::Agent, Self::Storage];

    /// 配置与工具参数里的写法。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Security => "security",
            Self::Budget => "budget",
            Self::Agent => "agent",
            Self::Storage => "storage",
        }
    }

    /// 解析 `security` / `budget` / `agent` / `storage`。
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == raw)
    }

    /// monitor 的 `source` 字段值：`internal:<kind>`。
    pub fn source(self) -> String {
        format!("internal:{}", self.as_str())
    }

    /// 校验失败时的说明。
    pub fn expected() -> &'static str {
        "internal source must be security, budget, agent or storage"
    }
}

/// 一次状态变化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalEvent {
    pub kind: InternalKind,
    /// 事件名（`hold`、`exhausted`……）；`event.type` 是 `<kind>:<name>`。
    pub name: &'static str,
    /// 给 agent 看的一行说明（英文）。
    pub text: String,
}

impl InternalEvent {
    /// `event.type` 的值。
    pub fn event_type(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.name)
    }
}

/// 内部事件总线；克隆共享同一个通道。
#[derive(Clone)]
pub struct InternalBus {
    sender: broadcast::Sender<InternalEvent>,
}

impl Default for InternalBus {
    fn default() -> Self {
        Self::new()
    }
}

impl InternalBus {
    pub fn new() -> Self {
        Self {
            sender: broadcast::channel(CAPACITY).0,
        }
    }

    /// 发出一个事件；没有订阅者时丢弃。
    pub fn emit(&self, kind: InternalKind, name: &'static str, text: impl Into<String>) {
        let _ = self.sender.send(InternalEvent {
            kind,
            name,
            text: text.into(),
        });
    }

    /// 订阅之后发出的事件。
    pub fn subscribe(&self) -> broadcast::Receiver<InternalEvent> {
        self.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip() {
        for kind in InternalKind::ALL {
            assert_eq!(InternalKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(InternalKind::parse("network"), None);
        assert_eq!(InternalKind::Budget.source(), "internal:budget");
    }

    #[tokio::test]
    async fn subscribers_see_events_emitted_after_they_subscribe() {
        let bus = InternalBus::new();
        // 没人订阅：丢弃，不报错。
        bus.emit(InternalKind::Security, "hold", "lost");
        let mut receiver = bus.subscribe();
        bus.clone()
            .emit(InternalKind::Budget, "exhausted", "out of tokens");
        let event = receiver.recv().await.unwrap();
        assert_eq!(event.event_type(), "budget:exhausted");
        assert_eq!(event.text, "out of tokens");
    }
}
