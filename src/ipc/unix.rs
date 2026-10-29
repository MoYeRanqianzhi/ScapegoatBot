//! Unix 域套接字服务端。
//!
//! 设计依据：设计 §2.2。
//!
//! - 路径优先 `$XDG_RUNTIME_DIR/<name>.sock`，回退临时目录。
//! - 启动时先探测：能连上说明已有实例，拒绝启动；连不上说明是陈旧文件，删除后重新 bind。
//! - 权限设置为 0600，只允许当前用户连接。

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tracing::{debug, warn};

use super::Handler;

/// 接受连接，直到 `shutdown` 变为 `true`。
pub async fn serve(
    path: &Path,
    handler: Arc<dyn Handler>,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }

    // 先判断是否为陈旧套接字：能连上 = 已有实例；连不上 = 可以安全删除。
    if path.exists() {
        match UnixStream::connect(path).await {
            Ok(_) => {
                anyhow::bail!(
                    "another instance is already listening on {}",
                    path.display()
                );
            }
            Err(_) => {
                debug!(path = %path.display(), "removing stale socket");
                std::fs::remove_file(path)
                    .with_context(|| format!("cannot remove stale socket {}", path.display()))?;
            }
        }
    }

    let listener =
        UnixListener::bind(path).with_context(|| format!("cannot bind {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot set permissions on {}", path.display()))?;

    let result = accept_loop(&listener, handler, shutdown).await;

    // 无论正常退出还是出错，都清理套接字文件，避免下次启动误判。
    if let Err(error) = std::fs::remove_file(path) {
        warn!(%error, path = %path.display(), "cannot remove socket on shutdown");
    }
    result
}

async fn accept_loop(
    listener: &UnixListener,
    handler: Arc<dyn Handler>,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    loop {
        if *shutdown.borrow() {
            debug!("ipc server shutting down");
            return Ok(());
        }

        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _addr) = accepted.context("unix socket accept failed")?;
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    super::handle_connection(stream, handler).await;
                });
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return Ok(());
                }
            }
        }
    }
}
