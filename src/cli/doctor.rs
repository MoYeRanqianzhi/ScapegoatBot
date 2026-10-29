//! `sbot doctor`：只读诊断。
//!
//! 设计依据：设计 §6.6「sbot doctor」。
//!
//! 只报告，不自动删除或修复数据：
//! - 配置 schema、models.json、auth.json 引用；
//! - `key` 与数据库 canary；
//! - 文件权限（Unix）；
//! - 消息 JSONL 与索引、transcript 分段与索引的一致性；
//! - 备份目录提醒、存储路径是否在根目录之外。

use std::path::PathBuf;

use anyhow::Result;
use rusqlite::Connection;

use crate::config::models::ModelsFile;
use crate::config::{AuthFile, SbotConfig};
use crate::paths::SbotPaths;
use crate::storage::crypto::MasterKey;

const EMBEDDED_MODELS: &str = include_str!("../../assets/models.default.json");

/// 诊断报告。
#[derive(Default)]
struct Report {
    ok: Vec<String>,
    warnings: Vec<String>,
    errors: Vec<String>,
}

impl Report {
    fn ok(&mut self, message: impl Into<String>) {
        self.ok.push(message.into());
    }
    fn warn(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }
    fn error(&mut self, message: impl Into<String>) {
        self.errors.push(message.into());
    }
}

/// 执行诊断；有错误时返回非零。
pub fn run(config_path: Option<PathBuf>) -> Result<()> {
    let paths = SbotPaths::discover()?;
    let report = collect(&paths, config_path);
    print_report(&report);
    if report.errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("doctor found {} error(s)", report.errors.len())
    }
}

