//! 投递：批处理窗口、空闲等待、速率、上限与到期，最后交给 [`MonitorSink`]。

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::Local;
use tracing::debug;

use super::{Monitor, MonitorDelivery, MonitorEvent, MonitorSink};
use crate::config::DeliverAs;

/// 单个 monitor 同时等待投递的任务上限。
///
/// `idle_only` 的任务最多等 10 分钟；事件洪泛时不能无限 spawn。
const MAX_PENDING_DELIVERIES: usize = 8;

/// 按批处理窗口投递事件；窗口为 0 时立即投递。
///
/// 投递在独立任务里执行，避免 `idle_only` 等待空闲时阻塞事件源。
pub(super) async fn enqueue(
    monitor: &Arc<Monitor>,
    sink: &Arc<dyn MonitorSink>,
    event: &MonitorEvent,
) {
    {
        if monitor.pending_deliveries.load(Ordering::SeqCst) >= MAX_PENDING_DELIVERIES {
            debug!(
                monitor = %monitor.name,
                "too many pending deliveries; dropping this event"
            );
            return;
        }
        // `as = "chat"` 要把每条原始消息交给 ChatQueue（它有自己的去抖），不能合并成文本。
        if monitor.batch_window.is_zero() || monitor.deliver_as == DeliverAs::Chat {
            // 无批处理且无需等空闲时直接投递，保持调用方看到同步结果。
            if !monitor.idle_only {
                monitor.pending_deliveries.fetch_add(1, Ordering::SeqCst);
                deliver_once(monitor, &**sink, Some(event), 1, false).await;
                monitor.pending_deliveries.fetch_sub(1, Ordering::SeqCst);
                return;
            }
            let monitor = Arc::clone(monitor);
            let sink = Arc::clone(sink);
            let event = event.clone();
            monitor.pending_deliveries.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                deliver_once(&monitor, &*sink, Some(&event), 1, false).await;
                monitor.pending_deliveries.fetch_sub(1, Ordering::SeqCst);
            });
            return;
        }

        monitor
            .batch
            .lock()
            .expect("monitor lock")
            .push(if event.chat.is_empty() {
                // 定时、内部、外部流事件没有 chat。
                event.text.clone()
            } else {
                format!("[{}] {}", event.chat, event.text)
            });
        if monitor.flush_queued.swap(true, Ordering::SeqCst) {
            return;
        }
        let monitor = Arc::clone(monitor);
        let sink = Arc::clone(sink);
        monitor.pending_deliveries.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            tokio::time::sleep(monitor.batch_window).await;
            monitor.flush_queued.store(false, Ordering::SeqCst);
            let texts = std::mem::take(&mut *monitor.batch.lock().expect("monitor lock"));
            if texts.is_empty() {
                monitor.pending_deliveries.fetch_sub(1, Ordering::SeqCst);
                return;
            }
            let event = MonitorEvent {
                text: texts.join("\n"),
                ..MonitorEvent::default()
            };
            deliver_once(&monitor, &*sink, Some(&event), texts.len() as u64, false).await;
            monitor.pending_deliveries.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// 到期：停用 monitor 并发一条系统通知。并发的几次投递同时发现到期时只通知一次。
async fn expire(monitor: &Monitor, sink: &dyn MonitorSink) {
    if monitor.cancelled.swap(true, Ordering::SeqCst) {
        return;
    }
    sink.deliver(monitor.system_delivery(
        &monitor.source,
        format!(
            "system: monitor {:?} expired and was disabled",
            monitor.name
        ),
    ))
    .await;
}

