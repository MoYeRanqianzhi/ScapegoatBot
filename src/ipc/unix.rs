//! Unix 域套接字服务端。
//!
//! 设计依据：设计 §2.2。
//!
//! - 路径优先 `$XDG_RUNTIME_DIR/<name>.sock`，回退 `~/.sbot/run/<name>.sock`。
//! - 所在目录必须只有当前用户能进入（0700）：别人既不能抢先放同名套接字，也不能
//!   在 bind 与 chmod 之间连进来。
//! - 启动时先探测：能连上说明已有实例，拒绝启动；连不上说明是陈旧文件，删除后重新 bind。
//! - 套接字本身再设为 0600。

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
        ensure_private_dir(parent)?;
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

/// 确保 `dir` 存在且只有当前用户能进入。
///
/// 新建时直接用 0700；已存在但对其他用户开放时收紧到 0700。收紧失败（目录不属于
/// 当前用户）就拒绝启动——在别人控制的目录里放控制套接字不安全。
fn ensure_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    let mode = std::fs::metadata(dir)
        .with_context(|| format!("cannot inspect {}", dir.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).with_context(
            || {
                format!(
                    "{} is accessible to other users and cannot be made private; \
                     set XDG_RUNTIME_DIR to a directory only you can enter",
                    dir.display()
                )
            },
        )?;
    }
    Ok(())
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
                match accepted {
                    Ok((stream, _addr)) => {
                        let handler = Arc::clone(&handler);
                        tokio::spawn(async move {
                            super::handle_connection(stream, handler).await;
                        });
                    }
                    // 瞬时错误（文件描述符用尽、对端在握手中断开）：控制通道不能因为一次
                    // accept 失败就永久消失，稍等再接受。
                    Err(error) => {
                        warn!(%error, "unix socket accept failed; retrying");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return Ok(());
                }
            }
        }
    }
}
