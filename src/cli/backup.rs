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

    let agents_dir = paths.resolve(&loaded.config.swarm.agents_dir, &loaded.dir);

    let output = output.unwrap_or_else(|| {
        paths
            .backups_dir()
            .join(chrono::Local::now().format("%Y%m%dT%H%M%S").to_string())
    });
    // 写到词法规范化后的绝对路径：下面按它判断真实位置，两者必须是同一个地方（`a/../b`
    // 交给操作系统的话，Unix 上还会顺手把 `a` 建出来）。
    let output = crate::paths::normalize(&std::path::absolute(&output).unwrap_or(output));
    // 归档目录不能落在被复制的目录里，否则复制会递归包含正在写入的归档。比的是真实位置：
    // 同一个目录可以有别的写法（相对路径、符号链接、Windows 上的大小写）。
    let real_output = real_location(&output);
    for (name, source) in [
        ("storage.path", &data_dir),
        ("swarm.agents_dir", &agents_dir),
    ] {
        if real_output.starts_with(real_location(source)) {
            bail!(
                "refusing to write the backup inside {name} ({}); choose another --output",
                source.display()
            );
        }
    }
    // 只写进空目录。已有内容的目录要么是上一次的备份（混在一起就不是一致的快照），要么是
    // 实例目录本身——那时密钥、配置的“备份位置”就是它们自己，而把文件复制到自身会把它清空。
    if std::fs::read_dir(&output).is_ok_and(|mut entries| entries.next().is_some()) {
        bail!(
            "refusing to write the backup into {}: the directory is not empty; choose an empty \
             or new --output",
            output.display()
        );
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
        // 宿主机上的子代理定义（`[swarm].agents_dir`）；技能在 VFS 里，已随数据库快照。
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

/// 一个（可能还不存在的）路径在磁盘上的真实位置：最近的已存在祖先解析掉符号链接，后面
/// 还不存在的部分原样接上。`path` 必须是词法规范化过的绝对路径（没有 `.` 与 `..`），这样
/// 接上去的都是普通名字，结果就是之后在 `path` 上创建目录时实际落到的地方。
///
/// 一级祖先都解析不了时返回 `path` 本身。
fn real_location(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(mut real) = std::fs::canonicalize(existing) {
            real.extend(missing.iter().rev());
            return real;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
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

    /// 同一个位置的另一种写法也拒绝：判断的是真实位置，不是字符串前缀。
    #[test]
    fn refuses_output_inside_the_storage_directory_under_another_spelling() {
        let (_dir, paths) = setup();
        let data_name = paths.data_dir().file_name().unwrap().to_owned();
        // `<home>/missing/../<data>/backup`：按路径分量看不以数据目录开头。
        let output = paths
            .home()
            .join("missing")
            .join("..")
            .join(&data_name)
            .join("backup");
        let error = run_with_paths(paths.clone(), None, Some(output)).unwrap_err();
        assert!(
            error.to_string().contains("refusing"),
            "unexpected error: {error}"
        );
        assert!(!paths.data_dir().join("backup").exists());
        assert!(!paths.home().join("missing").exists());
    }

    /// 经由符号链接指进数据目录的输出路径同样拒绝。
    #[cfg(unix)]
    #[test]
    fn refuses_output_that_reaches_the_storage_directory_through_a_symlink() {
        let (dir, paths) = setup();
        let link = dir.path().join("elsewhere");
        std::os::unix::fs::symlink(paths.data_dir(), &link).unwrap();
        let error = run_with_paths(paths.clone(), None, Some(link.join("backup"))).unwrap_err();
        assert!(
            error.to_string().contains("refusing"),
            "unexpected error: {error}"
        );
        assert!(!paths.data_dir().join("backup").exists());
    }

    /// 输出目录里已经有东西就拒绝，而且什么都不碰。把实例目录本身当输出目录时，密钥、配置
    /// 与子代理定义的“备份位置”就是它们自己：复制到自身会把源文件清空。
    #[test]
    fn refuses_a_non_empty_output_and_touches_nothing() {
        let (_dir, paths) = setup();
        let agents = paths.home().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("reviewer.md"), "---\nname: reviewer\n---\nbody").unwrap();
        let key_before = std::fs::read(paths.key_file()).unwrap();
        let config_before = std::fs::read(paths.config_file()).unwrap();
        assert!(!key_before.is_empty());

        let error =
            run_with_paths(paths.clone(), None, Some(paths.home().to_path_buf())).unwrap_err();
        assert!(
            error.to_string().contains("not empty"),
            "unexpected error: {error}"
        );
        assert_eq!(std::fs::read(paths.key_file()).unwrap(), key_before);
        assert_eq!(std::fs::read(paths.config_file()).unwrap(), config_before);
        assert_eq!(
            std::fs::read_to_string(agents.join("reviewer.md")).unwrap(),
            "---\nname: reviewer\n---\nbody"
        );
        assert!(!paths.home().join("data.db").exists());
    }

    /// 已经存在的空目录可以用作输出目录。
    #[test]
    fn an_existing_empty_output_directory_is_accepted() {
        let (dir, paths) = setup();
        let output = dir.path().join("empty");
        std::fs::create_dir(&output).unwrap();
        run_with_paths(paths, None, Some(output.clone())).unwrap();
        assert!(output.join("key").is_file());
    }
}
