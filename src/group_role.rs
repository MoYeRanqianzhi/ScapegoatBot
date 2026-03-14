// src/group_role.rs
// 群身份状态管理
// 跟踪 Bot 在各群的角色（群主 / 管理员 / 普通成员），
// 供系统 prompt 注入和工具权限提示使用。
// 支持三种同步方式：启动全量同步、事件驱动即时更新、12 小时定时全量同步。

use napcat_link::api::OneBotApi;
use std::collections::HashMap;
use std::sync::RwLock;

// ============================================================================
// 群角色枚举
// ============================================================================

/// Bot 在群内的角色
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupRole {
    /// 群主
    Owner,
    /// 管理员
    Admin,
    /// 普通成员
    Member,
}

impl GroupRole {
    /// 从 OneBot API 返回的 role 字符串解析角色
    /// API 返回值："owner" / "admin" / "member"
    pub fn from_api_str(s: &str) -> Self {
        match s {
            "owner" => GroupRole::Owner,
            "admin" => GroupRole::Admin,
            _ => GroupRole::Member,
        }
    }

    /// 转换为中文显示文本
    pub fn display_cn(&self) -> &'static str {
        match self {
            GroupRole::Owner => "群主",
            GroupRole::Admin => "管理员",
            GroupRole::Member => "普通成员",
        }
    }

    /// 是否拥有管理员权限（群主和管理员都有）
    pub fn is_admin_or_above(&self) -> bool {
        matches!(self, GroupRole::Owner | GroupRole::Admin)
    }
}

// ============================================================================
// 群身份状态（线程安全共享）
// ============================================================================

/// 群身份状态
/// 通过 `Arc<GroupRoleState>` 在多个任务间共享：
/// - AgentRunner：读取状态用于 prompt 注入
/// - 事件监听任务：接收 notice.group_admin 事件即时更新
/// - 定时同步任务：每 12 小时全量同步一次
pub struct GroupRoleState {
    /// group_id → 角色的映射表
    /// 使用 std::sync::RwLock（非 tokio 的），因为持锁时间极短（仅读写 HashMap）
    roles: RwLock<HashMap<i64, GroupRole>>,
}

impl GroupRoleState {
    /// 创建空的群身份状态
    pub fn new() -> Self {
        Self {
            roles: RwLock::new(HashMap::new()),
        }
    }

    /// 更新单个群的角色（事件驱动更新用）
    pub fn set_role(&self, group_id: i64, role: GroupRole) {
        let mut map = self.roles.write().expect("GroupRoleState write lock poisoned");
        map.insert(group_id, role);
    }

    /// 批量替换所有角色（全量同步用）
    /// 旧数据被完全丢弃，替换为新的映射表
    pub fn replace_all(&self, new_roles: HashMap<i64, GroupRole>) {
        let mut map = self.roles.write().expect("GroupRoleState write lock poisoned");
        *map = new_roles;
    }

    /// 格式化为系统 prompt section 文本
    /// 输出示例：
    /// ```text
    /// ## 群身份
    ///
    /// 你在以下群拥有管理员权限（可使用管理员工具）：
    /// - 群 123456789（管理员）
    /// - 群 987654321（群主）
    ///
    /// 你在其他群是普通成员，管理员工具将会执行失败。
    /// 使用管理员工具前请确认你在目标群拥有相应权限。
    /// ```
    pub fn format_prompt_section(&self) -> String {
        let map = self.roles.read().expect("GroupRoleState read lock poisoned");

        // 收集有管理权限的群（管理员 + 群主），按群号排序便于阅读
        let mut privileged: Vec<(i64, GroupRole)> = map
            .iter()
            .filter(|(_, role)| role.is_admin_or_above())
            .map(|(&gid, &role)| (gid, role))
            .collect();
        privileged.sort_by_key(|(gid, _)| *gid);

        let mut section = String::from("## 群身份\n\n");

        if privileged.is_empty() {
            // 没有任何管理权限
            section.push_str(
                "你在所有群都是普通成员，没有管理员权限。管理员工具将会执行失败。\n",
            );
        } else {
            // 列出有权限的群
            section.push_str("你在以下群拥有管理员权限（可使用管理员工具）：\n");
            for (gid, role) in &privileged {
                section.push_str(&format!("- 群 {}（{}）\n", gid, role.display_cn()));
            }
            section.push('\n');
            section.push_str(
                "你在其他群是普通成员，管理员工具将会执行失败。\n\
                 使用管理员工具前请确认你在目标群拥有相应权限。\n",
            );
        }

        section
    }
}

