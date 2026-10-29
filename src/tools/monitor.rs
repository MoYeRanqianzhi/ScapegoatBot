//! `Monitor` 工具：订阅事件并在上下文里收到消息。
//!
//! 设计依据：设计 §5.11 与提示词清单 §3「Monitor」。
//!
//! - `add` / `remove` / `list` / `fire`；
//! - 运行时创建默认有界：`expires = 2h`、`max_events = 50`、`idle_only = true`；
//! - 拒绝 `stream` 源（外部网络只能由主人写进配置文件）；
//! - 一次性子代理的 monitor 绑定由 Phase 6 接入；当前只支持 `deliver.to = "main"`。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::ToolResult;
use crate::monitor::{CronSchedule, MonitorManager, TimerSpec};

use super::{Tool, ToolContext, arg_opt_str, arg_str, object_schema};

/// `Monitor` 工具。
pub struct MonitorTool {
    manager: Arc<MonitorManager>,
}

impl MonitorTool {
    pub fn new(manager: Arc<MonitorManager>) -> Self {
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
         Params: op (add, remove, list, fire), name, and for add: source, optional when, deliver, \
         optional lifecycle.\n\n\
         source — exactly one kind: {\"qq\": true} for inbound QQ events; \
         {\"timer\": {\"every\": \"10m\"}} or {\"timer\": {\"cron\": \"7 9 * * 1-5\"}} or \
         {\"timer\": {\"at\": \"09:07\"}} in local time (every needs at least 60s); \
         {\"internal\": \"security\" | \"budget\" | \"agent\" | \"storage\"} for runtime state \
         changes. A stream source is config-file only; add rejects it.\n\n\
         when — optional expression over event.*, e.g. event.mentions_self, \
         event.text contains \"报名\", event.chat == \"group:123\". A broken expression is \
         reported, never silently ignored.\n\n\
         deliver — {\"to\": \"main\", \"as\": \"event\" | \"prompt\", \"prompt\"?: \"...\"}. \
         as: prompt sends an instruction you write; {{event.text}} and {{count}} are substituted.\n\n\
         lifecycle — {idle_only?, min_interval_secs?, max_events?, expires?, one_shot?, \
         catch_up?}. Runtime adds are bounded: expires defaults to 2h, max_events to 50.\n\n\
         Silence is not success. Before adding one, ask: if the thing I am watching failed right \
         now, would this monitor deliver anything? If not, widen when. A monitor delivers \
         messages; it never executes anything on its own. list shows name, source, remaining \
         quota and expiry; fire triggers one immediately."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "op": {"type": "string", "enum": ["add", "remove", "list", "fire"]},
                "name": {"type": "string"},
                "source": {"type": "object"},
                "when": {"type": "string"},
                "deliver": {"type": "object"},
                "lifecycle": {"type": "object"}
            }),
            &["op"],
        )
    }

    async fn execute(&self, _ctx: &ToolContext, args: Value) -> ToolResult {
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
                match self.manager.fire(name).await {
                    Ok(()) => ToolResult::ok(json!({"fired": name}).to_string()),
                    Err(error) => ToolResult::error(error.to_string()),
                }
            }
            "add" => self.add(&args).await,
            other => ToolResult::error(format!(
                "Unknown op {other:?}; use add, remove, list or fire."
            )),
        }
    }
}

