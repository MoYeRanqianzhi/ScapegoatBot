//! `Views` 的联系人部分：`/friends` 与 `/groups`，实时查询 NapCat。
//!
//! 路由（与 `skills/sbot-use/references/tools-and-vfs.md` 一致）：
//!
//! | 路径 | 列目录 | 读文件 |
//! |------|--------|--------|
//! | `/friends` | 每个好友一个 `<qq>.md` | — |
//! | `/friends/<qq>.md` | — | 好友资料 |
//! | `/groups` | 每个群一个 `<gid>.md` 与一个 `<gid>/` | — |
//! | `/groups/<gid>.md` | — | 群资料 |
//! | `/groups/<gid>` | 每个成员一个 `<qq>.md` | — |
//! | `/groups/<gid>/<qq>.md` | — | 群成员资料 |
//!
//! 所有路径都受 `receive_scopes` 约束；不在范围内的好友/群既不列出也不能读。
//! `QqClient::call` 返回的已经是 OneBot 响应的 `data`，这里不再解一层。

use serde_json::{Value, json};

use super::*;

impl Views {
    /// 调一次 NapCat；没有连接或调用失败都是 `Offline`。
    async fn contacts_query(&self, action: &str, params: Value) -> Result<Value, VfsError> {
        let Some(client) = &self.client else {
            return Err(VfsError::Offline(format!(
                "{action}: NapCat is not connected"
            )));
        };
        client.call(action, params).await.map_err(VfsError::Offline)
    }

    /// 列表类查询的结果数组。
    async fn contacts_list(&self, action: &str, params: Value) -> Result<Vec<Value>, VfsError> {
        match self.contacts_query(action, params).await? {
            Value::Array(items) => Ok(items),
            other => Err(VfsError::Storage(format!(
                "{action} returned {} instead of a list",
                kind_of(&other)
            ))),
        }
    }

    pub(super) async fn list_friends(&self) -> Result<Vec<ViewEntry>, VfsError> {
        let friends = self.contacts_list("get_friend_list", json!({})).await?;
        Ok(friends
            .iter()
            .filter_map(|friend| id_field(friend, "user_id"))
            .filter(|qq| self.receive_allows(ChatId::Private(*qq)))
            .map(|qq| text_entry(&format!("/friends/{qq}.md")))
            .collect())
    }

    pub(super) async fn list_groups(&self) -> Result<Vec<ViewEntry>, VfsError> {
        let groups = self.contacts_list("get_group_list", json!({})).await?;
        Ok(groups
            .iter()
            .filter_map(|group| id_field(group, "group_id"))
            .filter(|gid| self.receive_allows(ChatId::Group(*gid)))
            .flat_map(|gid| {
                [
                    text_entry(&format!("/groups/{gid}.md")),
                    dir_entry(&format!("/groups/{gid}")),
                ]
            })
            .collect())
    }

    pub(super) async fn list_group_members(&self, group: &str) -> Result<Vec<ViewEntry>, VfsError> {
        let gid = parse_id(group, "/groups")?;
        self.ensure_receive(ChatId::Group(gid))?;
        let members = self
            .contacts_list("get_group_member_list", json!({"group_id": gid}))
            .await?;
        Ok(members
            .iter()
            .filter_map(|member| id_field(member, "user_id"))
            .map(|qq| text_entry(&format!("/groups/{gid}/{qq}.md")))
            .collect())
    }

    /// `/friends/<qq>.md`：好友列表里的那一项。
    pub(super) async fn read_friend(&self, file: &str) -> Result<ViewRead, VfsError> {
        let qq = parse_md_id(file, "/friends")?;
        self.ensure_receive(ChatId::Private(qq))?;
        let friends = self.contacts_list("get_friend_list", json!({})).await?;
        let friend = friends
            .iter()
            .find(|friend| id_field(friend, "user_id") == Some(qq))
            .ok_or_else(|| VfsError::NotFound(format!("/friends/{file}")))?;
        Ok(profile(&format!("Friend {qq}"), friend))
    }

    /// `/groups/<gid>.md`：群资料。
    pub(super) async fn read_group(&self, file: &str) -> Result<ViewRead, VfsError> {
        let gid = parse_md_id(file, "/groups")?;
        self.ensure_receive(ChatId::Group(gid))?;
        let info = self
            .contacts_query("get_group_info", json!({"group_id": gid}))
            .await?;
        Ok(profile(&format!("Group {gid}"), &info))
    }

