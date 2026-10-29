//! 运行时装配与生命周期。
//!
//! 设计依据：设计 §2.1 / §2.3 / §10。
//!
//! Phase 0 提供可运行的最小闭环：
//! 配置与模型加载 → 单实例锁 → 主密钥与数据库 → IPC 服务 → 优雅退出。
//!
//! 之后各阶段把 agent loop、VFS、NapCat、工具、监视器等挂到同一个运行时上；
//! 本模块只做**装配**，不承载业务逻辑。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use fs2::FileExt;
use serde_json::json;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::{AuthFile, EncryptionMode, LoadedConfig, ModelsFile, SbotConfig};
use crate::ipc::{self, AiAction, Handler, Request, RequestBody, Response};
use crate::paths::SbotPaths;
use crate::storage::Database;
use crate::storage::crypto::MasterKey;

/// 内置模型清单：随二进制发布，用户 `models.json` 覆盖/追加（设计 §9.3）。
const EMBEDDED_MODELS: &str = include_str!("../assets/models.default.json");

/// 守护进程运行时。
pub struct Runtime {
    /// 路径布局。
    pub paths: SbotPaths,
    /// 已校验的配置；`SetModel` / `SetEffort` 会就地更新。
    config: RwLock<LoadedConfig>,
    /// 模型清单（内置 + 用户覆盖）。
    pub models: ModelsFile,
    /// 凭据（只读，不挂载 VFS）。Phase 2 起被 provider 使用。
    #[allow(dead_code)]
    pub auth: AuthFile,
    /// SQLite。
    pub db: Database,
    /// 主密钥。
    pub key: MasterKey,
    /// LLM 调用开关（`sbot ai stop` 只停 LLM，不收消息）。
    ai_enabled: AtomicBool,
    /// 启动时间用于 uptime。
    started_at: chrono::DateTime<chrono::Utc>,
    /// 关闭信号。
    shutdown: watch::Sender<bool>,
    /// 单实例锁；字段本身只为保持文件句柄存活。
    _instance_lock: InstanceLock,
}

impl Runtime {
    /// 启动运行时：准备目录、加载配置与密钥、打开数据库、启动 IPC。
    pub async fn start(config_path: Option<PathBuf>) -> Result<()> {
        let paths = SbotPaths::discover()?;
        paths.ensure_layout()?;

        let config_path = config_path.unwrap_or_else(|| paths.config_file());
        ensure_default_config(&config_path)?;

        let loaded = SbotConfig::load(&config_path)?;
        let models = load_models(&paths, &loaded)?;

        loaded
            .config
            .validate_with_models(&models)
            .map_err(|errors| anyhow::anyhow!("config validation failed:\n{errors}"))?;

        let auth_path = paths.resolve(&loaded.config.auth_file, &loaded.dir);
        let auth = AuthFile::load_optional(&auth_path)?;
        report_missing_credentials(&loaded.config, &auth);

        // 单实例锁必须早于数据库与 IPC：否则第二个进程会在打开 WAL 时才失败。
        let instance_lock = InstanceLock::acquire(&paths.instance_lock())?;

        let key = MasterKey::load_or_create(paths.key_file())?;
        let data_dir = paths.resolve(&loaded.config.storage.path, &loaded.dir);
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("cannot create storage path {}", data_dir.display()))?;
        let db = Database::open(data_dir.join("sbot.db"))?;
        db.ensure_key(&key)?;

        if loaded.config.storage.encryption == EncryptionMode::None {
            warn!("storage.encryption = \"none\": data is stored in plaintext (debug only)");
        }

        let (shutdown, shutdown_rx) = watch::channel(false);
        let runtime = Arc::new(Self {
            paths: paths.clone(),
            config: RwLock::new(loaded),
            models,
            auth,
            db,
            key,
            ai_enabled: AtomicBool::new(true),
            started_at: chrono::Utc::now(),
            shutdown,
            _instance_lock: instance_lock,
        });

        let endpoint = {
            let config = runtime.config();
            ipc::Endpoint::resolve(&paths, &config.config.ipc)
        };
        info!(endpoint = %endpoint.display(), "starting IPC server");

