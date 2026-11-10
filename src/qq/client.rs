//! NapCat 客户端封装。
//!
//! 设计依据：设计 §4 / §9.7 / §10。
//!
//! 只做三件事：
//! - 建立 WebSocket 连接并暴露事件订阅；
//! - 提供统一的 action 调用（`API` 工具直接复用）；
//! - 把发送结果解析成消息 id，并在发送账本里登记（见 [`super::ledger`]）。
//!
//! 断线重连分两层：NapCatLink 内置的退避策略先试（最多 10 次，约 7 分钟），它放弃之后
//! 由 [`QqClient::keep_connected`] 接手，一直重试到连上为止。这个 agent 无人值守，
//! “重连几次就不再试”等于永久失联。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use napcat_link::{ConnectionState, EventSubscription, MessageSegment, NapLink};
use serde_json::Value;
use tokio::sync::watch;
use tracing::{info, warn};

use super::event::ChatId;
use super::ledger::{OutboundLedger, produces_message};

/// [`QqClient::keep_connected`] 多久看一次连接状态。
const CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// 重连失败后的最短 / 最长等待；每次失败翻倍。
const RETRY_MIN: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(300);

/// 登录信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginInfo {
    pub user_id: u64,
    pub nickname: String,
}

/// NapCat 客户端。
pub struct QqClient {
    link: NapLink,
    /// 本进程发出的消息；`message_sent` 回声据此区分 agent 的话与手打的消息。
    ledger: OutboundLedger,
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
        Ok(Self {
            link,
            ledger: OutboundLedger::default(),
        })
    }

    /// 建立连接。
    pub async fn connect(&self) -> Result<()> {
        self.link
            .connect()
            .await
            .context("cannot connect to NapCat")
    }

    /// 连接是否处于“已连接”。
    pub fn is_connected(&self) -> bool {
        self.link.state() == ConnectionState::Connected
    }

    /// 看着连接，直到 `shutdown` 变为 `true`：NapCatLink 放弃之后接着重连，永不放弃。
    ///
    /// NapCatLink 自己会重连，但有三种情况会停下并把状态留在 `Disconnected`：重连次数用尽、
    /// 服务端正常关闭（NapCat 重启时常见）、连上后连续几次立刻被断开（token 不对时）。
    /// 停下之后没有任何东西会再调用 `connect()`，守护进程就此与 QQ 失联到被人重启为止。
    ///
    /// 这里每隔 [`CHECK_INTERVAL`] 看一次状态，只在 `Disconnected`（SDK 已经停手）时出手；
    /// `Connecting` / `Reconnecting` 是 SDK 正在处理，不去打扰（它的 actor 在重连期间
    /// 不处理新命令，排进去的 `connect()` 只会在它完成后收到一条“已连接”的拒绝）。
    /// 连不上时按 [`RETRY_MIN`]…[`RETRY_MAX`] 指数退避；连上后退避归零。
    ///
    /// 只负责“恢复已经建立过的连接”：启动时就没连上的情况不走这里（那时账号还不知道，
    /// 见 `app::assembly::connect_qq`）。
    pub fn keep_connected(self: &Arc<Self>, shutdown: watch::Receiver<bool>) {
        self.keep_connected_every(CHECK_INTERVAL, RETRY_MIN, shutdown);
    }

    /// [`Self::keep_connected`] 的实现；检查间隔与最短重试等待可以指定（集成测试用很短的值，
    /// 不必真的等上几秒）。
    pub fn keep_connected_every(
        self: &Arc<Self>,
        check_interval: Duration,
        retry_min: Duration,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let client = Arc::clone(self);
        tokio::spawn(async move {
            let mut delay = retry_min;
            // 上一次看到的是不是“已连接”：只在变化时记日志，断线期间不刷屏。
            let mut was_connected = client.is_connected();
            loop {
                tokio::select! {
                    changed = shutdown.changed() => {
                        // 发送端没了（进程在退出）或明确要求停止。
                        if changed.is_err() || *shutdown.borrow() {
                            return;
                        }
                        continue;
                    }
                    _ = tokio::time::sleep(check_interval) => {}
                }
                let state = client.link.state();
                let connected = state == ConnectionState::Connected;
                if connected != was_connected {
                    if connected {
                        info!("NapCat connection restored");
                    } else {
                        warn!("NapCat connection lost; QQ actions fail until it is back");
                    }
                    was_connected = connected;
                }
                if state != ConnectionState::Disconnected {
                    // 已连接，或 SDK 正在连接 / 重连。
                    if connected {
                        delay = retry_min;
                    }
                    continue;
                }
                match client.connect().await {
                    Ok(()) => {
                        info!("reconnected to NapCat");
                        was_connected = true;
                        delay = retry_min;
                    }
                    Err(error) => {
                        warn!(%error, retry_in_secs = delay.as_secs(), "cannot reconnect to NapCat");
                        tokio::select! {
                            changed = shutdown.changed() => {
                                if changed.is_err() || *shutdown.borrow() {
                                    return;
                                }
                            }
                            _ = tokio::time::sleep(delay) => {}
                        }
                        delay = (delay * 2).min(RETRY_MAX);
                    }
                }
            }
        });
    }

    /// 订阅事件流。
    pub fn subscribe(&self) -> EventSubscription {
        self.link.subscribe()
    }

    /// 发送账本。
    pub fn ledger(&self) -> &OutboundLedger {
        &self.ledger
    }

    /// 原始 action 调用（`API` 工具、运行时通知与内部查询共用）。
    ///
    /// NapCatLink 返回的是 OneBot 响应中的 `data` 字段（错误已由库转成 `Err`），
    /// 因此这里直接返回 `data`，不再二次解包。
    ///
    /// 会产生消息的 action 在调用前登记进账本、返回后补记 id：本进程的每一条
    /// 发送都经过这里，回声才能被认出来。
    pub async fn call(&self, action: &str, params: Value) -> Result<Value> {
        // 守卫保证登记一定会结束：调用方的超时会直接丢弃这个 future，
        // 永远“在途”的登记会让该 chat 后续的手打消息都被当成 agent 的话。
        let mut guard = produces_message(action).then(|| SendGuard {
            ledger: &self.ledger,
            ticket: self.ledger.begin(action, &params),
        });
        let result = self
            .link
            .call_api(action, params)
            .await
            .map_err(|error| anyhow::anyhow!("NapCat action {action} failed: {error}"));
        if let Some(guard) = guard.take() {
            guard.finish(result.as_ref().ok());
        }
        result
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
}

/// 在途发送的守卫：正常返回时带响应结束；被丢弃时按“没有响应”结束。
struct SendGuard<'a> {
    ledger: &'a OutboundLedger,
    ticket: u64,
}

impl SendGuard<'_> {
    fn finish(self, response: Option<&Value>) {
        self.ledger.finish(self.ticket, response);
        // 已经结束，不再让 Drop 重复结束。
        std::mem::forget(self);
    }
}

impl Drop for SendGuard<'_> {
    fn drop(&mut self) {
        self.ledger.finish(self.ticket, None);
    }
}
