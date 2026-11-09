//! `Monitor` 工具：订阅事件并在上下文里收到消息。
//!
//! 设计依据：设计 §5.11 与提示词清单 §3「Monitor」。
//!
//! - `add` / `remove` / `list` / `fire`；
//! - `add` 的参数与配置文件的 `[[monitor]]` 同一套 schema 与校验（`monitor::spec`）；
//!   运行时创建默认有界：`expires = 2h`、`max_events = 50`、`idle_only = true`，没有 `catch_up`；
//! - 拒绝 `stream` 源与 `qq.backlog`（只能由主人写进配置文件）；
//! - `deliver.to` 支持 `main`、`<spawn_id>` 与 `agent:<name>`；子代理结束后绑定不会自动删除
//!   （结束的子代理仍可被续聊），到期或达到上限时停用。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::monitor::{MonitorManager, MonitorOrigin, MonitorSpec, runtime_config};
use crate::transcript::ToolResult;

use super::{Tool, ToolContext, arg_str, object_schema};

/// `Monitor` 工具。
pub struct MonitorTool {
    manager: Arc<MonitorManager>,
}

impl MonitorTool {
    pub(crate) fn new(manager: Arc<MonitorManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for MonitorTool {
    fn name(&self) -> &'static str {
        "Monitor"
    }

    fn description(&self) -> &'static str {
        "Subscribe to an event source; each event, or a prompt you write, arrives as a message in \
         this conversation so you do not have to wait for a turn.\n\n\
         Params: op (add, remove, list, fire), name, and for add the fields of a [[monitor]] in \
         the config file: source, optional when, deliver, lifecycle and on_missing. Unknown \
         fields are rejected.\n\n\
         source — exactly one kind: {\"qq\": true} for inbound QQ events; \
         {\"timer\": {\"every\": \"10m\"}} or {\"timer\": {\"cron\": \"7 9 * * 1-5\"}} or \
         {\"timer\": {\"at\": \"09:07\"}} every day, all in local time (every needs at least \
         60s; when a cron restricts both day and weekday, either one matching is enough); add \
         \"jitter_secs\": n to a timer to delay each run by a random 0..n seconds (at most \
         3600, shorter than every); \
         {\"internal\": {\"kind\": \"security\" | \"budget\" | \"agent\" | \"storage\"}} for \
         runtime state changes; event.type is security:hold, security:released, \
         budget:exhausted, agent:done, agent:failed, storage:quota or storage:disk. Stream \
         sources and qq backlog are config-file only.\n\n\
         when — optional expression over event.*, e.g. event.mentions_self, \
         event.sender == \"10001\", event.text contains \"报名\", event.chat == \"group:123\"; \
         join terms with and, negate one with not. A broken expression is rejected when you \
         add it.\n\n\
         deliver — {\"to\": \"main\" | \"<subagent id>\" | \"agent:<name>\", \
         \"as\": \"event\" | \"prompt\" | \"chat\", \"prompt\"?, \"batch_window_ms\"?, \"cc\"?}. \
         as: prompt sends an instruction you write; {{event.text}}, {{event.chat}} and \
         {{count}} are substituted. as: chat (qq source, to main) hands each message to the \
         normal chat queue, like the built-in wake-on-mention and wake-on-owner presets. cc \
         copies a subagent delivery to you. on_missing — queue (default, waits up to 60s), \
         drop or spawn when the target subagent does not exist.\n\n\
         lifecycle — {idle_only?, min_interval_secs?, max_events?, expires?, one_shot?}. A \
         monitor you add lives in memory and is gone after a restart; by default it delivers \
         only when you are idle, expires after 2h (\"0\" = never) and stops after 50 events.\n\n\
         Silence is not success. Before adding one, ask: if the thing I am watching failed right \
         now, would this monitor deliver anything? If not, widen when. A monitor delivers \
         messages; it never executes anything on its own. list shows name, source, target, \
         origin (config, runtime or preset), deliveries and expiry; removing a preset is \
         remembered across restarts. fire triggers one delivery (idle_only may wait)."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "op": {"type": "string", "enum": ["add", "remove", "list", "fire"]},
                "name": {"type": "string"},
                "source": {"type": "object"},
                "when": {"type": "string"},
                "deliver": {"type": "object"},
                "lifecycle": {"type": "object"},
                "on_missing": {"type": "string", "enum": ["queue", "drop", "spawn"]}
            }),
            &["op"],
        )
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let op = match arg_str(&args, "op") {
            Ok(op) => op,
            Err(error) => return error,
        };
        match op {
            "list" => match serde_json::to_string_pretty(&self.manager.list().await) {
                Ok(text) => ToolResult::ok(text),
                Err(error) => ToolResult::error(error.to_string()),
            },
            "remove" => {
                let name = match arg_str(&args, "name") {
                    Ok(name) => name,
                    Err(error) => return error,
                };
                if self.manager.remove(name).await {
                    ToolResult::ok(json!({"removed": name}).to_string())
                } else {
                    ToolResult::error(format!("Monitor {name:?} does not exist."))
                }
            }
            "fire" => {
                let name = match arg_str(&args, "name") {
                    Ok(name) => name,
                    Err(error) => return error,
                };
                match self.manager.fire_async(name).await {
                    Ok(()) => ToolResult::ok(json!({"fired": name}).to_string()),
                    Err(error) => ToolResult::error(error.to_string()),
                }
            }
            "add" => self.add(ctx, &args).await,
            other => ToolResult::error(format!(
                "Unknown op {other:?}; use add, remove, list or fire."
            )),
        }
    }
}

