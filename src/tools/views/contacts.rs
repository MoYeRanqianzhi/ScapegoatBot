//! `Views` 的 contacts 部分。
//!
//! 由 `views/mod.rs` 的 `list` / `read` 路由调用。

use super::*;

impl Views {
    pub(super) async fn list_friends(&self) -> Result<Vec<ViewEntry>, VfsError> {
        let Some(client) = &self.client else {
            // 离线：仍然列出 VFS 里已有的 notes 目录。
            return self.vfs_list_fallback(&VfsPath::parse("/friends").expect("valid"));
        };
        let response = client
            .friend_list()
            .await
            .map_err(|error| VfsError::Offline(error.to_string()))?;
        let list = response
            .get("data")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(list
            .iter()
            .filter_map(|friend| friend.get("user_id").and_then(serde_json::Value::as_u64))
            .map(|qq| dir_entry(&format!("/friends/{qq}")))
            .collect())
    }

    pub(super) async fn list_groups(&self) -> Result<Vec<ViewEntry>, VfsError> {
        let Some(client) = &self.client else {
            return self.vfs_list_fallback(&VfsPath::parse("/groups").expect("valid"));
        };
        let response = client
            .group_list()
            .await
            .map_err(|error| VfsError::Offline(error.to_string()))?;
        let list = response
            .get("data")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(list
            .iter()
            .filter_map(|group| group.get("group_id").and_then(serde_json::Value::as_u64))
            .map(|gid| dir_entry(&format!("/groups/{gid}")))
            .collect())
    }

    pub(super) async fn list_friend(
        &self,
        _scope: &str,
        peer: &str,
    ) -> Result<Vec<ViewEntry>, VfsError> {
        Ok(vec![
            text_entry(&format!("/friends/{peer}/profile.md")),
            text_entry(&format!("/friends/{peer}/info.json")),
            text_entry(&format!("/friends/{peer}/notes.md")),
        ])
    }

    pub(super) async fn list_group(&self, group: &str) -> Result<Vec<ViewEntry>, VfsError> {
        Ok(vec![
            text_entry(&format!("/groups/{group}/profile.md")),
            text_entry(&format!("/groups/{group}/info.json")),
            dir_entry(&format!("/groups/{group}/members")),
            text_entry(&format!("/groups/{group}/notes.md")),
        ])
    }

    pub(super) async fn list_group_members(&self, group: &str) -> Result<Vec<ViewEntry>, VfsError> {
        let Some(client) = &self.client else {
            return Err(VfsError::Offline("/groups members".to_owned()));
        };
        let response = client
            .call(
                "get_group_member_list",
                serde_json::json!({"group_id": group.parse::<u64>().unwrap_or(0)}),
            )
            .await
            .map_err(|error| VfsError::Offline(error.to_string()))?;
        let list = response
            .get("data")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(list
            .iter()
            .filter_map(|member| member.get("user_id").and_then(serde_json::Value::as_u64))
            .map(|qq| text_entry(&format!("/groups/{group}/members/{qq}.md")))
            .collect())
    }

    pub(super) async fn read_friend_file(
        &self,
        _scope: &str,
        peer: &str,
    ) -> Result<ViewRead, VfsError> {
        let path = VfsPath::parse(&format!("/friends/{peer}"))?;
        let Some(client) = &self.client else {
            return Err(VfsError::Offline(format!("/friends/{peer}")));
        };
        let response = client
            .friend_list()
            .await
            .map_err(|error| VfsError::Offline(error.to_string()))?;
        let list = response
            .get("data")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let friend = list
            .iter()
            .find(|item| {
                item.get("user_id")
                    .and_then(serde_json::Value::as_u64)
                    .map(|id| id.to_string())
                    .as_deref()
                    == Some(peer)
            })
            .ok_or_else(|| VfsError::NotFound(path.to_string()))?;
        Ok(ViewRead::Text(format!(
            "# Friend {peer}\n\n```json\n{}\n```\n",
            serde_json::to_string_pretty(friend).unwrap_or_default()
        )))
    }

    pub(super) async fn read_group_file(
        &self,
        group: &str,
        file: &str,
    ) -> Result<ViewRead, VfsError> {
        let _ = file;
        let Some(client) = &self.client else {
            return Err(VfsError::Offline(format!("/groups/{group}")));
        };
        let response = client
            .group_list()
            .await
            .map_err(|error| VfsError::Offline(error.to_string()))?;
        let list = response
            .get("data")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let found = list.iter().find(|item| {
            item.get("group_id")
                .and_then(serde_json::Value::as_u64)
                .map(|id| id.to_string())
                .as_deref()
                == Some(group)
        });
        let group = found.ok_or_else(|| VfsError::NotFound(format!("/groups/{group}")))?;
        Ok(ViewRead::Text(format!(
            "# Group {group}\n\n```json\n{}\n```\n",
            serde_json::to_string_pretty(group).unwrap_or_default()
        )))
    }

    pub(super) async fn read_group_member(
        &self,
        group: &str,
        member: &str,
    ) -> Result<ViewRead, VfsError> {
        let Some(client) = &self.client else {
            return Err(VfsError::Offline(format!("/groups/{group}/members")));
        };
        let response = client
            .call(
                "get_group_member_info",
                serde_json::json!({
                    "group_id": group.parse::<u64>().unwrap_or(0),
                    "user_id": member.parse::<u64>().unwrap_or(0),
                }),
            )
            .await
            .map_err(|error| VfsError::Offline(error.to_string()))?;
        Ok(ViewRead::Text(format!(
            "# Member {member} of {group}\n\n```json\n{}\n```\n",
            serde_json::to_string_pretty(response.get("data").unwrap_or(&response))
                .unwrap_or_default()
        )))
    }

    // ---------------------------------------------------------------------
    // /config /system
    // ---------------------------------------------------------------------
}