impl MonitorTool {
    async fn add(&self, args: &Value) -> ToolResult {
        let name = match arg_str(args, "name") {
            Ok(name) => name,
            Err(error) => return error,
        };
        let Some(source) = args.get("source") else {
            return ToolResult::error("add requires \"source\".");
        };

        let (source_kind, timer) = if source.get("stream").is_some() {
            return ToolResult::error(
                "A stream source is config-file only; `Monitor add` refuses external network sources.",
            );
        } else if source.get("qq").is_some() {
            ("qq".to_owned(), None)
        } else if let Some(timer) = source.get("timer") {
            match parse_timer(timer) {
                Ok(spec) => ("timer".to_owned(), Some(spec)),
                Err(error) => return error,
            }
        } else if let Some(internal) = source.get("internal").and_then(Value::as_str) {
            if !["security", "budget", "agent", "storage"].contains(&internal) {
                return ToolResult::error(
                    "internal source must be security, budget, agent or storage.",
                );
            }
            (format!("internal:{internal}"), None)
        } else {
            return ToolResult::error(
                "source must set exactly one of qq, timer, internal (stream is rejected here).",
            );
        };

        let when = arg_opt_str(args, "when").map(str::to_owned);
        let deliver = args.get("deliver").cloned().unwrap_or_else(|| json!({}));
        let to = deliver.get("to").and_then(Value::as_str).unwrap_or("main");
        if to != "main" {
            return ToolResult::error(
                "Only deliver.to = \"main\" is supported; subagent monitors arrive with Phase 6.",
            );
        }
        let as_prompt = deliver.get("as").and_then(Value::as_str) == Some("prompt");
        let prompt = deliver
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if as_prompt && prompt.is_none() {
            return ToolResult::error("deliver.as = \"prompt\" requires deliver.prompt.");
        }

        let lifecycle = args.get("lifecycle").cloned().unwrap_or_else(|| json!({}));
        let max_events = lifecycle
            .get("max_events")
            .and_then(Value::as_u64)
            .unwrap_or(50);
        let min_interval = Duration::from_secs(
            lifecycle
                .get("min_interval_secs")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        let one_shot = lifecycle
            .get("one_shot")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let expires = match lifecycle.get("expires").and_then(Value::as_str) {
            None => Some(Duration::from_secs(2 * 3600)),
            Some("0") => None,
            Some(raw) => match crate::config::parse_duration_secs(raw) {
                Some(seconds) => Some(Duration::from_secs(seconds)),
                None => return ToolResult::error(format!("invalid expires {raw:?}")),
            },
        };
        if one_shot && timer.is_none() {
            return ToolResult::error("one_shot requires a timer source.");
        }

        match self
            .manager
            .add(
                name.to_owned(),
                source_kind,
                timer,
                when,
                prompt,
                as_prompt,
                max_events,
                expires,
                one_shot,
                min_interval,
            )
            .await
        {
            Ok(()) => ToolResult::ok(json!({"added": name, "max_events": max_events}).to_string()),
            Err(error) => ToolResult::error(error.to_string()),
        }
    }
}

fn parse_timer(value: &Value) -> Result<TimerSpec, ToolResult> {
    let set = ["cron", "every", "at"]
        .iter()
        .filter(|key| value.get(**key).is_some())
        .count();
    if set != 1 {
        return Err(ToolResult::error(
            "timer must set exactly one of cron, every, at.",
        ));
    }
    if let Some(every) = value.get("every").and_then(Value::as_str) {
        let seconds = crate::config::parse_duration_secs(every)
            .ok_or_else(|| ToolResult::error(format!("invalid duration {every:?}")))?;
        if seconds < 60 {
            return Err(ToolResult::error("timer.every must be at least 60s."));
        }
        return Ok(TimerSpec::Every(Duration::from_secs(seconds)));
    }
    if let Some(at) = value.get("at").and_then(Value::as_str) {
        let (hour, minute) = at
            .split_once(':')
            .ok_or_else(|| ToolResult::error("timer.at must be HH:MM"))?;
        return Ok(TimerSpec::At {
            hour: hour
                .parse()
                .map_err(|_| ToolResult::error("invalid hour"))?,
            minute: minute
                .parse()
                .map_err(|_| ToolResult::error("invalid minute"))?,
        });
    }
    let cron = value
        .get("cron")
        .and_then(Value::as_str)
        .unwrap_or_default();
    CronSchedule::parse(cron).map_err(ToolResult::error)?;
    Ok(TimerSpec::Cron(cron.to_owned()))
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

        let result = run(
            &tool,
            json!({
                "op": "add", "name": "y", "source": {"qq": true},
                "deliver": {"to": "agent:helper"}
            }),
        )
        .await;
        assert!(result.is_error);
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