/// 渲染 `as = "prompt"` 的模板：把 `{{event.text}}`、`{{event.chat}}`、`{{count}}` 换成这次
/// 投递的值。
///
/// 只扫一遍模板，换进去的内容不再被扫描：事件文本来自 QQ 消息或外部流，里面出现的
/// `{{count}}` 是别人写的字，不是模板的一部分。逐个 `replace` 的话，先换进去的事件文本会被
/// 后面的替换再改一遍。
fn render_prompt(template: &str, event: &MonitorEvent, count: u64) -> String {
    let count = count.to_string();
    let placeholders = [
        ("{{event.text}}", event.text.as_str()),
        ("{{event.chat}}", event.chat.as_str()),
        ("{{count}}", count.as_str()),
    ];
    let mut out = String::with_capacity(template.len() + event.text.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        match placeholders.iter().find(|(name, _)| rest.starts_with(name)) {
            Some((name, value)) => {
                out.push_str(value);
                rest = &rest[name.len()..];
            }
            // 不是占位符：只前进一个 `{`，这样 `{{{count}}` 里的占位符仍然认得出来。
            None => {
                out.push('{');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 投递一次：检查到期、等空闲、限速、计数上限，渲染文本后交给 sink；`force` 跳过限速
/// （手动 `fire`）。`count` 是本次合并的事件数，供 `{{count}}` 使用。
pub(super) async fn deliver_once(
    monitor: &Monitor,
    sink: &dyn MonitorSink,
    event: Option<&MonitorEvent>,
    count: u64,
    force: bool,
) {
    if monitor.cancelled.load(Ordering::SeqCst) {
        return;
    }
    let expired = || {
        monitor
            .expires_at
            .is_some_and(|deadline| Local::now() > deadline)
    };
    if expired() {
        expire(monitor, sink).await;
        return;
    }
    // idle_only：等到 agent 空闲再投递；最多等 10 分钟，超时放弃本次事件。
    if monitor.idle_only {
        let deadline = std::time::Instant::now() + Duration::from_secs(600);
        while !sink.is_idle().await {
            if monitor.cancelled.load(Ordering::SeqCst) || std::time::Instant::now() > deadline {
                return;
            }
            // 等着的时候到期了：和一开始就到期一样处理，不是悄悄丢掉这次事件。
            if expired() {
                expire(monitor, sink).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    if force {
        *monitor.last_delivery.lock().expect("monitor lock") = Some(std::time::Instant::now());
    } else if !monitor.rate_limit_ok() {
        return;
    }

    let text = match (&monitor.prompt, event) {
        (Some(template), Some(event)) => render_prompt(template, event, count),
        (Some(template), None) => template.clone(),
        // 没有 chat 的事件（定时、内部、外部流、合并后的批次）不写空的 chat 段。
        (None, Some(event)) if event.chat.is_empty() => format!(
            "[monitor {} | {}]\n{}",
            monitor.name, monitor.source, event.text
        ),
        (None, Some(event)) => format!(
            "[monitor {} | {} | {}]\n{}",
            monitor.name, monitor.source, event.chat, event.text
        ),
        (None, None) => format!(
            "[monitor {} | {}] fired manually",
            monitor.name, monitor.source
        ),
    };
    let previous = monitor.delivered.fetch_add(1, Ordering::SeqCst);
    if previous >= monitor.max_events {
        // 回滚预留；并发的两个投递只有一个能拿到最后一个名额。
        monitor.delivered.fetch_sub(1, Ordering::SeqCst);
        if !monitor.cancelled.swap(true, Ordering::SeqCst) {
            sink.deliver(monitor.system_delivery(
                &monitor.source,
                format!(
                    "system: monitor {:?} reached its event limit and was disabled",
                    monitor.name
                ),
            ))
            .await;
        }
        return;
    }

    sink.deliver(MonitorDelivery {
        name: monitor.name.clone(),
        source: monitor.source.clone(),
        target: monitor.target.clone(),
        deliver_as: monitor.deliver_as,
        text: text.clone(),
        system: false,
        owner_intent: event.as_ref().is_some_and(|event| event.owner_intent),
        message: event.as_ref().and_then(|event| event.message.clone()),
        on_missing: monitor.on_missing,
    })
    .await;

    if monitor.cc && monitor.target.is_some() {
        sink.deliver(MonitorDelivery {
            name: monitor.name.clone(),
            source: monitor.source.clone(),
            target: None,
            // 抄送只是给主代理看一眼，不当作会话消息处理。
            deliver_as: DeliverAs::Event,
            text: format!("[cc {}] {text}", monitor.name),
            system: false,
            owner_intent: false,
            message: None,
            on_missing: crate::config::OnMissing::Queue,
        })
        .await;
    }

    if monitor.one_shot {
        monitor.cancelled.store(true, Ordering::SeqCst);
    }
}