/// 收集全部检查结果；不打印、不退出，便于测试。
fn collect(paths: &SbotPaths, config_path: Option<PathBuf>) -> Report {
    let config_path = config_path.unwrap_or_else(|| paths.config_file());
    let mut report = Report::default();

    let loaded = match SbotConfig::load(&config_path) {
        Ok(loaded) => {
            report.ok(format!(
                "config: {} (version {})",
                config_path.display(),
                loaded.config.version
            ));
            loaded
        }
        Err(error) => {
            report.error(format!("config: {error}"));
            return report;
        }
    };

    // models.json：内置 + 用户覆盖。
    let models = match ModelsFile::parse(EMBEDDED_MODELS) {
        Ok(mut models) => {
            let user_path = paths.resolve(&loaded.config.models_file, &loaded.dir);
            if user_path.exists() {
                match ModelsFile::load(&user_path) {
                    Ok(overlay) => {
                        models.merge(overlay);
                        report.ok(format!(
                            "models: {} (+ {})",
                            "embedded",
                            user_path.display()
                        ));
                    }
                    Err(error) => report.error(format!("models: {error}")),
                }
            } else {
                report.ok("models: embedded defaults only");
            }
            models
        }
        Err(error) => {
            report.error(format!("embedded models are invalid: {error}"));
            ModelsFile::default()
        }
    };
    if let Err(errors) = models.validate() {
        for error in errors {
            report.error(format!("models: {error}"));
        }
    }
    if let Err(errors) = loaded.config.validate_with_models(&models) {
        for error in errors.0 {
            report.error(format!("config: {error}"));
        }
    }

    // auth.json：只检查引用到的 provider。
    let auth_path = paths.resolve(&loaded.config.auth_file, &loaded.dir);
    match AuthFile::load_optional(&auth_path) {
        Ok(auth) => {
            let referenced = loaded
                .config
                .agent
                .available_models
                .iter()
                .filter_map(|reference| {
                    reference
                        .split_once('/')
                        .map(|(provider, _)| provider.to_owned())
                });
            let missing = auth.missing_providers(referenced);
            if missing.is_empty() {
                report.ok("auth: all referenced providers have keys");
            } else {
                report.warn(format!("auth: missing keys for {missing:?}"));
            }
            if auth.guard_key().is_none() && loaded.config.security.guard.enabled {
                report.warn("auth: security.guard.api_key is missing; the guard runs rules only");
            }
        }
        Err(error) => report.error(format!("auth: {error}")),
    }

    // key 与数据库。
    let key_path = paths.key_file();
    let key = match MasterKey::load_existing(&key_path) {
        Ok(key) => {
            report.ok(format!("key: {}", key_path.display()));
            Some(key)
        }
        Err(error) => {
            report.error(format!("key: {error}"));
            None
        }
    };
    #[cfg(unix)]
    if key.is_some() {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(&key_path) {
            Ok(metadata) => {
                let mode = metadata.permissions().mode() & 0o777;
                if mode == 0o600 {
                    report.ok("key permissions: 0600");
                } else {
                    report.error(format!("key permissions: {mode:o} (expected 600)"));
                }
            }
            Err(error) => report.error(format!("key metadata: {error}")),
        }
    }

    let data_dir = paths.resolve(&loaded.config.storage.path, &loaded.dir);
    let db_path = data_dir.join("data.db");
    if !db_path.exists() {
        report.error(format!("database: missing {}", db_path.display()));
    } else {
        match open_readonly(&db_path) {
            Ok(conn) => {
                // canary 需要密钥；没有密钥时仍继续检查其余结构。
                match &key {
                    None => report.warn("database: cannot verify the canary without a key"),
                    Some(key) => {
                        let canary: Option<Vec<u8>> = conn
                            .query_row(
                                "SELECT value FROM meta WHERE key = 'encryption.canary'",
                                [],
                                |row| row.get(0),
                            )
                            .ok();
                        match canary {
                            Some(sealed) if crate::storage::verify_canary(key, &sealed) => {
                                report.ok("database: canary matches the master key");
                            }
                            Some(_) => {
                                report.error("database: canary does NOT match the master key")
                            }
                            None => report.warn("database: no canary yet (fresh database)"),
                        }
                    }
                }
                let version: u32 = conn
                    .query_row(
                        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                report.ok(format!("database: schema v{version}"));
                check_indexes(&conn, &data_dir, &mut report);
                check_blobs(&conn, &data_dir, &mut report);
                if let Some(key) = &key {
                    check_memory(&conn, key, &mut report);
                }
            }
            Err(error) => report.error(format!("database: {error}")),
        }
    }

    // 备份目录权限：默认位于 `~/.sbot/backups`，归档含主密钥。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let backups = paths.home().join("backups");
        if let Ok(metadata) = std::fs::metadata(&backups) {
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                report.warn(format!(
                    "backups: {} has permissions {mode:o}; archives contain the master key and should be 0700",
                    backups.display()
                ));
            } else {
                report.ok("backups: permissions are owner-only");
            }
        }
    }

    // 主账号：`agent.owner_qq` 为 0 时，stand_in 依赖登录账号、companion 无法识别主人。
    if loaded.config.agent.mode == crate::config::AgentMode::Companion
        && loaded.config.agent.owner_qq == 0
    {
        report.error("agent.owner_qq: companion mode requires the owner's QQ number");
    } else if loaded.config.agent.owner_qq == 0 {
        report.warn("agent.owner_qq is 0: owner-only rules fall back to the login account");
    }
    if loaded.config.agent.master != "self"
        && loaded
            .config
            .agent
            .master
            .parse::<u64>()
            .map_or(true, |qq| qq == 0)
    {
        report.error("agent.master: must be \"self\" or a non-zero QQ number");
    }

    // 路径边界：允许显式放在根目录之外，但必须提醒迁移时要一起复制。
    for (name, raw) in [
        ("storage.path", loaded.config.storage.path.as_str()),
        ("models_file", loaded.config.models_file.as_str()),
        ("auth_file", loaded.config.auth_file.as_str()),
    ] {
        let resolved = paths.resolve(raw, &loaded.dir);
        if !resolved.starts_with(paths.home()) {
            report.warn(format!(
                "{name} points outside {}: {} (include it when migrating)",
                paths.home().display(),
                resolved.display()
            ));
        }
    }

    if loaded.config.storage.encryption == crate::config::EncryptionMode::None {
        report.warn("storage.encryption = none: data is stored in plaintext");
    }
    if loaded.config.session.scope == crate::config::SessionScope::Shared {
        report.warn("session.scope = shared: one transcript spans all chats (by design)");
    } else {
        report.ok("session.scope = per_chat: one transcript chain per chat");
    }
    if !loaded.config.security.guard.enabled {
        report.warn("security.guard.enabled = false: no input/output classifier");
    }

    report
}

