//! 路径布局：所有运行状态集中在 `~/.sbot`（可用环境变量 `SBOT_HOME` 覆盖，便于测试与多实例隔离）。
//!
//! 设计依据：`.agents/plan/2026-10-28-rebuild-design.md` §2 / §6.6 / §9.1。
//!
//! 这里的职责只有一个：把“逻辑名”翻译成宿主路径，并保证目录存在。
//! 它不读取配置、不创建数据库，避免与 `config` / `storage` 形成循环依赖。

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

/// `~/.sbot` 的目录布局。
///
/// 进程内只有一个实例，所有路径都由它派生，方便 `sbot backup` 与 `sbot doctor`
/// 以“复制整个目录即迁移”为前提工作（设计 §13）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SbotPaths {
    /// 状态根目录，默认 `~/.sbot`。
    home: PathBuf,
}

impl SbotPaths {
    /// 发现状态根目录。
    ///
    /// 优先级：`SBOT_HOME` 环境变量 > 用户主目录下的 `.sbot`。
    /// 环境变量是测试与多实例调试的逃生口；正常部署不需要设置。
    pub fn discover() -> Result<Self> {
        if let Some(raw) = std::env::var_os("SBOT_HOME") {
            let raw = PathBuf::from(raw);
            return Ok(Self::from_home(raw));
        }

        let home = dirs::home_dir()
            .context("cannot locate the user home directory; set SBOT_HOME to override")?;
        Ok(Self::from_home(home.join(".sbot")))
    }

    /// 用显式目录构造，主要供测试使用。
    pub fn from_home(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// 状态根目录。
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// 主配置文件 `sbot.toml`。
    pub fn config_file(&self) -> PathBuf {
        self.home.join("sbot.toml")
    }

    /// 模型定义文件 `models.json`；启动加载与 `sbot doctor` 使用。
    #[allow(dead_code)]
    pub fn models_file(&self) -> PathBuf {
        self.home.join("models.json")
    }

    /// 凭据文件 `auth.json`（不挂载 VFS）。
    #[allow(dead_code)]
    pub fn auth_file(&self) -> PathBuf {
        self.home.join("auth.json")
    }

    /// 主密钥文件 `key`（内容加密的唯一秘密）。
    pub fn key_file(&self) -> PathBuf {
        self.home.join("key")
    }

    /// 数据目录（SQLite、消息 JSONL 等）；配置里的 `[storage].path` 默认就是它。
    pub fn data_dir(&self) -> PathBuf {
        self.home.join("data")
    }

    /// 运行时目录：单实例锁、PID 文件。
    pub fn run_dir(&self) -> PathBuf {
        self.home.join("run")
    }

    /// 单实例锁文件。
    pub fn instance_lock(&self) -> PathBuf {
        self.run_dir().join("instance.lock")
    }

    /// 日志目录。
    pub fn logs_dir(&self) -> PathBuf {
        self.home.join("logs")
    }

    /// 共享技能目录；`Skill` 工具与 curator 使用。
    #[allow(dead_code)]
    pub fn skills_dir(&self) -> PathBuf {
        self.home.join("skills")
    }

    /// 自定义子代理定义目录；`Agent` 工具使用。
    #[allow(dead_code)]
    pub fn agents_dir(&self) -> PathBuf {
        self.home.join("agents")
    }

    /// ESTOP 哨兵：存在即暂停新回合（设计 §2.3）。
    pub fn estop_file(&self) -> PathBuf {
        self.home.join("ESTOP")
    }

    /// 安全停机哨兵：存在即处于 `hold`（设计 §5.9 / §5.10）。
    pub fn security_hold_file(&self) -> PathBuf {
        self.home.join("SECURITY_HOLD")
    }

    /// 创建布局所需的目录。
    ///
    /// 只创建目录，不创建文件：配置、密钥、数据库由各自模块负责，
    /// 避免“先创建空文件再判断是否首次初始化”这类歧义。
    pub fn ensure_layout(&self) -> Result<()> {
        for dir in [
            self.home.clone(),
            self.data_dir(),
            self.run_dir(),
            self.logs_dir(),
        ] {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("cannot create directory {}", dir.display()))?;
        }
        Ok(())
    }

    /// 展开配置里的路径字符串。
    ///
    /// 规则（设计 §9.1）：
    /// - `~/.sbot/...` 映射到本实例的根目录（仍尊重 `SBOT_HOME`）；
    /// - `~/...` 映射到用户主目录；
    /// - 其他相对路径按 `base` 解析；
    /// - 结果做词法规范化（去掉 `.` 与可消解的 `..`），不解析符号链接。
    pub fn resolve(&self, raw: &str, base: &Path) -> PathBuf {
        let expanded = if let Some(rest) = raw.strip_prefix("~/.sbot") {
            // `~/.sbot` 是逻辑根：即便用户用 SBOT_HOME 改了位置，也不该写回真实主目录。
            let rest = rest.trim_start_matches(['/', '\\']);
            if rest.is_empty() {
                self.home.clone()
            } else {
                self.home.join(rest)
            }
        } else if raw == "~" {
            dirs::home_dir().unwrap_or_else(|| self.home.clone())
        } else if let Some(rest) = raw.strip_prefix("~/") {
            dirs::home_dir()
                .unwrap_or_else(|| self.home.clone())
                .join(rest)
        } else {
            let path = PathBuf::from(raw);
            if path.is_absolute() {
                path
            } else {
                base.join(path)
            }
        };

        normalize(&expanded)
    }
}

/// 词法规范化路径：折叠 `.`、消解 `..`、去除多余分隔符。
///
/// 不做 I/O，不解析符号链接——VFS 有自己的规范化逻辑（设计 §6.3），
/// 这里只用于宿主路径，避免配置里的 `./data/../data` 产生两个不同字符串。
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // 只有上一步是可弹出的普通目录时才弹出；否则保留 `..`。
                if out.components().next_back().is_some_and(|last| {
                    !matches!(
                        last,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                }) {
                    out.pop();
                } else {
                    out.push(component.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_home_maps_to_sbot_home() {
        let paths = SbotPaths::from_home("/tmp/sbot-home");
        assert_eq!(
            paths.resolve("~/.sbot/models.json", Path::new("/tmp/sbot-home")),
            PathBuf::from("/tmp/sbot-home/models.json")
        );
    }

    #[test]
    fn relative_paths_resolve_against_base() {
        let paths = SbotPaths::from_home("/tmp/sbot-home");
        assert_eq!(
            paths.resolve("./data", Path::new("/tmp/sbot-home")),
            PathBuf::from("/tmp/sbot-home/data")
        );
    }

    #[test]
    fn normalize_folds_dots() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
    }

    #[test]
    fn normalize_keeps_leading_parent() {
        assert_eq!(normalize(Path::new("../x")), PathBuf::from("../x"));
    }
}
