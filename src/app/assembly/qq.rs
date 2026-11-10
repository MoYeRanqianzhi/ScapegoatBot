//! NapCat 连接与 owner / master 解析。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::warn;

use crate::config::{AgentMode, AuthFile, SbotConfig};
use crate::qq::{LoginInfo, QqClient};

/// 等 NapCat 时两次尝试之间的最短 / 最长间隔；每次失败翻倍。
const WAIT_MIN: Duration = Duration::from_secs(5);
const WAIT_MAX: Duration = Duration::from_secs(60);
/// 一次连接最多等多久。SDK 自己的连接超时是 30 秒，正常情况下轮不到这个上限。
const CONNECT_LIMIT: Duration = Duration::from_secs(40);
/// `get_login_info` 最多等多久。
const LOGIN_LIMIT: Duration = Duration::from_secs(10);

/// 按配置建 NapCat 客户端（还没有连接）。地址建不出客户端时返回错误。
pub(in crate::app) fn napcat_client(config: &SbotConfig, auth: &AuthFile) -> Result<Arc<QqClient>> {
    let token = auth.napcat.access_token.value();
    Ok(Arc::new(QqClient::new(
        &config.napcat.url,
        token.as_deref(),
    )?))
}

/// 等到 NapCat 连上并且 QQ 已登录，返回登录账号。
///
/// 连不上、超时、没有账号登录都是“还没好”：按 [`WAIT_MIN`]…[`WAIT_MAX`] 退避一直重试，
/// 每次失败记一条警告（`url` 只用于这条日志）。不会自己放弃；调用方用 `select!` 让等待
/// 可以被停止信号打断。
///
/// 从头到尾用调用方给的这一个客户端：NapCatLink 的客户端被丢弃后，它的后台任务互相持有
/// 对方的通道而不会退出，每次重试都新建一个就是持续泄漏。
pub(in crate::app) async fn wait_for_login(client: &QqClient, url: &str) -> LoginInfo {
    wait_for_login_with(client, url, WAIT_MIN, WAIT_MAX).await
}