fn open_readonly(path: &std::path::Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

/// 索引与磁盘文件的一致性（只报告数量，不解析内容）。
fn check_indexes(conn: &Connection, data_dir: &std::path::Path, report: &mut Report) {
    let message_rows: u64 = conn
        .query_row("SELECT COUNT(*) FROM message_files", [], |row| row.get(0))
        .unwrap_or(0);
    let message_files = count_files(&data_dir.join("messages"), Some("jsonl"));
    if message_rows == message_files {
        report.ok(format!("messages: {message_files} JSONL file(s) indexed"));
    } else {
        report.warn(format!(
            "messages: {message_files} JSONL file(s) on disk but {message_rows} indexed; \
             `sbot start` rebuilds the index when file counts differ"
        ));
    }

    let session_rows: u64 = conn
        .query_row("SELECT COUNT(*) FROM transcript_files", [], |row| {
            row.get(0)
        })
        .unwrap_or(0);
    let session_files = count_files(&data_dir.join("sessions"), Some("jsonl"));
    if session_rows == session_files {
        report.ok(format!("sessions: {session_files} segment(s) indexed"));
    } else {
        report.warn(format!(
            "sessions: {session_files} segment(s) on disk but {session_rows} indexed"
        ));
    }

    let open_turns: u64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT turn_id) FROM turn_buffer WHERE state = 'open'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if open_turns > 0 {
        report.warn(format!(
            "{open_turns} interrupted turn(s) in the buffer; they are replayed on the next start"
        ));
    } else {
        report.ok("turns: no interrupted turns");
    }
}

/// blob 分块与 `vfs_chunks` 的对账：只报告，不删除孤儿目录。
fn check_blobs(conn: &Connection, data_dir: &std::path::Path, report: &mut Report) {
    let blobs_dir = data_dir.join("blobs");
    if !blobs_dir.exists() {
        report.warn("blobs: directory is missing (no binary files have been written)");
        return;
    }

    // 每个有分块的节点应当有一个 `blobs/<node_id>/` 目录，文件数等于分块数。
    let mut mismatched = 0u64;
    let mut known: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let query = conn.prepare("SELECT node_id, COUNT(*) FROM vfs_chunks GROUP BY node_id");
    match query {
        Ok(mut statement) => {
            let rows =
                statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, u64>(1)?)));
            if let Ok(rows) = rows {
                for (node_id, expected) in rows.flatten() {
                    known.insert(node_id);
                    let directory = blobs_dir.join(node_id.to_string());
                    if !directory.is_dir() || count_files(&directory, None) != expected {
                        mismatched += 1;
                    }
                }
            }
        }
        Err(error) => report.warn(format!("blobs: cannot read vfs_chunks: {error}")),
    }
    if mismatched == 0 {
        report.ok(format!("blobs: {} chunked node(s) consistent", known.len()));
    } else {
        report.warn(format!(
            "blobs: {mismatched} node(s) have missing or extra chunk files; only reporting"
        ));
    }

    // 孤儿目录：目录名是数字，但数据库里没有对应节点。
    let mut orphans = 0u64;
    let mut leftovers = 0u64;
    if let Ok(entries) = std::fs::read_dir(&blobs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                match name.parse::<i64>() {
                    Ok(node_id) if !known.contains(&node_id) => orphans += 1,
                    Ok(_) => {}
                    Err(_) => leftovers += 1,
                }
            } else if name.contains(".tmp-") {
                // 写入中断留下的暂存文件。
                leftovers += 1;
            }
        }
    }
    if orphans > 0 {
        report.warn(format!(
            "blobs: {orphans} orphan director(ies) not referenced by vfs_chunks; only reporting"
        ));
    }
    if leftovers > 0 {
        report.warn(format!(
            "blobs: {leftovers} stale temp/unrecognized entr(ies); only reporting"
        ));
    }
}

