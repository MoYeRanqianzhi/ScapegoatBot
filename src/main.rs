//! ScapegoatBot 入口。
//!
//! 只做三件事：初始化日志、解析 CLI、分派到 `cli::run`。
//! 任何业务逻辑都不应写在 `main.rs` 里。

use anyhow::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    scapegoat_bot::cli::run().await
}

/// 初始化日志。
///
/// 默认 `info`；用 `RUST_LOG` 覆盖（例如 `RUST_LOG=sbot=debug`）。
/// 输出到 stderr：`start` 时 stdout 留给人类面提示，日志不混进去。
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}
