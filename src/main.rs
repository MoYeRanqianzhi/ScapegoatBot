//! ScapegoatBot 入口。
//!
//! 只做三件事：初始化日志、解析 CLI、分派到 `app` 或 `cli`。
//! 任何业务逻辑都不应写在 `main.rs` 里。

mod app;
mod cli;
mod config;
mod ipc;
mod paths;
mod storage;
// VFS 的完整接口（读写/编辑/复制/回收站/二进制）在 Phase 4 由文件工具调用；
// 在此之前生产路径只用到 `usage`，其余方法只被单元测试覆盖，因此整体允许 dead_code。
#[allow(dead_code)]
mod vfs;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    cli::run().await
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
