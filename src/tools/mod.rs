//! 工具层：注册表、权限标记与参数解析。
//!
//! 设计依据：设计 §5.1 / §5.4 / §10。
//!
//! 没有 Bash：文件能力由独立的工具补齐；QQ 操作合并为一个 `API` 工具。
//! 本模块只定义契约与注册表，具体工具在 `fs` / `api` / `monitor` 等子模块。
//!
//! 权限标记（[`Permission`]）是**声明**，不是执行点：真正的门禁与 `hold`
//! 检查在 runtime（API 工具与安全闸）；声明用于审计与策略表。

pub mod actions;
pub mod agent;
pub mod api;
pub mod download;
pub mod fs;
pub mod memory;
pub mod monitor;
pub mod net;
pub mod skill;
pub mod views;
pub mod web;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::{ExecutionMode, ToolCall, ToolExecutor, ToolResult};
use crate::providers::ToolSpec;
use crate::vfs::path::ParsedTarget;
use crate::vfs::sqlite::VfsStore;
use crate::vfs::{FileContent, VfsPath};

/// 工具声明的权限。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Permission {
    Read,
    Write,
    Delete,
    Network,
    Send,
    Admin,
}

/// 工具执行上下文。
#[derive(Clone)]
pub struct ToolContext {
    pub vfs: Arc<VfsStore>,
    /// 动态视图（`/messages`、`/inbox`、`/sessions`、`/friends` 等）。
    /// 测试可以没有；真实运行时一定有。
    pub views: Option<Arc<views::Views>>,
    /// 安全闸；`None` 只用于单元测试。
    pub gate: Option<Arc<crate::security::gate::Gate>>,
    /// 本回合的运行时证据（owner intent / origin_chat）；门禁只消费它。
    pub evidence: crate::agent::TurnEvidence,
}

impl ToolContext {
    pub fn new(vfs: Arc<VfsStore>, views: Option<Arc<views::Views>>) -> Self {
        Self {
            vfs,
            views,
            gate: None,
            evidence: crate::agent::TurnEvidence::default(),
        }
    }

    /// 注入回合证据；`API` 的 owner intent 检查读取它。
    pub fn with_evidence(mut self, evidence: crate::agent::TurnEvidence) -> Self {
        self.evidence = evidence;
        self
    }

    /// 注入安全闸；工具据此拒绝 hold 期间的外部访问。
    pub fn with_gate(mut self, gate: Arc<crate::security::gate::Gate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// 当前是否处于 hold：对外工具一律拒绝，避免绕过 `napcat.*` 的限制。
    pub fn hold_active(&self) -> bool {
        self.gate.as_ref().is_some_and(|gate| gate.hold_active())
    }
}

/// 工具接口。
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> Value;
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }
    fn permissions(&self) -> &'static [Permission] {
        &[Permission::Read]
    }
    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolResult;
}

/// 工具注册表。
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, Arc<dyn Tool>>,
    /// 执行上下文；`None` 只用于不依赖 VFS 的测试工具。
    vfs: Option<Arc<VfsStore>>,
    views: Option<Arc<views::Views>>,
    /// 安全闸；`hold` 期间对外工具统一拒绝。
    gate: Option<Arc<crate::security::gate::Gate>>,
    /// 只读 `API` 的替身；docs 等子代理用。
    read_only_api: Option<Arc<dyn Tool>>,
    /// 单次工具结果的字节上限；0 = 不限制。
    max_result_bytes: usize,
}

impl ToolRegistry {
    /// 不带上下文的空注册表（测试用）。
    pub fn empty() -> Self {
        Self::default()
    }

    /// 以 VFS 作为执行上下文的注册表。
    pub fn new(vfs: Arc<VfsStore>, views: Option<Arc<views::Views>>) -> Self {
        Self {
            tools: BTreeMap::new(),
            vfs: Some(vfs),
            views,
            gate: None,
            read_only_api: None,
            max_result_bytes: 0,
        }
    }

    /// 截断超长工具结果，避免一次 `Read`/`Grep` 把上下文撑爆。
    pub fn with_max_result_bytes(mut self, bytes: usize) -> Self {
        self.max_result_bytes = bytes;
        self
    }

    /// 提供只读 `API` 实例；`read_only()` 会用它替换完整版本。
    pub fn with_read_only_api(mut self, tool: Arc<dyn Tool>) -> Self {
        self.read_only_api = Some(tool);
        self
    }

    /// 把 `API` 换成只允许 `api.*` 的实例。
    pub fn read_only(mut self) -> Self {
        if let Some(api) = &self.read_only_api {
            self.tools.insert("API", Arc::clone(api));
        }
        self
    }

    /// 注入安全闸；`hold` 期间 Download / Web* / MCP 会被拒绝。
    pub fn with_gate(mut self, gate: Arc<crate::security::gate::Gate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// 注册一个工具；同名后注册者覆盖先注册者（用于测试替身）。
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name(), tool);
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tools.keys().copied().collect()
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// 复制注册表，只保留 `allowed` 中的工具；上下文（VFS/视图）沿用。
    ///
    /// 子代理用它实现“工具集是父代理的子集”。
    pub fn filtered(&self, allowed: &[&'static str]) -> Self {
        let mut tools = BTreeMap::new();
        for name in allowed {
            if let Some(tool) = self.tools.get(name) {
                tools.insert(*name, Arc::clone(tool));
            }
        }
        Self {
            tools,
            vfs: self.vfs.clone(),
            views: self.views.clone(),
            gate: self.gate.clone(),
            read_only_api: self.read_only_api.clone(),
            max_result_bytes: self.max_result_bytes,
        }
    }

    /// 删除一个工具，返回是否存在（用于子代理去掉 `API` 发送能力）。
    pub fn without(mut self, name: &str) -> Self {
        self.tools.remove(name);
        self
    }

    /// 全部工具声明，顺序稳定（BTreeMap）。
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|tool| ToolSpec {
                name: tool.name().to_owned(),
                description: tool.description().to_owned(),
                input_schema: tool.parameters(),
            })
            .collect()
    }
}