        let server = tokio::spawn({
            let handler: Arc<dyn Handler> = runtime.clone();
            let endpoint = endpoint.clone();
            async move { ipc::serve(&endpoint, handler, shutdown_rx).await }
        });

        // 等待关闭信号：IPC `Shutdown`、Ctrl-C 或 SIGTERM。
        let mut wait_shutdown = runtime.shutdown.subscribe();
        tokio::select! {
            _ = wait_shutdown.changed() => {}
            _ = tokio::signal::ctrl_c() => {
                info!("received Ctrl-C, shutting down");
            }
            _ = terminate_signal() => {
                info!("received termination signal, shutting down");
            }
        }

        let _ = runtime.shutdown.send(true);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
        info!("sbot stopped");
        Ok(())
    }

    /// 读取配置快照。配置很小，读锁即可；需要长期持有时克隆。
    pub fn config(&self) -> std::sync::RwLockReadGuard<'_, LoadedConfig> {
        self.config.read().expect("config lock poisoned")
    }

    /// 当前 `hold` 状态：`SECURITY_HOLD` 存在即为真（设计 §5.9 / §5.10）。
    fn hold_active(&self) -> bool {
        self.paths.security_hold_file().exists()
    }

    /// `ESTOP` 哨兵：存在即暂停新回合；空文件或损坏内容同样视为暂停（fail safe）。
    fn estop_active(&self) -> bool {
        self.paths.estop_file().exists()
    }

    /// 状态响应。
    fn status_payload(&self) -> serde_json::Value {
        let loaded = self.config();
        let config = &loaded.config;
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "started_at": self.started_at.to_rfc3339(),
            "uptime_secs": (chrono::Utc::now() - self.started_at).num_seconds().max(0),
            "home": self.paths.home().display().to_string(),
            "config_path": loaded.path.display().to_string(),
            "schema_version": self.db.schema_version().ok(),
            "mode": config.agent.mode,
            "model": config.agent.model,
            "thinking_level": config.agent.thinking_level,
            "available_models": config.agent.available_models,
            "ai_enabled": self.ai_enabled.load(Ordering::Relaxed),
            "estop": self.estop_active(),
            "hold": self.hold_active(),
            "session_scope": config.session.scope,
            "guard": {
                "enabled": config.security.guard.enabled,
                "mode": config.security.guard.mode,
                "backend": config.security.guard.backend,
            },
        })
    }

    /// 热切换模型：校验 → 写回 `sbot.toml` → 更新内存。
    fn set_model(&self, model: &str) -> Result<serde_json::Value> {
        let mut loaded = self.config.write().expect("config lock poisoned");
        let config = &mut loaded.config;

        if !config
            .agent
            .available_models
            .iter()
            .any(|item| item == model)
        {
            anyhow::bail!(
                "model {model:?} is not in agent.available_models; available: {:?}",
                config.agent.available_models
            );
        }
        let Some(reference) = self.models.find(model) else {
            anyhow::bail!("model {model:?} is not defined in models.json");
        };

        let previous = std::mem::replace(&mut config.agent.model, model.to_owned());
        if let Err(error) = persist_agent_setting(&loaded.path, "model", model) {
            // 写失败时运行时仍然生效（设计 §9.5）。
            warn!(%error, "cannot persist agent.model; change is runtime-only");
        }
        info!(from = %previous, to = model, "primary model switched");

        Ok(json!({
            "model": model,
            "context_window": reference.model.context_window,
            "thinking_levels": reference.model.thinking_levels,
        }))
    }

    /// 热切换 effort。
    fn set_effort(&self, level: &str) -> Result<serde_json::Value> {
        let mut loaded = self.config.write().expect("config lock poisoned");
        let config = &mut loaded.config;

        let model_name = config.agent.model.clone();
        let Some(reference) = self.models.find(&model_name) else {
            anyhow::bail!("current model {model_name:?} is not defined in models.json");
        };
        if !reference.model.thinking_levels.is_empty()
            && !reference
                .model
                .thinking_levels
                .iter()
                .any(|item| item == level)
        {
            anyhow::bail!(
                "thinking level {level:?} is not supported by {model_name}; supported: {:?}",
                reference.model.thinking_levels
            );
        }

        config.agent.thinking_level = level.to_owned();
        if let Err(error) = persist_agent_setting(&loaded.path, "thinking_level", level) {
            warn!(%error, "cannot persist agent.thinking_level; change is runtime-only");
        }
        Ok(json!({ "thinking_level": level }))
    }

    /// 处理一次请求；幂等在外层 `handle` 完成。
    async fn dispatch(&self, request: Request) -> Response {
        let id = request.id.clone();
        match request.body {
            RequestBody::Status => Response::ok(id, self.status_payload()),

            RequestBody::Shutdown => {
                // 先让响应发出去，再触发关闭。
                let shutdown = self.shutdown.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let _ = shutdown.send(true);
                });
                Response::ok(id, json!({ "stopping": true }))
            }

            RequestBody::Ai { action } => match action {
                AiAction::Start => {
                    self.ai_enabled.store(true, Ordering::Relaxed);
                    Response::ok(id, json!({ "ai_enabled": true }))
                }
                AiAction::Stop => {
                    self.ai_enabled.store(false, Ordering::Relaxed);
                    Response::ok(id, json!({ "ai_enabled": false }))
                }
                AiAction::Restart => {
                    self.ai_enabled.store(true, Ordering::Relaxed);
                    Response::ok(id, json!({ "ai_enabled": true, "restarted": true }))
                }
            },

            RequestBody::Chat { message, scope } => {
                if message.trim().is_empty() {
                    return Response::error(id, "bad_request", "message must not be empty");
                }
                // 控制通道输入加密落库，作为 `control:<id>` 来源注入（设计 §2.3）。
                let sealed =
                    match self
                        .key
                        .seal("control", b"control_inputs.text_enc", message.as_bytes())
                    {
                        Ok(sealed) => sealed,
                        Err(error) => {
                            return Response::error(
                                id,
                                "internal",
                                format!("cannot encrypt input: {error}"),
                            );
                        }
                    };
                match self.db.insert_control_input(&sealed, "cli") {
                    Ok(control_id) => Response::ok(
                        id,
                        json!({
                            "accepted": true,
                            "control_id": control_id,
                            "scope": scope,
                        }),
                    ),
                    Err(error) => {
                        Response::error(id, "internal", format!("cannot store input: {error}"))
                    }
                }
            }

            RequestBody::SetModel { model } => match self.set_model(&model) {
                Ok(payload) => Response::ok(id, payload),
                Err(error) => Response::error(id, "invalid_model", error.to_string()),
            },

            RequestBody::SetEffort { level } => match self.set_effort(&level) {
                Ok(payload) => Response::ok(id, payload),
                Err(error) => Response::error(id, "invalid_effort", error.to_string()),
            },

            RequestBody::Pause => match write_sentinel(&self.paths.estop_file()) {
                Ok(()) => Response::ok(id, json!({ "paused": true })),
                Err(error) => Response::error(id, "internal", error.to_string()),
            },

            RequestBody::Resume => match remove_sentinel(&self.paths.estop_file()) {
                Ok(()) => Response::ok(id, json!({ "paused": false })),
                Err(error) => Response::error(id, "internal", error.to_string()),
            },

            RequestBody::SecurityStatus => Response::ok(
                id,
                json!({
                    "hold": self.hold_active(),
                    "estop": self.estop_active(),
                    "events": [],
                }),
            ),

            RequestBody::SecurityResume => {
                if self.hold_active() {
                    Response::error(
                        id,
                        "not_ready",
                        "clearing a security hold requires the input/output gate (Phase 5)",
                    )
                } else {
                    Response::ok(id, json!({ "hold": false }))
                }
            }

            RequestBody::Backup { output } => Response::error(
                id,
                "not_ready",
                format!("sbot backup arrives in Phase 7 (requested output: {output:?})"),
            ),

            RequestBody::Doctor => {
                Response::error(id, "not_ready", "sbot doctor arrives in Phase 7")
            }
        }
    }
}

