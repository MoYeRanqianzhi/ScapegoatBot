//! ScapegoatBot 库入口。
//!
//! 二进制 `sbot` 只是薄壳（见 `main.rs`）；把模块放进库是为了：
//! - 集成测试可以直接调用 provider / agent / VFS；
//! - 守护进程与 `sbot doctor` / `sbot backup` 等本地命令复用同一套实现。
//!
//! 这个库只给 `sbot` 自己与测试用，文档按维护者的需要用 `--document-private-items` 构建
//! （CI 加 `-D warnings`），所以公开项的文档可以链到私有项。

#![allow(rustdoc::private_intra_doc_links)]

/// 全局内存分配器：mimalloc。
///
/// 放在库里而不是 `main.rs`：单元测试、集成测试与 `sbot` 用的是同一个分配器，CI 在各平台
/// 跑测试时也就验证了它。选它是为了性能：musl 自带的 malloc 在多线程下慢得多（发布的
/// Linux 全静态版本用 musl），glibc 的 malloc 在长时间运行的多线程进程里容易碎片化；各平台
/// 统一用一个分配器，性能与内存行为也一致。
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// `API` 工具的 action 注册表：名字、参数 schema、分级与作用域（执行在 `tools/api.rs`）。
pub mod actions;
// Agent 循环：内/外循环、事件流、steering 与工具执行。
pub mod agent;
pub mod app;
// 运行预算：每日 token/USD、回合上限与 guard 花费。
pub mod budget;
pub mod cli;
pub mod config;
pub mod hooks;
pub(crate) mod ipc;
// LLM 公共类型：provider 与 agent 共享的用量、停止原因与拒绝细节。
pub(crate) mod llm;
// 日志初始化：写出前遮蔽凭据（`main.rs` 调用）。
pub mod logging;
// MCP 可选，默认关闭；启用时由 app 连接 stdio 服务器。
pub(crate) mod mcp;
// 图片解码上限、缩图与阻塞线程池：`Read`、`API` 图片动作与 MCP 共用。
pub(crate) mod media;
// 后台复盘：记忆候选与 /STYLE.md；只在空闲时运行。
pub mod memory;
pub(crate) mod monitor;
pub(crate) mod paths;
// 提示词：静态前缀 + 动态 hooks + 每回合 runtime 上下文。
pub(crate) mod prompt;
// Provider：Anthropic Messages、OpenAI Responses、Chat Completions 与 SSE。
pub mod providers;
// 安全闸：规则层 + Jev 分类器；shadow / enforce 由配置决定。
pub mod security;
pub mod storage;
pub(crate) mod transcript;
// 工具层：注册表、文件/API/子代理/技能/网络/监视器与动态视图。
pub mod tools;
// QQ 接入：事件归一化、消息格式与准入过滤。
pub mod qq;
pub mod session;
// 子代理：定义、实例与生命周期；由 `Agent` 工具驱动。
pub mod swarm;

// VFS：路径校验、SQLite 后端、加密 blob、回收站与配额；文件工具调用。
pub mod vfs;
