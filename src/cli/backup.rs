//! `sbot backup`：一致性备份。
//!
//! 设计依据：设计 §6.6「备份与迁移」。
//!
//! - 用 SQLite backup API 生成一致性快照，运行中也可以执行；
//! - 连同 `key`、`sbot.toml`、`models.json`、`auth.json`、`messages/`、
//!   `sessions/`、`blobs/`、`skills/`、`agents/` 一起复制；
//! - 归档包含主密钥：泄露归档等于泄露全部内容，输出里必须明确警告；
//! - 默认写入 `~/.sbot/backups/<timestamp>/`，自定义路径时同样警告。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use rusqlite::backup::StepResult;

use crate::config::SbotConfig;
use crate::paths::SbotPaths;

/// 执行备份，返回输出目录。
pub fn run(config_path: Option<PathBuf>, output: Option<PathBuf>) -> Result<PathBuf> {
    let paths = SbotPaths::discover()?;
    run_with_paths(paths, config_path, output)
}

/// 用显式路径布局执行备份；测试用它注入临时根目录。
fn run_with_paths(
    paths: SbotPaths,
    config_path: Option<PathBuf>,
    output: Option<PathBuf>,
) -> Result<PathBuf> {
    let config_path = config_path.unwrap_or_else(|| paths.config_file());
    let loaded = SbotConfig::load(&config_path)?;
    let data_dir = paths.resolve(&loaded.config.storage.path, &loaded.dir);
    let db_path = data_dir.join("data.db");
    if !db_path.exists() {
        bail!(
            "no database at {}; is this a fresh install?",
            db_path.display()
        );
    }

    let skills_dir = paths.resolve(&loaded.config.skills.shared_dir, &loaded.dir);
    let agents_dir = paths.resolve(&loaded.config.swarm.agents_dir, &loaded.dir);

    let output = output.unwrap_or_else(|| {
        paths
            .home()
            .join("backups")
            .join(chrono::Local::now().format("%Y%m%dT%H%M%S").to_string())
    });
    // 归档目录不能落在被复制的目录里，否则复制会递归包含正在写入的归档。
    for (name, source) in [
        ("storage.path", &data_dir),
        ("skills.shared_dir", &skills_dir),
        ("swarm.agents_dir", &agents_dir),
    ] {
        if output.starts_with(source) {
            bail!(
                "refusing to write the backup inside {name} ({}); choose another --output",
                source.display()
            );
        }
    }
    std::fs::create_dir_all(&output)
        .with_context(|| format!("cannot create {}", output.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o700));
    }

    // SQLite 一致性快照。
    //
    // 用底层 `Backup` 句柄而不是 `Connection::backup(path)`：
    // 守护进程运行时源库仍可能被写入，`Busy` / `Locked` 应当重试而不是直接失败。
    {
        let source =
            Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("cannot open {}", db_path.display()))?;
        source
            .busy_timeout(std::time::Duration::from_secs(5))
            .context("cannot configure the source busy timeout")?;
        let mut destination = Connection::open(output.join("data.db"))?;
        {
            let backup = rusqlite::backup::Backup::new(&source, &mut destination)
                .context("cannot start the SQLite backup")?;
            let mut attempts = 0u32;
            loop {
                match backup.step(100)? {
                    StepResult::Done => break,
                    // 还有页未复制，继续。
                    StepResult::More => continue,
                    // 源库正在写：等待后重试，最多 10 秒。
                    StepResult::Busy | StepResult::Locked => {
                        attempts += 1;
                        if attempts > 100 {
                            bail!("SQLite backup stayed busy for 10 seconds");
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    // `StepResult` 是 non_exhaustive；未知结果按失败处理，
                    // 避免在不认识的返回码上死循环。
                    other => bail!("unexpected SQLite backup step result: {other:?}"),
                }
            }
        }
        destination.close().map_err(|(_, error)| error)?;
    }

    let mut copied = 0u64;
    // 配置与密钥按“实际生效的路径”复制：它们可能被显式放在根目录之外。
    for (source, name) in [
        (paths.key_file(), "key"),
        (loaded.path.clone(), "sbot.toml"),
        (
            paths.resolve(&loaded.config.models_file, &loaded.dir),
            "models.json",
        ),
        (
            paths.resolve(&loaded.config.auth_file, &loaded.dir),
            "auth.json",
        ),
    ] {
        if source.exists() {
            std::fs::copy(&source, output.join(name))
                .with_context(|| format!("cannot copy {}", source.display()))?;
            copied += 1;
        } else if name == "key" {
            bail!("master key is missing at {}", source.display());
        }
    }

    // 数据目录：JSONL 分区与 blob；SQLite 已在上面做快照。
    for (source, name) in [
        (data_dir.join("messages"), "messages"),
        (data_dir.join("sessions"), "sessions"),
        (data_dir.join("blobs"), "blobs"),
        // 技能与子代理定义可能在根目录，也可能被配置指到别处。
        (skills_dir, "skills"),
        (agents_dir, "agents"),
    ] {
        if source.exists() {
            copy_dir(&source, &output.join(name))?;
            copied += 1;
        }
    }

    println!(
        "backup written to {}\n\
         copied {copied} item(s).\n\
         WARNING: this archive includes the master key. Anyone who can read the archive can read \
         every message, transcript and VFS file in it. Store it with the same care as ~/.sbot.",
        output.display()
    );
    Ok(output)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("cannot copy {}", entry.path().display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;
    use crate::storage::crypto::MasterKey;

    /// 建一个带数据库、密钥、配置与一条消息文件的临时实例。
    fn setup() -> (tempfile::TempDir, SbotPaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = SbotPaths::from_home(dir.path());
        std::fs::create_dir_all(paths.home()).unwrap();
        std::fs::write(
            paths.config_file(),
            toml::to_string(&SbotConfig::default()).unwrap(),
        )
        .unwrap();
        let key = MasterKey::load_or_create(paths.key_file()).unwrap();
        let db = Database::open(paths.data_dir().join("data.db")).unwrap();
        db.ensure_key(&key).unwrap();
        drop(db);

        let day_file = paths
            .data_dir()
            .join("messages/groups/10001/2026-10/30.jsonl");
        std::fs::create_dir_all(day_file.parent().unwrap()).unwrap();
        std::fs::write(&day_file, "{\"file_id\":\"x\"}\n").unwrap();
        (dir, paths)
    }

    #[test]
    fn copies_database_key_config_and_messages() {
        let (_dir, paths) = setup();
        let output = paths.home().join("out");

        let written = run_with_paths(paths.clone(), None, Some(output.clone())).unwrap();
        assert_eq!(written, output);
        for name in ["data.db", "key", "sbot.toml"] {
            assert!(output.join(name).is_file(), "{name} should be copied");
        }
        assert!(
            output
                .join("messages/groups/10001/2026-10/30.jsonl")
                .is_file(),
            "message JSONL should be copied"
        );

        // 快照必须是可打开的完整数据库。
        let conn = Connection::open(output.join("data.db")).unwrap();
        let version: u32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(version >= 1, "backup lost the schema migrations");
    }

    #[test]
    fn refuses_output_inside_the_storage_directory() {
        let (_dir, paths) = setup();
        let output = paths.data_dir().join("backup");
        let error = run_with_paths(paths, None, Some(output)).unwrap_err();
        assert!(
            error.to_string().contains("refusing"),
            "unexpected error: {error}"
        );
    }
}