#[async_trait::async_trait]
impl Handler for Runtime {
    async fn handle(&self, request: Request) -> Response {
        // 幂等：只对声明了幂等键的写请求生效，命中即原样返回上次结果。
        let idempotency_key = if request.wants_idempotency() {
            request.idempotency_key.clone()
        } else {
            None
        };

        if let Some(key) = idempotency_key.as_deref() {
            match self.db.idempotency_get(key) {
                Ok(Some(stored)) => {
                    return serde_json::from_str(&stored).unwrap_or_else(|_| {
                        Response::error(
                            request.id,
                            "internal",
                            "stored idempotent response is corrupt",
                        )
                    });
                }
                Ok(None) => {}
                Err(error) => warn!(%error, "idempotency lookup failed"),
            }
        }

        let response = self.dispatch(request).await;

        // 只缓存成功结果：失败允许客户端重试。
        if response.ok
            && let Some(key) = idempotency_key
                && let Ok(serialized) = serde_json::to_string(&response)
                    && let Err(error) = self.db.idempotency_put(&key, &serialized) {
                        warn!(%error, "cannot store idempotent response");
                    }

        response
    }
}

// ---------------------------------------------------------------------------
// 启动辅助
// ---------------------------------------------------------------------------

/// 首次启动时写出带默认值的 `sbot.toml`。
///
/// 用 `create_new` 保证不覆盖用户文件；竞态下第二个进程会拿到 `AlreadyExists`，
/// 按“文件已存在”处理即可。
fn ensure_default_config(path: &Path) -> Result<()> {
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

/// 加载模型清单：内置清单 + 用户文件覆盖。
fn load_models(paths: &SbotPaths, loaded: &LoadedConfig) -> Result<ModelsFile> {
    let mut models =
        ModelsFile::parse(EMBEDDED_MODELS).context("embedded models.default.json is invalid")?;

    let user_path = paths.resolve(&loaded.config.models_file, &loaded.dir);
    if user_path.exists() {
        let overlay = ModelsFile::load(&user_path)?;
        models.merge(overlay);
    }

    if let Err(errors) = models.validate() {
        anyhow::bail!(
            "models.json validation failed:\n  - {}",
            errors.join("\n  - ")
        );
    }
    Ok(models)
}

/// 启动时打印缺失凭据；只检查 `available_models` 引用到的 provider。
fn report_missing_credentials(config: &SbotConfig, auth: &AuthFile) {
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

/// 单实例锁。
struct InstanceLock {
    file: std::fs::File,
    path: PathBuf,
}

impl InstanceLock {
    /// 尝试独占锁；失败时读取文件里的 pid，给出可直接行动的提示。
    fn acquire(path: &Path) -> Result<Self> {
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
            // 锁被占用时不能读锁文件本身（Windows 会报共享冲突），
            // 因此 pid 放在旁边一个独立文件里，只用于提示。
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

        // 锁文件本身不写内容：它只承担互斥，内容随时可能被截断。
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
        // 删除锁文件不是必须的（文件本身不再持锁也能重新获取），
        // 但能让 `~/.sbot/run` 保持干净；删除失败不影响正确性。
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_file_name("instance.pid"));
    }
}

/// 写哨兵文件：存在即生效。
fn write_sentinel(path: &Path) -> Result<()> {
    std::fs::write(path, chrono::Utc::now().to_rfc3339())
        .with_context(|| format!("cannot create {}", path.display()))
}

/// 删除哨兵文件；不存在视为成功。
fn remove_sentinel(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(anyhow::anyhow!("cannot remove {}: {error}", path.display())),
    }
}

/// 用 `toml_edit` 原子写回 `[agent]` 下的一个字符串，保留注释与其它内容。
fn persist_agent_setting(path: &Path, key: &str, value: &str) -> Result<()> {
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

    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, document.to_string())
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// Unix 上等待 SIGTERM；Windows 上永远挂起（Ctrl-C 已覆盖）。
#[cfg(unix)]
async fn terminate_signal() {
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
async fn terminate_signal() {
    std::future::pending::<()>().await
}