/// `/MEMORY.md` 与 `/memory/*.md` 的对账（设计 §6.6）。
fn check_memory(conn: &Connection, key: &MasterKey, report: &mut Report) {
    use rusqlite::OptionalExtension;

    let index = conn
        .query_row(
            "SELECT id, version, content_enc FROM vfs_nodes
             WHERE parent_id = 1 AND name = 'MEMORY.md' AND trashed_at IS NULL",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                ))
            },
        )
        .optional();
    let (index_id, index_version, sealed) = match index {
        Ok(Some(row)) => row,
        Ok(None) => {
            report.warn("memory: /MEMORY.md is missing");
            return;
        }
        Err(error) => {
            report.warn(format!("memory: cannot read /MEMORY.md: {error}"));
            return;
        }
    };
    let Some(sealed) = sealed else {
        report.warn("memory: /MEMORY.md has no content");
        return;
    };
    let Some(text) = crate::vfs::sqlite::decrypt_node_text(key, index_id, index_version, &sealed)
    else {
        report.error("memory: cannot decrypt /MEMORY.md with this key");
        return;
    };
    let indexed: std::collections::BTreeSet<String> = text
        .lines()
        .filter_map(crate::tools::memory::index_slug)
        .collect();

    let memory_dir: Option<i64> = conn
        .query_row(
            "SELECT id FROM vfs_nodes
             WHERE parent_id = 1 AND name = 'memory' AND kind = 'dir' AND trashed_at IS NULL",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or(None);
    let mut files = std::collections::BTreeSet::new();
    if let Some(memory_dir) = memory_dir
        && let Ok(mut statement) = conn.prepare(
            "SELECT name FROM vfs_nodes
             WHERE parent_id = ?1 AND kind != 'dir' AND trashed_at IS NULL",
        )
        && let Ok(rows) = statement.query_map([memory_dir], |row| row.get::<_, String>(0))
    {
        for name in rows.flatten() {
            if let Some(slug) = name.strip_suffix(".md") {
                files.insert(slug.to_owned());
            }
        }
    }

    let missing: Vec<&String> = indexed.difference(&files).collect();
    let orphans: Vec<&String> = files.difference(&indexed).collect();
    if missing.is_empty() && orphans.is_empty() {
        report.ok(format!(
            "memory: {} entr(ies) indexed and present",
            indexed.len()
        ));
    } else {
        if !missing.is_empty() {
            report.warn(format!(
                "memory: {} index entr(ies) point at missing file(s): {missing:?}",
                missing.len()
            ));
        }
        if !orphans.is_empty() {
            report.warn(format!(
                "memory: {} file(s) under /memory are not in the index: {orphans:?}",
                orphans.len()
            ));
        }
    }

    // 第三方对账：`memory_entries` 是 Memory 工具维护的可查询索引。
    let recorded: u64 = conn
        .query_row("SELECT COUNT(*) FROM memory_entries", [], |row| row.get(0))
        .unwrap_or(0);
    if recorded != files.len() as u64 {
        report.warn(format!(
            "memory: memory_entries has {recorded} row(s) but {} file(s) exist; \
             the next Memory write rebuilds it",
            files.len()
        ));
    } else {
        report.ok(format!("memory: memory_entries matches {recorded} file(s)"));
    }
}

fn count_files(directory: &std::path::Path, extension: Option<&str>) -> u64 {
    let mut count = 0;
    let mut stack = vec![directory.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if extension.is_none_or(|extension| {
                path.extension().and_then(|ext| ext.to_str()) == Some(extension)
            }) {
                count += 1;
            }
        }
    }
    count
}

