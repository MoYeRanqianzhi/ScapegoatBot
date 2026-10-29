//! `Agent` 工具：子代理的 spawn / message / check / wait / list。
//!
//! 设计依据：设计 §5.7。
//!
//! 工具本身不含执行逻辑；它只把请求转发给 [`crate::swarm::AgentHub`]。
//! 子代理的工具集由类型或自定义定义强制，模型不能通过参数扩大。

use std::sync::Arc;

use async_trait::async_trait;

use super::{Permission, Tool, ToolContext, arg_opt_str, arg_str, object_schema};
use crate::agent::ToolResult;
use crate::swarm::{AgentHub, AgentType};

/// 子代理管理器句柄；runner 构造完成后注入。
pub type HubHandle = std::sync::OnceLock<Arc<AgentHub>>;

pub struct AgentTool {
    hub: Arc<HubHandle>,
    /// 当前代理深度；根代理为 0，子代理为父深度 + 1。
    depth: u32,
}

impl AgentTool {
    pub fn new(hub: Arc<HubHandle>, depth: u32) -> Self {
        Self { hub, depth }
    }

    fn hub(&self) -> Result<Arc<AgentHub>, ToolResult> {
        self.hub
            .get()
            .cloned()
            .ok_or_else(|| ToolResult::error("The subagent runtime is not ready yet."))
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &'static str {
        "Agent"
    }

    fn description(&self) -> &'static str {
        "Spawn and manage subagents. `action` is spawn, message, check, wait or list.\n\n\
         A subagent has its own context and a restricted tool set. Types: explore (read-only \
         search), plan (read-only planning), docs (API help), general (local tools, no sending \
         by default). Custom agents come from /agents/<name>.md and can only use a subset of \
         your tools; their `allow_send` cannot be changed by the spawn call.\n\n\
         Spawn returns an id. Results come back as a summary only; the subagent never talks to \
         QQ directly. Use check to poll, wait to block briefly, message to continue a finished \
         subagent. Do not spawn a subagent for something one tool call can do."
    }

    fn parameters(&self) -> serde_json::Value {
        object_schema(
            serde_json::json!({
                "action": {"type": "string", "enum": ["spawn", "message", "check", "wait", "list"]},
                "type": {"type": "string", "enum": ["general", "explore", "plan", "docs"]},
                "agent": {"type": "string", "description": "Custom agent name from /agents"},
                "task": {"type": "string"},
                "id": {"type": "string"},
                "text": {"type": "string"},
                "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600}
            }),
            &["action"],
        )
    }

    fn permissions(&self) -> &'static [Permission] {
        &[Permission::Read]
    }

    async fn execute(&self, _ctx: &ToolContext, args: serde_json::Value) -> ToolResult {
        let hub = match self.hub() {
            Ok(hub) => hub,
            Err(error) => return error,
        };
        let action = match arg_str(&args, "action") {
            Ok(action) => action,
            Err(error) => return error,
        };
        match action {
            "spawn" => {
                let task = match arg_str(&args, "task") {
                    Ok(task) => task.to_owned(),
                    Err(error) => return error,
                };
                let kind = arg_opt_str(&args, "type");
                if let Some(kind) = kind
                    && AgentType::parse(kind).is_none()
                {
                    return ToolResult::error("type must be general, explore, plan or docs.");
                }
                match hub
                    .spawn(kind, arg_opt_str(&args, "agent"), task, self.depth)
                    .await
                {
                    Ok(value) => ToolResult::ok(value.to_string()),
                    Err(error) => ToolResult::error(error.to_string()),
                }
            }
            "message" => {
                let id = match arg_str(&args, "id") {
                    Ok(id) => id,
                    Err(error) => return error,
                };
                let text = match arg_str(&args, "text") {
                    Ok(text) => text.to_owned(),
                    Err(error) => return error,
                };
                match hub.message(id, text).await {
                    Ok(value) => ToolResult::ok(value.to_string()),
                    Err(error) => ToolResult::error(error.to_string()),
                }
            }
            "check" => match hub.check(arg_opt_str(&args, "id")).await {
                Ok(value) => ToolResult::ok(value.to_string()),
                Err(error) => ToolResult::error(error.to_string()),
            },
            "wait" => {
                let id = match arg_str(&args, "id") {
                    Ok(id) => id,
                    Err(error) => return error,
                };
                let seconds = args
                    .get("timeout_secs")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(120)
                    .clamp(1, 600);
                match hub.wait(id, std::time::Duration::from_secs(seconds)).await {
                    Ok(value) => ToolResult::ok(value.to_string()),
                    Err(error) => ToolResult::error(error.to_string()),
                }
            }
            "list" => ToolResult::ok(hub.list().await.to_string()),
            other => ToolResult::error(format!(
                "Unknown action {other:?}; use spawn, message, check, wait or list."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SwarmConfig;
    use crate::storage::Database;
    use crate::storage::crypto::MasterKey;
    use crate::vfs::quota::Limits;
    use crate::vfs::sqlite::VfsStore;

    struct DoneRunner;

    #[async_trait]
    impl crate::swarm::SubagentRunner for DoneRunner {
        async fn run_subagent(
            &self,
            request: crate::swarm::SubagentRequest,
        ) -> anyhow::Result<String> {
            Ok(format!("ok: {}", request.task))
        }
    }

    async fn tool() -> (AgentTool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let key = Arc::new(MasterKey::from_bytes([3u8; 32]));
        let vfs =
            Arc::new(VfsStore::open(db, key, dir.path().join("data"), Limits::default()).unwrap());
        let hub = AgentHub::new(
            Arc::new(DoneRunner),
            SwarmConfig::default(),
            vfs,
            dir.path().join("agents"),
        );
        let handle = Arc::new(HubHandle::new());
        let _ = handle.set(hub);
        (AgentTool::new(handle, 0), dir)
    }

    async fn call(tool: &AgentTool, value: serde_json::Value) -> ToolResult {
        let ctx = ToolContext::new(
            Arc::new(
                VfsStore::open(
                    Arc::new(Database::open_in_memory().unwrap()),
                    Arc::new(MasterKey::from_bytes([1u8; 32])),
                    tempfile::tempdir().unwrap().keep(),
                    Limits::default(),
                )
                .unwrap(),
            ),
            None,
        );
        tool.execute(&ctx, value).await
    }

    #[tokio::test]
    async fn spawn_then_wait_returns_result() {
        let (tool, _dir) = tool().await;
        let spawned = call(
            &tool,
            serde_json::json!({"action": "spawn", "type": "explore", "task": "look"}),
        )
        .await;
        assert!(!spawned.is_error, "{}", spawned.content);
        let value: serde_json::Value = serde_json::from_str(&spawned.content).unwrap();
        let id = value["id"].as_str().unwrap().to_owned();

        let waited = call(
            &tool,
            serde_json::json!({"action": "wait", "id": id, "timeout_secs": 5}),
        )
        .await;
        assert!(!waited.is_error);
        assert!(waited.content.contains("ok: look"), "{}", waited.content);
    }

    #[tokio::test]
    async fn unknown_type_is_rejected() {
        let (tool, _dir) = tool().await;
        let result = call(
            &tool,
            serde_json::json!({"action": "spawn", "type": "root", "task": "x"}),
        )
        .await;
        assert!(result.is_error);
    }
}
