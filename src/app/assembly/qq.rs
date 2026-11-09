//! NapCat 连接与 owner / master 解析。

use std::sync::Arc;

use anyhow::Result;
use tracing::warn;

use crate::config::{AgentMode, AuthFile, SbotConfig};
use crate::qq::{LoginInfo, QqClient};

/// 连接 NapCat；失败不阻止守护进程启动。
pub(in crate::app) async fn connect_qq(
    config: &SbotConfig,
    auth: &AuthFile,
) -> Option<(Arc<QqClient>, LoginInfo)> {
    let token = auth.napcat.access_token.value();
    let client = match QqClient::new(&config.napcat.url, token.as_deref()) {
        Ok(client) => Arc::new(client),
        Err(error) => {
            warn!(%error, "cannot build the NapCat client");
            return None;
        }
    };
    match tokio::time::timeout(std::time::Duration::from_secs(10), client.connect()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            warn!(%error, url = %config.napcat.url, "cannot connect to NapCat");
            return None;
        }
        Err(_) => {
            warn!(url = %config.napcat.url, "NapCat connection timed out");
            return None;
        }
    }
    match tokio::time::timeout(std::time::Duration::from_secs(10), client.login_info()).await {
        Ok(Ok(info)) => Some((client, info)),
        Ok(Err(error)) => {
            warn!(%error, "cannot read NapCat login info");
            None
        }
        Err(_) => {
            warn!("NapCat login info timed out");
            None
        }
    }
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
