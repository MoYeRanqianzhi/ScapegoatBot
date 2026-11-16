//! 命令行客户端。
//!
//! 设计依据：设计 §2.1 / §2.3 / §9.5。
//!
//! CLI 是独立进程：`start` 变成守护进程本身，`backup` / `doctor` / `docs` 在本地完成，
//! 其余子命令通过 IPC 与控制通道交互。
//! 人类面输出可以本地化；这里用中文，便于主人直接阅读。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;

use crate::config::{IpcConfig, LoadedConfig, SbotConfig};
use crate::ipc::{self, AiAction, Request, RequestBody, Response};
use crate::paths::SbotPaths;

/// 本地子命令模块：备份、诊断与内嵌文档。
pub mod backup;
pub mod docs;
pub mod doctor;
// `sbot --version` 的构建信息（目标平台、CPU 等级）。
mod version;

/// `sbot` 命令行。
#[derive(Debug, Parser)]
#[command(
    name = "sbot",
    version,
    long_version = version::long_version(),
    about = "ScapegoatBot — 一个 QQ Agent，代替用户像真人一样回复消息",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// 配置文件路径；默认 `~/.sbot/sbot.toml`。
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 启动守护进程（前台运行）。
    Start,
    /// 停止守护进程。
    Stop,
    /// 重启守护进程（停止后重新拉起）。
    Restart,
    /// 查看运行状态。
    Status,
    /// 暂停 / 恢复 LLM 调用；消息接收与存储不中断。
    Ai {
        #[command(subcommand)]
        action: AiCommand,
    },
    /// 从控制通道给 agent 发一句话。
    Chat {
        /// 消息内容。
        message: String,
        /// 这句话针对的会话（`group:<gid>` / `private:<qq>`），写进 `owner_context` 的
        /// `scope` 属性；省略则由 agent 判断。格式错误时守护进程拒绝请求。
        #[arg(long)]
        scope: Option<String>,
    },
    /// 暂停新回合（写 `ESTOP`）。
    Pause,
    /// 恢复新回合（删除 `ESTOP`）。
    Resume,
    /// 安全状态与恢复。
    Security {
        #[command(subcommand)]
        action: SecurityCommand,
    },
    /// `[gate]` 待确认请求：列出、批准、拒绝。
    Gate {
        #[command(subcommand)]
        action: GateCommand,
    },
    /// 模型热切换。
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
    /// effort 热切换。
    Effort {
        #[command(subcommand)]
        action: EffortAction,
    },
    /// 备份：SQLite 快照 + 文件归档（包含主密钥）。
    Backup {
        /// 输出目录；默认 `~/.sbot/backups/<timestamp>`。
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// 只读一致性自检：配置、密钥、索引、记忆对账。
    Doctor,
    /// 打印编在二进制里的使用文档（部署、使用、运维）；不带参数列出全部。
    Docs {
        /// skill 名，例如 `sbot-deploy`。
        skill: Option<String>,
        /// 该 skill 的参考文件名，例如 `configuration`。
        reference: Option<String>,
        /// 不打印，把整套文件写进这个目录（每个 skill 一个子目录）。
        #[arg(long, value_name = "DIR", conflicts_with_all = ["skill", "reference"])]
        export: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AiCommand {
    /// 开始调用 LLM。
    Start,
    /// 停止调用 LLM。
    Stop,
}

#[derive(Debug, Subcommand)]
pub enum SecurityCommand {
    /// 查看 hold 状态与最近事件。
    Status,
    /// 用当前上下文重新过闸并尝试解除 hold。
    Resume,
}

#[derive(Debug, Subcommand)]
pub enum GateCommand {
    /// 列出等待确认的变更动作。
    Pending,
    /// 批准一个请求；批准后 120 秒内可重试一次。
    Approve {
        /// 请求 id：`sbot gate pending` 列出的 `id`。
        id: String,
    },
    /// 拒绝一个请求。
    Reject {
        /// 请求 id：`sbot gate pending` 列出的 `id`。
        id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ModelAction {
    /// 切换主模型，例如 `anthropic/claude-opus-5-5`。
    Set { model: String },
    /// 列出可用模型。
    List,
}

#[derive(Debug, Subcommand)]
pub enum EffortAction {
    /// 切换 effort，例如 `high`。
    Set { level: String },
    /// 列出当前模型支持的 effort。
    List,
}

/// 入口：解析参数并分派。
pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    // 文档在定位实例目录之前处理：它不需要 `~/.sbot`，连主目录都找不到的环境里也要能用。
    if let Command::Docs {
        skill,
        reference,
        export,
    } = &cli.command
    {
        return docs::run(skill.as_deref(), reference.as_deref(), export.as_deref());
    }
    let paths = SbotPaths::discover()?;

    match cli.command {
        // `start` 是守护进程本身，不连接 IPC。
        Command::Start => crate::app::Runtime::start(cli.config).await,
        // 备份与诊断直接读本地文件：守护进程没运行时也必须可用，
        // 尤其是 `doctor`——它正是用来排查“守护进程起不来”的。
        Command::Backup { output } => {
            backup::run(cli.config, output)?;
            Ok(())
        }
        Command::Doctor => doctor::run(cli.config),
        command => run_remote(command, &paths, cli.config).await,
    }
}

/// 通过 IPC 与控制通道执行的子命令。
async fn run_remote(command: Command, paths: &SbotPaths, config: Option<PathBuf>) -> Result<()> {
    let daemon = Daemon {
        endpoints: ipc::Endpoint::candidates(paths, &client_ipc_config(paths, config.as_deref())),
        home: paths.home().to_path_buf(),
    };

    match command {
        Command::Start | Command::Backup { .. } | Command::Doctor | Command::Docs { .. } => {
            unreachable!("local commands are handled before the daemon connection")
        }
        Command::Status => daemon.run(RequestBody::Status).await,
        Command::Stop => daemon.run(RequestBody::Shutdown).await,
        Command::Restart => restart(&daemon, config.as_deref()).await,

        Command::Ai { action } => {
            let action = match action {
                AiCommand::Start => AiAction::Start,
                AiCommand::Stop => AiAction::Stop,
            };
            daemon.run(RequestBody::Ai { action }).await
        }

        Command::Chat { message, scope } => {
            daemon.run_once(RequestBody::Chat { message, scope }).await
        }

        Command::Pause => daemon.run(RequestBody::Pause).await,
        Command::Resume => daemon.run(RequestBody::Resume).await,

        Command::Security { action } => match action {
            SecurityCommand::Status => daemon.run(RequestBody::SecurityStatus).await,
            SecurityCommand::Resume => daemon.run(RequestBody::SecurityResume).await,
        },

        Command::Gate { action } => match action {
            GateCommand::Pending => daemon.run(RequestBody::GatePending).await,
            GateCommand::Approve { id: approval } => {
                daemon.run(RequestBody::GateApprove { approval }).await
            }
            GateCommand::Reject { id: approval } => {
                daemon.run(RequestBody::GateReject { approval }).await
            }
        },

        Command::Model { action } => match action {
            ModelAction::Set { model } => daemon.run_once(RequestBody::SetModel { model }).await,
            // 经守护进程查询：它持有合并后的模型清单与热切换后的当前模型。
            ModelAction::List => daemon.run(RequestBody::ModelList).await,
        },

        Command::Effort { action } => match action {
            EffortAction::Set { level } => daemon.run_once(RequestBody::SetEffort { level }).await,
            EffortAction::List => daemon.run(RequestBody::EffortList).await,
        },
    }
}

/// 控制通道的另一端：可能的端点，以及这次调用所属的实例目录。
///
/// 请求都经这里构造，实例目录不会漏带：端点名只由 `[ipc].name` 决定，两个实例用了同一个
/// 名字时，守护进程靠请求里的目录认出“这条命令不是给我的”。
struct Daemon {
    /// 依次尝试的端点（见 [`ipc::Endpoint::candidates`]）。
    endpoints: Vec<ipc::Endpoint>,
    home: PathBuf,
}

impl Daemon {
    /// 发一条请求；成功返回 payload，失败把错误码与说明一起抛出。
    ///
    /// 逐个端点试：连不上的跳过；连上了但对面是另一个实例（`wrong_instance`）的也跳过——
    /// 那不是要找的守护进程，它可能正好在下一个位置。全都不行时报第一个失败，并附上其余的。
    async fn send(&self, request: Request) -> Result<serde_json::Value> {
        let mut failures: Vec<anyhow::Error> = Vec::new();
        for endpoint in &self.endpoints {
            let response = match exchange(endpoint, &request).await {
                Ok(response) => response,
                Err(error) => {
                    failures.push(error);
                    continue;
                }
            };
            let other_instance = response
                .error
                .as_ref()
                .is_some_and(|error| error.code == "wrong_instance");
            match into_payload(response) {
                Err(error) if other_instance => failures.push(error),
                result => return result,
            }
        }
        let mut failures = failures.into_iter();
        let first = failures
            .next()
            .unwrap_or_else(|| anyhow::anyhow!("no control endpoint to try"));
        Err(failures.fold(first, |error, next| {
            error.context(format!("also tried: {next:#}"))
        }))
    }

    /// 发一条请求并打印结果。
    async fn run(&self, body: RequestBody) -> Result<()> {
        let payload = self.send(Request::new(body).for_home(&self.home)).await?;
        print_value(&payload)
    }

    /// 发一条有副作用的请求并打印结果。传输层失败时重试一次，两次用同一个幂等键：
    /// 第一次其实已经送达时，守护进程回放原结果，不会执行两遍。
    async fn run_once(&self, body: RequestBody) -> Result<()> {
        let request = Request::new(body).for_home(&self.home);
        let key = request.id.clone();
        let request = request.with_idempotency(key);
        let payload = match self.send(request.clone()).await {
            Ok(payload) => payload,
            Err(first) => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                match self.send(request).await {
                    Ok(payload) => payload,
                    Err(_) => return Err(first),
                }
            }
        };
        print_value(&payload)
    }
}

/// 客户端侧只关心 IPC 端点名；配置缺失或损坏时不阻止连接尝试。
///
/// 配置损坏的完整校验由守护进程在启动时完成；CLI 只需一个名字，
/// 强行在这里报错会让 `sbot status` 在配置出错时也无法使用——那恰恰是最需要它的时候。
fn client_ipc_config(paths: &SbotPaths, explicit: Option<&Path>) -> IpcConfig {
    let path = explicit.map_or_else(|| paths.config_file(), Path::to_path_buf);
    match SbotConfig::load(&path) {
        Ok(LoadedConfig { config, .. }) => config.ipc,
        Err(_) => IpcConfig::default(),
    }
}

/// 连上一个端点，发一条请求，取回响应。
async fn exchange(endpoint: &ipc::Endpoint, request: &Request) -> Result<Response> {
    let mut client = ipc::Client::connect(endpoint).await?;
    client.request(request).await
}

fn into_payload(response: Response) -> Result<serde_json::Value> {
    if response.ok {
        Ok(response.payload.unwrap_or(serde_json::Value::Null))
    } else {
        let error = response
            .error
            .unwrap_or_else(|| ipc::ApiError::new("unknown", "no error details"));
        Err(anyhow::anyhow!("[{}] {}", error.code, error.message))
    }
}

fn print_value(value: &serde_json::Value) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).context("cannot render response")?
    );
    Ok(())
}