impl MonitorTool {
    /// `add`：除 `op` / `name` 外的参数就是一份 `[[monitor]]`，按配置文件的同一套 schema 与
    /// 校验解析（见 `monitor::spec`），只是默认值更保守。
    ///
    /// 子代理创建的 monitor 只投给它自己：`deliver.to` 必须省略或写它自己的 id，否则它可以
    /// 借 monitor 往主上下文或别的子代理里注入内容。
    async fn add(&self, ctx: &ToolContext, args: &Value) -> ToolResult {
        let name = match arg_str(args, "name") {
            Ok(name) => name,
            Err(error) => return error,
        };
        let mut fields = args.as_object().cloned().unwrap_or_default();
        fields.remove("op");
        fields.remove("name");
        let mut config = match runtime_config(name, Value::Object(fields)) {
            Ok(config) => config,
            Err(error) => return ToolResult::error(error),
        };
        if let Some(id) = &ctx.subagent {
            let explicit = args.pointer("/deliver/to").and_then(Value::as_str);
            if explicit.is_some_and(|to| to != id) {
                return ToolResult::error(format!(
                    "A subagent's monitor delivers only to itself; omit deliver.to or set it to \
                     {id:?}."
                ));
            }
            config.deliver.to = id.clone();
            config.deliver.cc = false;
        }
        let spec = match MonitorSpec::from_config(&config, MonitorOrigin::Runtime) {
            Ok(spec) => spec,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        let max_events = spec.max_events;
        match self.manager.add(spec).await {
            Ok(()) => ToolResult::ok(json!({"added": name, "max_events": max_events}).to_string()),
            Err(error) => ToolResult::error(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::MonitorSink;
    use std::sync::Mutex;

    #[derive(Default)]
    struct NullSink {
        deliveries: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl MonitorSink for NullSink {
        async fn deliver(&self, delivery: crate::monitor::MonitorDelivery) {
            self.deliveries.lock().unwrap().push(delivery.text);
        }
    }

    fn tool() -> (MonitorTool, Arc<NullSink>) {
        let sink = Arc::new(NullSink::default());
        let manager = MonitorManager::new(Arc::clone(&sink) as Arc<dyn MonitorSink>);
        (MonitorTool::new(manager), sink)
    }

    fn ctx() -> ToolContext {
        ToolContext::new(
            Arc::new(
                crate::vfs::sqlite::VfsStore::open(
                    Arc::new(crate::storage::Database::open_in_memory().unwrap()),
                    Arc::new(crate::storage::crypto::MasterKey::from_bytes([1u8; 32])),
                    tempfile::tempdir().unwrap().keep(),
                    crate::vfs::quota::Limits::default(),
                )
                .unwrap(),
            ),
            None,
        )
    }

    async fn run(tool: &MonitorTool, args: Value) -> ToolResult {
        tool.execute(&ctx(), args).await
    }

    #[tokio::test]
    async fn add_list_fire_remove() {
        let (tool, sink) = tool();
        let result = run(
            &tool,
            json!({
                "op": "add",
                "name": "at-me",
                "source": {"qq": true},
                "when": "event.mentions_self",
                "deliver": {"to": "main", "as": "event"}
            }),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);

        let result = run(&tool, json!({"op": "list"})).await;
        assert!(result.content.contains("at-me"));

        // 常量条件在 fire 时以“手动触发”文本投递。
        let result = run(&tool, json!({"op": "fire", "name": "at-me"})).await;
        assert!(!result.is_error, "{}", result.content);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(sink.deliveries.lock().unwrap().len(), 1);

        let result = run(&tool, json!({"op": "remove", "name": "at-me"})).await;
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn rejects_stream_and_subagent_delivery() {
        let (tool, _sink) = tool();
        let result = run(
            &tool,
            json!({"op": "add", "name": "x", "source": {"stream": {"url": "wss://x"}}}),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("config-file"));

        // 子代理绑定现在允许；列表里应带 target。
        let result = run(
            &tool,
            json!({
                "op": "add", "name": "y", "source": {"qq": true},
                "deliver": {"to": "agent:helper"}
            }),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let listed = run(&tool, json!({"op": "list"})).await;
        assert!(
            listed.content.contains("agent:helper"),
            "{}",
            listed.content
        );
    }

    /// 子代理的 monitor 只投给它自己，不能借它把事件塞进主上下文。
    #[tokio::test]
    async fn a_subagent_monitor_delivers_only_to_that_subagent() {
        let (tool, _sink) = tool();
        let sub = ctx().with_subagent("sa-7");
        let result = tool
            .execute(
                &sub,
                json!({"op": "add", "name": "mine", "source": {"qq": true}}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        let listed = run(&tool, json!({"op": "list"})).await;
        assert!(listed.content.contains("sa-7"), "{}", listed.content);

        for to in ["main", "sa-8", "agent:helper"] {
            let result = tool
                .execute(
                    &sub,
                    json!({"op": "add", "name": format!("x-{to}"), "source": {"qq": true},
                           "deliver": {"to": to}}),
                )
                .await;
            assert!(result.is_error, "{to}: {}", result.content);
        }
    }

    #[tokio::test]
    async fn timer_validation() {
        let (tool, _sink) = tool();
        let result = run(
            &tool,
            json!({"op": "add", "name": "t", "source": {"timer": {"every": "10s"}}}),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("60s"));

        let result = run(
            &tool,
            json!({"op": "add", "name": "t2", "source": {"timer": {"cron": "7 9 * * 1-5"}}}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
    }
}
