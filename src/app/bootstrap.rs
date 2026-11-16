//! 启动辅助：默认配置、单实例锁、哨兵文件、配置持久化与终止信号。

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt;
use tracing::{info, warn};

use crate::config::{AuthFile, SbotConfig};

// ---------------------------------------------------------------------------
// 启动辅助
// ---------------------------------------------------------------------------

pub(super) fn ensure_default_config(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(&SbotConfig::default())
        .context("cannot serialize the default configuration")?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(text.as_bytes())
                .with_context(|| format!("cannot write {}", path.display()))?;
            info!(path = %path.display(), "created default configuration");
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(anyhow::anyhow!("cannot create {}: {error}", path.display())),
    }
}

pub(super) fn report_missing_credentials(config: &SbotConfig, auth: &AuthFile) {
    let referenced = config
        .agent
        .available_models
        .iter()
        .filter_map(|reference| {
            reference
                .split_once('/')
                .map(|(provider, _)| provider.to_owned())
        });
    let missing = auth.missing_providers(referenced);
    if !missing.is_empty() {
        warn!(
            providers = ?missing,
            "missing API keys in auth.json; models from these providers are unavailable"
        );
    }
}

pub(super) struct InstanceLock {
    file: std::fs::File,
    path: PathBuf,
}

impl InstanceLock {
    pub(super) fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("cannot open lock file {}", path.display()))?;
        let pid_path = path.with_file_name("instance.pid");

        if let Err(error) = file.try_lock_exclusive() {
            let existing = std::fs::read_to_string(&pid_path).unwrap_or_default();
            anyhow::bail!(
                "another sbot instance is already running (lock {} held{}): {error}",
                path.display(),
                if existing.trim().is_empty() {
                    String::new()
                } else {
                    format!(" by pid {}", existing.trim())
                }
            );
        }
        let _ = write!(file, "{}", std::process::id());
        let _ = file.flush();
        let _ = std::fs::write(&pid_path, std::process::id().to_string());
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_file_name("instance.pid"));
    }
}

pub(super) fn write_sentinel(path: &Path) -> Result<()> {
    std::fs::write(path, chrono::Utc::now().to_rfc3339())
        .with_context(|| format!("cannot create {}", path.display()))
}

pub(super) fn remove_sentinel(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(anyhow::anyhow!("cannot remove {}: {error}", path.display())),
    }
}

/// 把 `[agent]` 里的一个字符串键写回 `sbot.toml`，其余内容（包括注释与排版）不动。
///
/// 调用方持有配置句柄的写锁：agent 写 `/config` 也在同一把锁里读改写这个文件。
pub(super) fn persist_agent_setting(path: &Path, key: &str, value: &str) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("cannot parse {}", path.display()))?;
    let agent = document
        .entry("agent")
        .or_insert(toml_edit::table())
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[agent] in {} is not a table", path.display()))?;
    agent[key] = toml_edit::value(value);
    crate::config::replace_file(path, &document.to_string())
        .with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
pub(super) async fn terminate_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut stream) => {
            stream.recv().await;
        }
        Err(error) => {
            warn!(%error, "cannot install SIGTERM handler");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(unix))]
pub(super) async fn terminate_signal() {
    std::future::pending::<()>().await
}
