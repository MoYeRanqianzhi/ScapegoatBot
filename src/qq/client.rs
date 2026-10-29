//! NapCat 客户端封装。
//!
//! 设计依据：设计 §4 / §9.7 / §10。
//!
//! 只做三件事：
//! - 建立 WebSocket 连接并暴露事件订阅；
//! - 提供统一的 action 调用（`API` 工具直接复用）；
//! - 把发送结果解析成消息 id，供发送账本与去重使用。
//!
//! 断线重连由 NapCatLink 内置的退避策略负责；补消息与去重在本模块之上（`resync`）。

use anyhow::{Context, Result, bail};
use napcat_link::{EventSubscription, MessageSegment, NapLink};
use serde_json::Value;

use super::event::ChatId;

/// 登录信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginInfo {
    pub user_id: u64,
    pub nickname: String,
}

/// NapCat 客户端。
pub struct QqClient {
    link: NapLink,
}

impl QqClient {
    /// 构造客户端。
    ///
    /// `token` 为空时不设置鉴权头（对应 NapCat 未配置 token 的情况）。
    pub fn new(url: &str, token: Option<&str>) -> Result<Self> {
        let mut builder = NapLink::builder(url);
        if let Some(token) = token.filter(|token| !token.is_empty()) {
            builder = builder.token(token);
        }
        let link = builder.build().context("cannot build the NapCat client")?;
        Ok(Self { link })
    }

    /// 建立连接。
    pub async fn connect(&self) -> Result<()> {
        self.link
            .connect()
            .await
            .context("cannot connect to NapCat")
    }

    /// 订阅事件流。
    pub fn subscribe(&self) -> EventSubscription {
        self.link.subscribe()
    }

    /// 原始 action 调用（`API` 工具与内部查询共用）。
    ///
    /// NapCatLink 返回的是 OneBot 响应中的 `data` 字段（错误已由库转成 `Err`），
    /// 因此这里直接返回 `data`，不再二次解包。
    pub async fn call(&self, action: &str, params: Value) -> Result<Value> {
        self.link
            .call_api(action, params)
            .await
            .with_context(|| format!("NapCat action {action} failed"))
    }

    /// 查询登录账号。
    pub async fn login_info(&self) -> Result<LoginInfo> {
        let data = self.call("get_login_info", Value::Null).await?;
        let user_id = data
            .get("user_id")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("get_login_info did not return user_id"))?;
        Ok(LoginInfo {
            user_id,
            nickname: data
                .get("nickname")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
    }

    /// NapCat 版本信息（设计 §9.7：低于最低版本时告警但继续）。
    pub async fn version_info(&self) -> Result<Value> {
        self.call("get_version_info", Value::Null).await
    }

    /// 发送消息，返回 `message_id`。
    pub async fn send_message(&self, chat: ChatId, segments: Vec<MessageSegment>) -> Result<i64> {
        let (action, params) = match chat {
            ChatId::Group(group) => (
                "send_group_msg",
                serde_json::json!({ "group_id": group, "message": segments }),
            ),
            ChatId::Private(user) => (
                "send_private_msg",
                serde_json::json!({ "user_id": user, "message": segments }),
            ),
        };
        let response = self.call(action, params).await?;
        response
            .get("message_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow::anyhow!("{action} did not return message_id"))
    }

    /// 发送纯文本（已剥离内部标签），返回 `message_id`。
    pub async fn send_text(&self, chat: ChatId, text: &str, reply_to: Option<i64>) -> Result<i64> {
        let segments = super::format::to_segments(reply_to, text);
        if segments.is_empty() {
            bail!("refusing to send an empty message");
        }
        self.send_message(chat, segments).await
    }

    /// 标记会话已读（设计 §4「首次成功请求后按来源标记」）。
    pub async fn mark_read(&self, chat: ChatId) -> Result<()> {
        let _ = match chat {
            ChatId::Private(user) => {
                self.call(
                    "mark_private_msg_as_read",
                    serde_json::json!({ "user_id": user }),
                )
                .await
            }
            ChatId::Group(group) => {
                self.call(
                    "mark_group_msg_as_read",
                    serde_json::json!({ "group_id": group }),
                )
                .await
            }
        };
        Ok(())
    }

    /// 群列表 / 好友列表，供动态视图使用。
    pub async fn group_list(&self) -> Result<Value> {
        self.call("get_group_list", Value::Null).await
    }

    pub async fn friend_list(&self) -> Result<Value> {
        self.call("get_friend_list", Value::Null).await
    }
}
