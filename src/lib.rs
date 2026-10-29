//! ScapegoatBot 库入口。
//!
//! 二进制 `sbot` 只是薄壳（见 `main.rs`）；把模块放进库是为了：
//! - 集成测试可以直接调用 provider / agent / VFS；
//! - 未来 `sbot doctor` 等工具能复用同一套实现。

// Agent 循环在 Phase 3 接入消息队列；Phase 2 只提供实现与单元测试。
#[allow(dead_code)]
pub mod agent;
pub mod app;
pub mod cli;
pub mod config;
pub mod ipc;
pub mod paths;
// Provider 在 Phase 3 由 runner 构造；Phase 2 已完成三协议与流解析。
#[allow(dead_code)]
pub mod providers;
// 闸在 Phase 5 切到 enforce；Phase 2 已接入 agent loop（shadow）。
#[allow(dead_code)]
pub mod security;
pub mod storage;
// VFS 的完整接口（读写/编辑/复制/回收站/二进制）在 Phase 4 由文件工具调用；
// 在此之前生产路径只用到 `usage`，其余方法只被单元测试覆盖，因此整体允许 dead_code。
#[allow(dead_code)]
pub mod vfs;