// ============================================================================
// 全量同步
// ============================================================================

/// 查询所有监听群，提取 Bot 自身在各群的角色
/// 对 recv_groups 中每个群调用 get_group_member_info，从 JSON 中解析 role 字段
///
/// # 参数
/// - `api`: NapCatLink API 聚合器
/// - `bot_qq`: Bot 自身 QQ 号
/// - `recv_groups`: 接收白名单中的群号列表
///
/// # 返回
/// group_id → GroupRole 的映射表（查询失败的群被跳过）
pub async fn sync_group_roles(
    api: &OneBotApi,
    bot_qq: i64,
    recv_groups: &[i64],
) -> HashMap<i64, GroupRole> {
    let mut result = HashMap::new();

    for &group_id in recv_groups {
        // 查询 Bot 在该群的成员信息
        match api
            .account
            .get_group_member_info(group_id, bot_qq, None)
            .await
        {
            Ok(info) => {
                // 从返回的 JSON 中提取 role 字段
                let role_str = info
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("member");
                let role = GroupRole::from_api_str(role_str);
                result.insert(group_id, role);
                tracing::debug!("群 {} 角色: {:?}", group_id, role);
            }
            Err(e) => {
                // 单个群查询失败不影响其他群，记录警告后跳过
                tracing::warn!("查询群 {} 成员信息失败（跳过）: {}", group_id, e);
            }
        }
    }

    tracing::info!(
        "群角色全量同步完成: {} 个群（管理员/群主 {} 个）",
        result.len(),
        result.values().filter(|r| r.is_admin_or_above()).count()
    );

    result
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试 GroupRole::from_api_str 解析
    #[test]
    fn parse_role_strings() {
        assert_eq!(GroupRole::from_api_str("owner"), GroupRole::Owner);
        assert_eq!(GroupRole::from_api_str("admin"), GroupRole::Admin);
        assert_eq!(GroupRole::from_api_str("member"), GroupRole::Member);
        // 未知字符串默认为 Member
        assert_eq!(GroupRole::from_api_str("unknown"), GroupRole::Member);
        assert_eq!(GroupRole::from_api_str(""), GroupRole::Member);
    }

    /// 测试 is_admin_or_above
    #[test]
    fn admin_check() {
        assert!(GroupRole::Owner.is_admin_or_above());
        assert!(GroupRole::Admin.is_admin_or_above());
        assert!(!GroupRole::Member.is_admin_or_above());
    }

    /// 测试 GroupRoleState 的基本操作
    #[test]
    fn state_set_and_replace() {
        let state = GroupRoleState::new();

        // 设置单个角色
        state.set_role(100, GroupRole::Admin);
        state.set_role(200, GroupRole::Owner);

        // 格式化输出应包含两个群
        let section = state.format_prompt_section();
        assert!(section.contains("群 100"));
        assert!(section.contains("群 200"));
        assert!(section.contains("管理员"));
        assert!(section.contains("群主"));

        // 全量替换
        let mut new_roles = HashMap::new();
        new_roles.insert(300, GroupRole::Admin);
        state.replace_all(new_roles);

        // 旧数据应被清除
        let section = state.format_prompt_section();
        assert!(!section.contains("群 100"));
        assert!(section.contains("群 300"));
    }

    /// 测试无管理员权限时的 prompt 输出
    #[test]
    fn empty_state_prompt() {
        let state = GroupRoleState::new();
        let section = state.format_prompt_section();
        assert!(section.contains("没有管理员权限"));
    }

    /// 测试普通成员不出现在 prompt 中
    #[test]
    fn member_not_shown() {
        let state = GroupRoleState::new();
        state.set_role(100, GroupRole::Member);
        let section = state.format_prompt_section();
        assert!(!section.contains("群 100"));
        assert!(section.contains("没有管理员权限"));
    }
}