    /// `/groups/<gid>/<qq>.md`：群成员资料。
    pub(super) async fn read_group_member(
        &self,
        group: &str,
        file: &str,
    ) -> Result<ViewRead, VfsError> {
        let gid = parse_id(group, "/groups")?;
        self.ensure_receive(ChatId::Group(gid))?;
        let qq = parse_md_id(file, &format!("/groups/{gid}"))?;
        let info = self
            .contacts_query(
                "get_group_member_info",
                json!({"group_id": gid, "user_id": qq}),
            )
            .await?;
        Ok(profile(&format!("Member {qq} of group {gid}"), &info))
    }
}

/// 资料文件：标题 + 原始 JSON。
fn profile(title: &str, value: &Value) -> ViewRead {
    ViewRead::Text(format!(
        "# {title}\n\n```json\n{}\n```\n",
        serde_json::to_string_pretty(value).unwrap_or_default()
    ))
}

/// 数字 id 字段；NapCat 有时把 id 写成字符串。
fn id_field(value: &Value, key: &str) -> Option<u64> {
    match value.get(key)? {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// 路径段是一个非零 QQ/群号。
fn parse_id(segment: &str, parent: &str) -> Result<u64, VfsError> {
    segment
        .parse()
        .ok()
        .filter(|id| *id != 0)
        .ok_or_else(|| VfsError::NotFound(format!("{parent}/{segment}")))
}

/// `<id>.md` 形式的文件名。
fn parse_md_id(file: &str, parent: &str) -> Result<u64, VfsError> {
    let stem = file
        .strip_suffix(".md")
        .ok_or_else(|| VfsError::NotFound(format!("{parent}/{file}")))?;
    parse_id(stem, parent)
}

/// 错误信息里用的 JSON 类型名。
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::api::NapcatGateway;

    /// 按 action 返回固定数据的假网关（形状与 `QqClient::call` 一致：已解包的 `data`）。
    struct FakeNapcat;

    #[async_trait::async_trait]
    impl NapcatGateway for FakeNapcat {
        async fn call(&self, action: &str, params: Value) -> Result<Value, String> {
            Ok(match action {
                "get_friend_list" => {
                    json!([{"user_id": 1001, "nickname": "A"}, {"user_id": "1002"}])
                }
                "get_group_list" => json!([{"group_id": 77}, {"group_id": 88}]),
                "get_group_member_list" => json!([{"user_id": 1001}]),
                "get_group_info" => json!({"group_id": params["group_id"], "group_name": "G"}),
                "get_group_member_info" => json!({"user_id": params["user_id"], "card": "c"}),
                other => return Err(format!("unexpected {other}")),
            })
        }
    }

    fn views(scopes: &[&str]) -> (Views, tempfile::TempDir) {
        let (views, dir) = super::super::tests::views_for_tests();
        let views = views.with_gateway(Arc::new(FakeNapcat));
        let views = views.with_context(ViewContext {
            receive_scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            ..ViewContext::default()
        });
        (views, dir)
    }

    fn paths(entries: Vec<ViewEntry>) -> Vec<String> {
        entries.into_iter().map(|entry| entry.path).collect()
    }

    #[tokio::test]
    async fn contacts_list_and_read_through_the_documented_paths() {
        let (views, _dir) = views(&["*"]);
        assert_eq!(
            paths(views.list_friends().await.unwrap()),
            vec!["/friends/1001.md", "/friends/1002.md"]
        );
        assert_eq!(
            paths(views.list_groups().await.unwrap()),
            vec!["/groups/77.md", "/groups/77", "/groups/88.md", "/groups/88"]
        );
        assert_eq!(
            paths(views.list_group_members("77").await.unwrap()),
            vec!["/groups/77/1001.md"]
        );
        let ViewRead::Text(friend) = views.read_friend("1001.md").await.unwrap() else {
            panic!("expected text");
        };
        assert!(friend.contains("\"nickname\": \"A\""));
        let ViewRead::Text(group) = views.read_group("77.md").await.unwrap() else {
            panic!("expected text");
        };
        assert!(group.contains("\"group_name\": \"G\""));
        let ViewRead::Text(member) = views.read_group_member("77", "1001.md").await.unwrap() else {
            panic!("expected text");
        };
        assert!(member.contains("\"card\": \"c\""));
        assert!(views.read_friend("9.md").await.is_err());
        assert!(views.read_friend("1001").await.is_err());
    }

    #[tokio::test]
    async fn contacts_respect_receive_scopes() {
        let (views, _dir) = views(&["group:77", "private:1002"]);
        assert_eq!(
            paths(views.list_friends().await.unwrap()),
            vec!["/friends/1002.md"]
        );
        assert_eq!(
            paths(views.list_groups().await.unwrap()),
            vec!["/groups/77.md", "/groups/77"]
        );
        assert!(views.read_group("88.md").await.is_err());
        assert!(views.list_group_members("88").await.is_err());
        assert!(views.read_friend("1001.md").await.is_err());
    }
}