fn print_report(report: &Report) {
    println!("sbot doctor");
    for line in &report.ok {
        println!("  ok    {line}");
    }
    for line in &report.warnings {
        println!("  warn  {line}");
    }
    for line in &report.errors {
        println!("  ERROR {line}");
    }
    println!(
        "\n{} ok, {} warning(s), {} error(s)",
        report.ok.len(),
        report.warnings.len(),
        report.errors.len()
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::SbotConfig;
    use crate::storage::Database;
    use crate::vfs::Vfs;
    use crate::vfs::path::VfsPath;
    use crate::vfs::quota::Limits;
    use crate::vfs::sqlite::VfsStore;

    /// 建一个可用的临时实例：配置、密钥、数据库与 VFS 种子齐全。
    fn setup() -> (tempfile::TempDir, SbotPaths, Arc<MasterKey>) {
        let dir = tempfile::tempdir().unwrap();
        let paths = SbotPaths::from_home(dir.path());
        std::fs::create_dir_all(paths.home()).unwrap();
        std::fs::write(
            paths.config_file(),
            toml::to_string(&SbotConfig::default()).unwrap(),
        )
        .unwrap();
        let key = Arc::new(MasterKey::load_or_create(paths.key_file()).unwrap());
        let db = Arc::new(Database::open(paths.data_dir().join("data.db")).unwrap());
        db.ensure_key(&key).unwrap();
        let vfs = VfsStore::open(
            Arc::clone(&db),
            Arc::clone(&key),
            paths.data_dir(),
            Limits::default(),
        )
        .unwrap();
        drop(vfs);
        (dir, paths, key)
    }

    #[test]
    fn healthy_instance_has_no_errors() {
        let (_dir, paths, _key) = setup();
        let report = collect(&paths, None);
        assert!(
            report.errors.is_empty(),
            "unexpected errors: {:?}",
            report.errors
        );
        assert!(
            report
                .ok
                .iter()
                .any(|line| line.contains("canary matches the master key"))
        );
        assert!(
            report
                .ok
                .iter()
                .any(|line| line.starts_with("database: schema v"))
        );
    }

    #[test]
    fn detects_canary_mismatch() {
        let (_dir, paths, _key) = setup();
        // 用一个同格式但不同的密钥覆盖 key：canary 必须报错。
        std::fs::write(
            paths.key_file(),
            format!("sbot-key-v1:{}\n", hex::encode([9u8; 32])),
        )
        .unwrap();
        let report = collect(&paths, None);
        assert!(
            report
                .errors
                .iter()
                .any(|line| line.contains("canary does NOT match")),
            "errors: {:?}",
            report.errors
        );
    }

    #[test]
    fn reports_memory_index_drift() {
        let (_dir, paths, key) = setup();
        let db = Arc::new(Database::open(paths.data_dir().join("data.db")).unwrap());
        let vfs = VfsStore::open(
            Arc::clone(&db),
            Arc::clone(&key),
            paths.data_dir(),
            Limits::default(),
        )
        .unwrap();

        let index = VfsPath::parse("/MEMORY.md").unwrap();
        let _ = vfs.read_lines(&index, None, None).unwrap();
        vfs.write_text(
            &index,
            "- [Alpha](memory/alpha.md) — present\n\
             - [Missing](memory/missing.md) — absent\n",
            None,
        )
        .unwrap();
        let alpha = VfsPath::parse("/memory/alpha.md").unwrap();
        vfs.write_text(&alpha, "alpha\n", None).unwrap();
        let beta = VfsPath::parse("/memory/beta.md").unwrap();
        vfs.write_text(&beta, "beta\n", None).unwrap();
        drop(vfs);

        let report = collect(&paths, None);
        let warnings = report.warnings.join("\n");
        assert!(warnings.contains("missing file"), "warnings: {warnings}");
        assert!(
            warnings.contains("beta"),
            "orphan file should be reported: {warnings}"
        );
    }
}
