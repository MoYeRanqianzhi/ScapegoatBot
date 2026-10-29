//! 命令行客户端。
//!
//! 设计依据：设计 §2.1 / §2.3 / §9.5。
//!
//! CLI 是独立进程：`start` 变成守护进程本身，其余子命令通过 IPC 与控制通道交互。
//! 人类面输出可以本地化；这里用中文，便于主人直接阅读。

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;

use crate::config::{IpcConfig, LoadedConfig, SbotConfig};
use crate::ipc::{self, AiAction, Request, RequestBody, Response};
use crate::paths::SbotPaths;

/// 本地子命令模块：备份与诊断。
pub mod backup;
pub mod doctor;

/// `sbot` 命令行。
#[derive(Debug, Parser)]
#[command(
    name = "sbot",
    version,
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
        /// 指定作用域（`group:<gid>` / `private:<qq>`），省略则由 agent 判断。
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
    /// 备份（Phase 7）。
    Backup {
        /// 输出目录；默认 `~/.sbot/backups/<timestamp>`。
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// 一致性自检（Phase 7）。
    Doctor,
}

#[derive(Debug, Subcommand)]
pub enum AiCommand {
    /// 开始调用 LLM。
    Start,
    /// 停止调用 LLM。
    Stop,
    /// 重启 LLM 调用。
    Restart,
}

#[derive(Debug, Subcommand)]
pub enum SecurityCommand {
    /// 查看 hold 状态与最近事件。
    Status,
    /// 用当前上下文重新过闸并尝试解除 hold。
    Resume,
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
    let loaded = load_config_lenient(paths, config);
    let endpoint = ipc::Endpoint::resolve(paths, &loaded.name);

    match command {
        Command::Start | Command::Backup { .. } | Command::Doctor => {
            unreachable!("local commands are handled before the daemon connection")
        }
        Command::Status => request(&endpoint, Request::new(RequestBody::Status)).await,
        Command::Stop => request(&endpoint, Request::new(RequestBody::Shutdown)).await,
        Command::Restart => restart(&endpoint).await,

        Command::Ai { action } => {
            let action = match action {
                AiCommand::Start => AiAction::Start,
                AiCommand::Stop => AiAction::Stop,
                AiCommand::Restart => AiAction::Restart,
            };
            request(&endpoint, Request::new(RequestBody::Ai { action })).await
        }

        Command::Chat { message, scope } => {
            let request = Request::new(RequestBody::Chat { message, scope });
            // 同一次调用内重试时复用同一个幂等键，避免重复入队。
            let key = request.id.clone();
            request_with_retry(&endpoint, request.with_idempotency(key)).await
        }

        Command::Pause => request(&endpoint, Request::new(RequestBody::Pause)).await,
        Command::Resume => request(&endpoint, Request::new(RequestBody::Resume)).await,

        Command::Security { action } => match action {
            SecurityCommand::Status => {
                request(&endpoint, Request::new(RequestBody::SecurityStatus)).await
            }
            SecurityCommand::Resume => {
                request(&endpoint, Request::new(RequestBody::SecurityResume)).await
            }
        },

        Command::Model { action } => match action {
            ModelAction::Set { model } => {
                let request = Request::new(RequestBody::SetModel { model });
                let key = request.id.clone();
                request_with_retry(&endpoint, request.with_idempotency(key)).await
            }
            ModelAction::List => {
                // 列表不经过 IPC：模型清单是静态配置，读文件即可。
                let status = request_payload(&endpoint, Request::new(RequestBody::Status)).await?;
                print_value(&status)?;
                Ok(())
            }
        },

        Command::Effort { action } => match action {
            EffortAction::Set { level } => {
                let request = Request::new(RequestBody::SetEffort { level });
                let key = request.id.clone();
                request_with_retry(&endpoint, request.with_idempotency(key)).await
            }
            EffortAction::List => {
                let status = request_payload(&endpoint, Request::new(RequestBody::Status)).await?;
                print_value(&status)?;
                Ok(())
            }
        },
    }
}

/// 客户端侧只关心 IPC 端点名；配置缺失或损坏时不阻止连接尝试。
///
/// 配置损坏的完整校验由守护进程在启动时完成；CLI 只需一个名字，
/// 强行在这里报错会让 `sbot status` 在配置出错时也无法使用——那恰恰是最需要它的时候。
fn load_config_lenient(paths: &SbotPaths, explicit: Option<PathBuf>) -> ClientConfig {
    let path = explicit.unwrap_or_else(|| paths.config_file());
    match SbotConfig::load(&path) {
        Ok(LoadedConfig { config, .. }) => ClientConfig { name: config.ipc },
        Err(_) => ClientConfig {
            name: IpcConfig::default(),
        },
    }
}

struct ClientConfig {
    name: IpcConfig,
}

/// 发送一次请求并打印结果。
async fn request(endpoint: &ipc::Endpoint, request: Request) -> Result<()> {
    let payload = request_payload(endpoint, request).await?;
    print_value(&payload)
}

/// 发送请求，成功时返回 payload；失败时把错误码与说明一起抛出。
async fn request_payload(endpoint: &ipc::Endpoint, request: Request) -> Result<serde_json::Value> {
    let mut client = ipc::Client::connect(endpoint).await?;
    let response = client.request(&request).await?;
    into_payload(response)
}

/// 带一次重试的请求：只在传输层失败时重试，且复用同一个幂等键。
async fn request_with_retry(endpoint: &ipc::Endpoint, request: Request) -> Result<()> {
    let payload = match request_payload(endpoint, request.clone()).await {
        Ok(payload) => payload,
        Err(first) => {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            match request_payload(endpoint, request).await {
                Ok(payload) => payload,
                Err(_) => return Err(first),
            }
        }
    };
    print_value(&payload)
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

/// 重启：先停，等端口关闭，再以分离进程拉起 `sbot start`。
async fn restart(endpoint: &ipc::Endpoint) -> Result<()> {
    // 停止失败（例如本来就没在运行）不阻止继续拉起。
    let stop = request_payload(endpoint, Request::new(RequestBody::Shutdown)).await;
    if stop.is_err() {
        eprintln!("提示：没有找到正在运行的实例，直接启动新进程。");
    }

    // 等待旧进程释放 IPC 与单实例锁，最多 15 秒。
    for _ in 0..150 {
        if ipc::Client::connect(endpoint).await.is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let exe = std::env::current_exe().context("cannot locate the sbot executable")?;
    let mut command = std::process::Command::new(exe);
    command.arg("start");
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
    fn parses_global_config_path() {
        let cli = Cli::parse_from(["sbot", "--config", "/tmp/x.toml", "status"]);
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/x.toml")));
        assert!(matches!(cli.command, Command::Status));
    }
}