/// 重启：先停，等旧进程放开控制通道，再以分离进程拉起 `sbot start`。
///
/// 新进程不受任何服务管理器管理，输出也不留（stdout / stderr 接到空设备）。守护进程由
/// systemd 之类托管时应该用它的重启命令。
async fn restart(daemon: &Daemon, config: Option<&Path>) -> Result<()> {
    // 停止失败（例如本来就没在运行）不阻止继续拉起。
    let stop = daemon
        .send(Request::new(RequestBody::Shutdown).for_home(&daemon.home))
        .await;
    if stop.is_err() {
        eprintln!("提示：没有找到正在运行的实例，直接启动新进程。");
    }

    // 等待旧进程释放 IPC 与单实例锁，最多 15 秒：问到它不再应答为止。
    for _ in 0..150 {
        let probe = Request::new(RequestBody::Status).for_home(&daemon.home);
        if daemon.send(probe).await.is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let exe = std::env::current_exe().context("cannot locate the sbot executable")?;
    let mut command = start_command(&exe, config);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS：子进程不继承控制台，CLI 退出后继续运行。
        command.creation_flags(0x0000_0008);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 新会话：脱离当前终端的信号。
        command.process_group(0);
    }

    let child = command.spawn().context("cannot start a new sbot process")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "restarted": true,
            "pid": child.id(),
        }))?
    );
    Ok(())
}

