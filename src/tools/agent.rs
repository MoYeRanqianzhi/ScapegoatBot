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
use crate::monitor::{CronSchedule, MonitorManager, TimerSpec};
use crate::swarm::{AgentHub, AgentType};

/// 子代理管理器句柄；runner 构造完成后注入。
pub type HubHandle = std::sync::OnceLock<Arc<AgentHub>>;

pub struct AgentTool {
    hub: Arc<HubHandle>,
    /// 当前代理深度；根代理为 0，子代理为父深度 + 1。
    depth: u32,
    /// 用于 `spawn` 的 `monitor` 绑定；未接入时忽略该参数。
    monitors: Option<Arc<MonitorManager>>,
}

impl AgentTool {
    pub fn new(hub: Arc<HubHandle>, depth: u32) -> Self {
        Self {
            hub,
            depth,
            monitors: None,
        }
    }

    /// 接入 monitor 管理器，支持 `Agent spawn` 的 `monitor` 绑定。
    pub fn with_monitors(mut self, monitors: Arc<MonitorManager>) -> Self {
        self.monitors = Some(monitors);
        self
    }

    fn hub(&self) -> Result<Arc<AgentHub>, ToolResult> {
        self.hub
            .get()
            .cloned()
            .ok_or_else(|| ToolResult::error("The subagent runtime is not ready yet."))
    }

    /// 把 `monitor` 参数绑定到刚创建的子代理：事件只投给它，不进主上下文。
    async fn bind_monitor(&self, id: &str, spec: &serde_json::Value) -> anyhow::Result<String> {
        let Some(manager) = &self.monitors else {
            anyhow::bail!("monitor binding is unavailable in this runtime");
        };
        let source = spec
            .get("source")
            .ok_or_else(|| anyhow::anyhow!("monitor.source is required"))?;
        let (kind, timer) = if source.get("qq").is_some() {
            ("qq".to_owned(), None)
        } else if let Some(timer) = source.get("timer") {
            if let Some(every) = timer.get("every").and_then(serde_json::Value::as_str) {
                let seconds = crate::config::parse_duration_secs(every)
                    .ok_or_else(|| anyhow::anyhow!("invalid timer.every {every:?}"))?;
                if seconds < 60 {
                    anyhow::bail!("timer.every must be at least 60s");
                }
                (
                    "timer".to_owned(),
                    Some(TimerSpec::Every(std::time::Duration::from_secs(seconds))),
                )
            } else if let Some(cron) = timer.get("cron").and_then(serde_json::Value::as_str) {
                CronSchedule::parse(cron).map_err(|error| anyhow::anyhow!(error))?;
                ("timer".to_owned(), Some(TimerSpec::Cron(cron.to_owned())))
            } else if let Some(at) = timer.get("at").and_then(serde_json::Value::as_str) {
                let (hour, minute) = at
                    .split_once(':')
                    .ok_or_else(|| anyhow::anyhow!("timer.at must be HH:MM"))?;
                (
                    "timer".to_owned(),
                    Some(TimerSpec::At {
                        hour: hour.parse()?,
                        minute: minute.parse()?,
                    }),
                )
            } else {
                anyhow::bail!("timer requires every, cron or at");
            }
        } else if let Some(internal) = source.get("internal").and_then(serde_json::Value::as_str) {
            (format!("internal:{internal}"), None)
        } else {
            anyhow::bail!("source must be qq, timer or internal (stream is not allowed here)");
        };

        let when = spec
            .get("when")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let deliver = spec
            .get("deliver")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let as_prompt = deliver.get("as").and_then(serde_json::Value::as_str) == Some("prompt");
        let prompt = deliver
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if as_prompt && prompt.is_none() {
            anyhow::bail!("deliver.as = \"prompt\" requires deliver.prompt");
        }
        let lifecycle = spec
            .get("lifecycle")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let max_events = lifecycle
            .get("max_events")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(50);
        let one_shot = lifecycle
            .get("one_shot")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let idle_only = lifecycle
            .get("idle_only")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let min_interval = std::time::Duration::from_secs(
            lifecycle
                .get("min_interval_secs")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        );
        let expires = match lifecycle.get("expires").and_then(serde_json::Value::as_str) {
            None => Some(std::time::Duration::from_secs(2 * 3600)),
            Some("0") => None,
            Some(raw) => Some(std::time::Duration::from_secs(
                crate::config::parse_duration_secs(raw)
                    .ok_or_else(|| anyhow::anyhow!("invalid expires {raw:?}"))?,
            )),
        };
        let batch_window = std::time::Duration::from_millis(
            deliver
                .get("batch_window_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(200),
        );

        let name = format!("{id}:monitor");
        manager
            .add(
                name.clone(),
                kind,
                timer,
                when,
                prompt,
                as_prompt,
                max_events,
                expires,
                one_shot,
                min_interval,
                idle_only,
                batch_window,
                Some(id.to_owned()),
                crate::config::CatchUp::Coalesce,
            )
            .await?;
        Ok(name)
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

    async fn execute(&self, ctx: &ToolContext, args: serde_json::Value) -> ToolResult {
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
                    .spawn(
                        kind,
                        arg_opt_str(&args, "agent"),
                        task,
                        self.depth,
                        ctx.evidence.clone(),
                    )
                    .await
                {
                    Ok(mut value) => {
                        if let Some(spec) = args.get("monitor") {
                            let id = value["id"].as_str().unwrap_or_default().to_owned();
                            match self.bind_monitor(&id, spec).await {
                                Ok(name) => value["monitor"] = serde_json::json!(name),
                                Err(error) => {
                                    value["monitor_error"] = serde_json::json!(error.to_string());
                                }
                            }
                        }
                        ToolResult::ok(value.to_string())
                    }
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
