//! ScapegoatBot 库入口。
//!
//! 二进制 `sbot` 只是薄壳（见 `main.rs`）；把模块放进库是为了：
//! - 集成测试可以直接调用 provider / agent / VFS；
//! - 未来 `sbot doctor` 等工具能复用同一套实现。

// Agent 循环：内/外循环、事件流、steering 与工具执行。
#[allow(dead_code)]
pub mod agent;
pub mod app;
// 运行预算：每日 token/USD、回合上限与 guard 花费。
pub mod budget;
pub mod cli;
pub mod config;
pub mod ipc;
// MCP 可选，默认关闭；启用时由 app 连接 stdio 服务器。
#[allow(dead_code)]
pub mod mcp;
// 后台复盘：记忆候选与 /STYLE.md；只在空闲时运行。
pub mod memory;
pub mod monitor;
pub mod paths;
// 提示词：静态前缀 + 动态 hooks + 每回合 runtime 上下文。
#[allow(dead_code)]
pub mod prompt;
// Provider：Anthropic Messages、OpenAI Responses、Chat Completions 与 SSE。
#[allow(dead_code)]
pub mod providers;
// 安全闸：规则层 + Jev 分类器；shadow / enforce 由配置决定。
#[allow(dead_code)]
pub mod security;
pub mod storage;
// 工具层：注册表、文件/API/记忆/技能/网络/监视器与动态视图。
#[allow(dead_code)]
pub mod tools;
// QQ 接入：事件归一化、消息格式、准入过滤与断线补消息。
#[allow(dead_code)]
pub mod qq;
#[allow(dead_code)]
pub mod session;
// 子代理：定义、实例与生命周期；由 `Agent` 工具驱动。
#[allow(dead_code)]
pub mod swarm;

// VFS：路径校验、SQLite 后端、加密 blob、回收站与配额；文件工具调用。
#[allow(dead_code)]
pub mod vfs;