#[async_trait]
impl ToolExecutor for ToolRegistry {
    fn specs(&self) -> Vec<ToolSpec> {
        self.specs()
    }

    fn execution_mode(&self, name: &str) -> ExecutionMode {
        self.get(name)
            .map_or(ExecutionMode::Parallel, |tool| tool.execution_mode())
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        self.execute_with_evidence(call, crate::agent::TurnEvidence::default())
            .await
    }
}

impl ToolRegistry {
    /// 带回合证据执行一次工具调用。
    ///
    /// `ToolExecutor` 的签名不带上下文；runner 用 [`crate::agent::TurnEvidence`]
    /// 包装一层，把 owner intent 与 origin_chat 传进来。
    pub async fn execute_with_evidence(
        &self,
        call: &ToolCall,
        evidence: crate::agent::TurnEvidence,
    ) -> ToolResult {
        let Some(tool) = self.get(&call.name) else {
            return ToolResult::error(format!(
                "Unknown tool {:?}. Available tools: {}",
                call.name,
                self.names().join(", ")
            ));
        };
        // `ToolExecutor` 的签名不带上下文，注册表在构造时固定 VFS。
        let Some(vfs) = self.vfs.clone() else {
            return ToolResult::error("Tool registry has no VFS context.");
        };
        let mut context = ToolContext::new(vfs, self.views.clone());
        context.gate = self.gate.clone();
        context.evidence = evidence;
        let result = tool.execute(&context, call.arguments.clone()).await;
        truncate_result(result, self.max_result_bytes)
    }
}

// ---------------------------------------------------------------------------
// 参数解析
// ---------------------------------------------------------------------------

/// 必填字符串参数。
pub fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolResult> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolResult::error(format!("Missing required parameter {key:?} (string).")))
}

/// 可选字符串参数。
pub fn arg_opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// 可选无符号整数参数。
pub fn arg_opt_u64(args: &Value, key: &str) -> Result<Option<u64>, ToolResult> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| ToolResult::error(format!("Parameter {key:?} must be a number."))),
    }
}

/// 可选布尔参数。
pub fn arg_opt_bool(args: &Value, key: &str) -> Result<bool, ToolResult> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| ToolResult::error(format!("Parameter {key:?} must be a boolean."))),
    }
}

/// 解析目标路径（可带查询串）。
pub fn parse_target(raw: &str) -> Result<ParsedTarget, ToolResult> {
    ParsedTarget::parse(raw).map_err(|error| ToolResult::error(error.to_string()))
}

/// 解析纯路径。
pub fn parse_path(raw: &str) -> Result<VfsPath, ToolResult> {
    VfsPath::parse(raw).map_err(|error| ToolResult::error(error.to_string()))
}

/// 把 VFS 错误转成工具错误。
pub fn vfs_error(error: crate::vfs::VfsError) -> ToolResult {
    ToolResult::error(error.to_string())
}

/// 把读取结果渲染成模型可读文本。
pub fn render_content(path: &VfsPath, content: FileContent) -> String {
    match content {
        FileContent::Text {
            text,
            total_bytes,
            is_partial,
        } => {
            if is_partial {
                format!("{text}\n\n[partial read of {path}; {total_bytes} bytes total]")
            } else {
                text
            }
        }
        FileContent::Metadata(meta) => format!(
            "{} {} ({} bytes{})",
            path,
            meta.kind.as_str(),
            meta.size,
            meta.mime
                .map(|mime| format!(", {mime}"))
                .unwrap_or_default()
        ),
    }
}

/// 按字节上限截断工具结果；按字符边界切，避免破坏 UTF-8。
fn truncate_result(mut result: ToolResult, limit: usize) -> ToolResult {
    if limit == 0 || result.content.len() <= limit {
        return result;
    }
    let mut cut = limit;
    while cut > 0 && !result.content.is_char_boundary(cut) {
        cut -= 1;
    }
    result.content.truncate(cut);
    result.content.push_str(
        "

[result truncated: exceeded tools.max_result_bytes]",
    );
    result
}

/// 工具声明里常见的参数骨架：避免每个工具重复写。
pub fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arg_helpers_report_errors() {
        let args = json!({"path": "/a", "limit": 5, "flag": true});
        assert_eq!(arg_str(&args, "path").unwrap(), "/a");
        assert_eq!(arg_opt_u64(&args, "limit").unwrap(), Some(5));
        assert!(arg_opt_u64(&args, "path").is_err());
        assert!(arg_opt_bool(&args, "flag").unwrap());
        assert!(!arg_opt_bool(&args, "missing").unwrap());
        assert!(arg_str(&args, "missing").is_err());
    }

    #[test]
    fn long_tool_results_are_truncated_on_char_boundaries() {
        let result = ToolResult::ok("你".repeat(10));
        let truncated = truncate_result(result, 10);
        assert!(truncated.content.starts_with("你你你"));
        assert!(truncated.content.contains("result truncated"));
        assert!(std::str::from_utf8(truncated.content.as_bytes()).is_ok());
        // 未超限时原样返回。
        let small = truncate_result(ToolResult::ok("ok"), 10);
        assert_eq!(small.content, "ok");
    }

    #[test]
    fn schemas_are_objects() {
        let schema = object_schema(json!({"path": {"type": "string"}}), &["path"]);
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"][0], "path");
    }
}