/// [`wait_for_login`] 的实现；重试间隔可以指定（测试用很短的值，不必真的等上几秒）。
async fn wait_for_login_with(
    client: &QqClient,
    url: &str,
    wait_min: Duration,
    wait_max: Duration,
) -> LoginInfo {
    let mut delay = wait_min;
    loop {
        match log_in(client).await {
            Ok(login) => return login,
            Err(error) => warn!(
                error = %format!("{error:#}"),
                url,
                retry_in_secs = delay.as_secs(),
                "waiting for NapCat; the agent starts once it is connected and logged in"
            ),
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(wait_max);
    }
}

/// 一次尝试：没连上就连，然后读登录账号。
async fn log_in(client: &QqClient) -> Result<LoginInfo> {
    tokio::time::timeout(CONNECT_LIMIT, client.ensure_connected())
        .await
        .context("the NapCat connection attempt timed out")??;
    let login = tokio::time::timeout(LOGIN_LIMIT, client.login_info())
        .await
        .context("NapCat did not answer get_login_info in time")??;
    // WebSocket 服务先于 QQ 登录就绪：这时没有账号，owner / master 都无从解析。
    if login.user_id == 0 {
        anyhow::bail!("NapCat is running but no QQ account is logged in");
    }
    Ok(login)
}

/// 解析 owner/master。
pub(in crate::app) fn resolve_principals(config: &SbotConfig, self_id: u64) -> Result<(u64, u64)> {
    let agent = &config.agent;
    let owner_qq = match agent.mode {
        AgentMode::StandIn => {
            if agent.owner_qq != 0 {
                agent.owner_qq
            } else {
                self_id
            }
        }
        AgentMode::Companion => {
            if agent.owner_qq == 0 {
                anyhow::bail!("companion mode requires a non-zero owner_qq");
            }
            if self_id != 0 && agent.owner_qq == self_id {
                anyhow::bail!("companion mode requires owner_qq to differ from the account");
            }
            agent.owner_qq
        }
    };
    let master_qq = if agent.master == "self" {
        self_id
    } else {
        agent
            .master
            .parse()
            .map_err(|_| anyhow::anyhow!("agent.master must be \"self\" or a QQ number"))?
    };
    Ok((owner_qq, master_qq))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;

    use super::*;

    /// 测试里的重试间隔。
    const FAST: Duration = Duration::from_millis(20);

    /// 最小的 OneBot 服务端：接受任意多个连接，`get_login_info` 回的账号由
    /// `user_id(这是第几次被问)` 决定，其余 action 回空对象。
    /// 返回（接受过的连接数，`get_login_info` 被问的次数）。
    fn serve_logins(
        listener: TcpListener,
        user_id: fn(u32) -> u64,
    ) -> (Arc<AtomicU32>, Arc<AtomicU32>) {
        let connections = Arc::new(AtomicU32::new(0));
        let asked = Arc::new(AtomicU32::new(0));
        let counters = (Arc::clone(&connections), Arc::clone(&asked));
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                    continue;
                };
                connections.fetch_add(1, Ordering::SeqCst);
                let asked = Arc::clone(&asked);
                tokio::spawn(async move {
                    while let Some(Ok(Message::Text(text))) = socket.next().await {
                        let Ok(request) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        let data = if request["action"] == "get_login_info" {
                            let nth = asked.fetch_add(1, Ordering::SeqCst) + 1;
                            json!({"user_id": user_id(nth), "nickname": "MockBot"})
                        } else {
                            json!({})
                        };
                        let response = json!({
                            "status": "ok",
                            "retcode": 0,
                            "data": data,
                            "echo": request.get("echo").cloned().unwrap_or(Value::Null),
                        });
                        if socket
                            .send(Message::Text(response.to_string().into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        counters
    }

    /// 守护进程先于 NapCat 启动：一开始连不上，NapCat 起来之后同一个客户端连上并登录。
    #[tokio::test]
    async fn waits_until_napcat_is_listening() {
        // 先占一个端口再放掉：这时没有任何服务在听。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("ws://{address}");
        let client = Arc::new(QqClient::new(&url, None).unwrap());
        // 确认现在确实连不上（Windows 上被拒绝的本机连接要重试约 2 秒才报错）。
        assert!(client.ensure_connected().await.is_err());

        let waiting = tokio::spawn({
            let (client, url) = (Arc::clone(&client), url.clone());
            async move { wait_for_login_with(&client, &url, FAST, FAST).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiting.is_finished(), "nothing is listening yet");

        let listener = TcpListener::bind(address).await.unwrap();
        let (connections, _) = serve_logins(listener, |_| 10000);
        let login = tokio::time::timeout(Duration::from_secs(20), waiting)
            .await
            .expect("did not log in after NapCat came up")
            .unwrap();
        assert_eq!(login.user_id, 10000);
        assert_eq!(login.nickname, "MockBot");
        assert!(client.is_connected());
        assert_eq!(connections.load(Ordering::SeqCst), 1);
    }

    /// NapCat 的 WebSocket 先于 QQ 登录就绪：账号是 0 时接着等，而且是在同一个连接上再问，
    /// 不重新建连接（对已连接的客户端再 `connect()` 会被 SDK 拒绝，那样就永远等不出来）。
    #[tokio::test]
    async fn waits_until_an_account_is_logged_in() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        // 前两次还没登录，第三次有账号了。
        let (connections, asked) = serve_logins(listener, |nth| if nth < 3 { 0 } else { 10000 });
        let client = QqClient::new(&url, None).unwrap();

        let login = tokio::time::timeout(
            Duration::from_secs(20),
            wait_for_login_with(&client, &url, FAST, FAST),
        )
        .await
        .expect("did not log in once an account appeared");
        assert_eq!(login.user_id, 10000);
        assert_eq!(asked.load(Ordering::SeqCst), 3);
        assert_eq!(connections.load(Ordering::SeqCst), 1);
    }
}
