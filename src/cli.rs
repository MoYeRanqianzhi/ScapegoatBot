// src/cli.rs
// 命令行参数定义模块
// 使用 clap derive 模式定义 sbot 可执行文件的所有子命令与参数
// 支持 start / stop / restart / ai / status / chat 等顶层子命令

use clap::{Parser, Subcommand};

/// ScapegoatBot 的顶层命令行接口
/// 通过 `sbot --help` 查看所有可用命令
#[derive(Parser, Debug)]
#[command(name = "sbot", version, about = "ScapegoatBot — QQ 替身 Agent")]
pub struct Cli {
    /// 配置文件路径（默认为当前目录下的 config.toml）
    #[arg(short, long, default_value = "config.toml", global = true)]
    pub config: String,

    /// 可选的子命令；若未提供，则打印帮助信息
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// 顶层子命令枚举
/// 每个变体对应 sbot 的一个主要操作
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// 启动 sbot 守护进程
    Start {
        /// 以后台守护进程模式运行（detach 到后台）
        #[arg(short, long)]
        daemon: bool,
    },

    /// 停止当前运行中的 sbot 实例
    Stop,

    /// 重启 sbot（等价于依次执行 stop + start）
    Restart {
        /// 以后台守护进程模式运行（detach 到后台）
        #[arg(short, long)]
        daemon: bool,
    },

    /// 控制 AI Agent 的开关（启动 / 停止 / 重启）
    Ai {
        /// AI Agent 的子操作
        #[command(subcommand)]
        action: AiAction,
    },

    /// 查看 sbot 的运行状态（是否在线、AI Agent 是否激活等）
    Status,

    /// 向 AI Agent 发送一条自然语言指令
    /// 可选地通过 --group / --private 限定本轮允许发送的目标范围
    Chat {
        /// 自然语言指令内容（例如 "帮我在群里回复xxx"）
        message: String,

        /// 本轮限定的群白名单 ID 列表
        /// 必须是全局发送白名单 (send_groups) 的子集，超出部分会被忽略
        #[arg(long)]
        group: Vec<i64>,

        /// 本轮限定的私聊白名单 QQ 号列表
        /// 必须是全局发送白名单 (send_privates) 的子集，超出部分会被忽略
        #[arg(long)]
        private: Vec<i64>,
    },
}

/// AI Agent 子命令枚举
/// 用于 `sbot ai <action>` 的二级操作
#[derive(Subcommand, Debug)]
pub enum AiAction {
    /// 启动 AI Agent（开始处理消息并自动回复）
    Start,

    /// 停止 AI Agent（消息收集仍继续，仅暂停 AI 回复）
    Stop,

    /// 重启 AI Agent（先停止再启动，刷新内部状态）
    Restart,
}
