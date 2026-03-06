// src/whitelist.rs
// 白名单检查引擎
// 在运行时快速判断一个会话（群/私聊）是否允许收消息或发消息
// 使用 HashSet 实现 O(1) 查找

use crate::config::WhitelistConfig;
use std::collections::HashSet;

// ============================================================================
// 白名单检查器
// ============================================================================

/// 白名单检查器
/// 从配置中构建 HashSet，提供 O(1) 时间复杂度的收/发权限查询
/// 核心约束：发白名单 ⊆ 收白名单（在 AppConfig::validate_whitelist 中保证）
#[derive(Debug, Clone)]
pub struct Whitelist {
    /// 可接收消息的群号集合
    recv_groups: HashSet<i64>,
    /// 可接收消息的私聊 QQ 号集合
    recv_privates: HashSet<i64>,
    /// 可发送消息的群号集合
    send_groups: HashSet<i64>,
    /// 可发送消息的私聊 QQ 号集合
    send_privates: HashSet<i64>,
}

impl Whitelist {
    /// 从白名单配置构建检查器
    pub fn new(config: &WhitelistConfig) -> Self {
        Self {
            recv_groups: config.recv_groups.iter().copied().collect(),
            recv_privates: config.recv_privates.iter().copied().collect(),
            send_groups: config.send_groups.iter().copied().collect(),
            send_privates: config.send_privates.iter().copied().collect(),
        }
    }

    /// 检查是否可以接收指定群的消息
    pub fn can_recv_group(&self, group_id: i64) -> bool {
        self.recv_groups.contains(&group_id)
    }

    /// 检查是否可以接收指定用户的私聊消息
    pub fn can_recv_private(&self, user_id: i64) -> bool {
        self.recv_privates.contains(&user_id)
    }

    /// 检查是否可以向指定群发送消息
    pub fn can_send_group(&self, group_id: i64) -> bool {
        self.send_groups.contains(&group_id)
    }

    /// 检查是否可以向指定用户发送私聊消息
    pub fn can_send_private(&self, user_id: i64) -> bool {
        self.send_privates.contains(&user_id)
    }
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> WhitelistConfig {
        WhitelistConfig {
            recv_groups: vec![100, 200, 300],
            recv_privates: vec![1000, 2000],
            send_groups: vec![100],
            send_privates: vec![1000],
        }
    }

    #[test]
    fn recv_group_in_whitelist() {
        let wl = Whitelist::new(&make_config());
        assert!(wl.can_recv_group(100));
        assert!(wl.can_recv_group(200));
        assert!(!wl.can_recv_group(999));
    }

    #[test]
    fn send_group_subset() {
        let wl = Whitelist::new(&make_config());
        // 100 在发白名单中
        assert!(wl.can_send_group(100));
        // 200 在收白名单但不在发白名单
        assert!(!wl.can_send_group(200));
        // 999 完全不在白名单中
        assert!(!wl.can_send_group(999));
    }

    #[test]
    fn recv_private_in_whitelist() {
        let wl = Whitelist::new(&make_config());
        assert!(wl.can_recv_private(1000));
        assert!(wl.can_recv_private(2000));
        assert!(!wl.can_recv_private(9999));
    }

    #[test]
    fn send_private_subset() {
        let wl = Whitelist::new(&make_config());
        assert!(wl.can_send_private(1000));
        assert!(!wl.can_send_private(2000));
    }
}