/// 拉起守护进程的命令行：`sbot [--config <path>] start`。
///
/// `--config` 必须原样带上：漏了它，新进程会去读默认位置的配置（没有就新建一份默认的），
/// 重启就悄悄变成了“换一份配置启动”。`SBOT_HOME` 等环境变量由子进程继承。
fn start_command(exe: &Path, config: Option<&Path>) -> std::process::Command {
    let mut command = std::process::Command::new(exe);
    if let Some(config) = config {
        command.arg("--config").arg(config);
    }
    command.arg("start");
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_model_set() {
        let cli = Cli::parse_from(["sbot", "model", "set", "anthropic/claude-opus-5-5"]);
        match cli.command {
            Command::Model { action } => match action {
                ModelAction::Set { model } => assert_eq!(model, "anthropic/claude-opus-5-5"),
                _ => panic!("expected set"),
            },
            _ => panic!("expected model command"),
        }
    }

    #[test]
    fn restart_starts_the_daemon_with_the_same_config() {
        let args = |command: std::process::Command| -> Vec<String> {
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(args(start_command(Path::new("sbot"), None)), ["start"]);
        assert_eq!(
            args(start_command(
                Path::new("sbot"),
                Some(Path::new("/etc/sbot/b.toml"))
            )),
            ["--config", "/etc/sbot/b.toml", "start"]
        );
    }

    /// 只回一种结果的处理器。
    struct Fixed(fn(String) -> Response);

    #[async_trait::async_trait]
    impl ipc::Handler for Fixed {
        async fn handle(&self, request: Request) -> Response {
            (self.0)(request.id)
        }
    }

    /// 一个本测试独占的端点；Unix 下套接字放在 `dir` 里。
    fn test_endpoint(dir: &Path, tag: &str) -> ipc::Endpoint {
        #[cfg(windows)]
        {
            let _ = dir;
            ipc::Endpoint::Pipe(format!(
                r"\\.\pipe\sbot-cli-test-{}-{tag}",
                std::process::id()
            ))
        }
        #[cfg(unix)]
        {
            ipc::Endpoint::Socket(dir.join(format!("{tag}.sock")))
        }
    }

    /// 在端点上起一个服务端，等到它能连上。
    async fn serve_fixed(
        endpoint: &ipc::Endpoint,
        reply: fn(String) -> Response,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        tokio::spawn({
            let endpoint = endpoint.clone();
            async move { ipc::serve(&endpoint, std::sync::Arc::new(Fixed(reply)), shutdown).await }
        });
        for _ in 0..200 {
            if ipc::Client::connect(endpoint).await.is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the test server at {} never came up", endpoint.display());
    }

    /// 没有服务的端点、后面是别的实例的端点都跳过，直到找到要找的守护进程；都不是时，
    /// 错误里既有第一个失败，也有后面试过的。
    #[tokio::test]
    async fn the_client_skips_endpoints_that_are_down_or_belong_to_another_instance() {
        let dir = tempfile::tempdir().unwrap();
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let nobody = test_endpoint(dir.path(), "nobody");
        let stranger = test_endpoint(dir.path(), "stranger");
        let ours = test_endpoint(dir.path(), "ours");
        serve_fixed(
            &stranger,
            |id| Response::error(id, "wrong_instance", "this daemon serves /elsewhere"),
            shutdown.clone(),
        )
        .await;
        serve_fixed(
            &ours,
            |id| Response::ok(id, json!({"here": true})),
            shutdown,
        )
        .await;
        let request = || Request::new(RequestBody::Status).for_home(dir.path());

        let daemon = Daemon {
            endpoints: vec![nobody.clone(), stranger.clone(), ours],
            home: dir.path().to_path_buf(),
        };
        assert_eq!(daemon.send(request()).await.unwrap(), json!({"here": true}));

        let daemon = Daemon {
            endpoints: vec![nobody, stranger],
            home: dir.path().to_path_buf(),
        };
        let error = format!("{:#}", daemon.send(request()).await.unwrap_err());
        assert!(error.contains("Is the daemon running?"), "{error}");
        assert!(error.contains("wrong_instance"), "{error}");
        let _ = stop.send(true);
    }

    #[test]
    fn parses_global_config_path() {
        let cli = Cli::parse_from(["sbot", "--config", "/tmp/x.toml", "status"]);
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/x.toml")));
        assert!(matches!(cli.command, Command::Status));
    }
}
