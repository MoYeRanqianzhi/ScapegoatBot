//! ScapegoatBot 入口。
//!
//! 只做三件事：初始化日志、解析 CLI、分派到 `cli::run`。
//! 任何业务逻辑都不应写在 `main.rs` 里。

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    // 默认 `info`，`RUST_LOG` 覆盖；写到 stderr，写出前遮蔽凭据。
    scapegoat_bot::logging::init();
    scapegoat_bot::cli::run().await
}
