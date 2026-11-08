//! 计量装饰器：每次模型请求的用量，按这个模型自己的价格记账。
//!
//! 以前只有主回合在结束时把累计用量按主模型的价格记一次：子代理、压缩摘要、记忆
//! flush、周期安全审查、LLM 门禁都不计入，热切换模型后价格也不变。现在所有经
//! [`build_metered`](super::build_metered) 构造的 provider 都包一层 [`Metered`]：
//! 无论谁在调用、用的是哪个模型，记账都发生在请求本身这一层，不可能漏。
//!
//! 一次请求的流里可能有多个 [`ProviderEvent::Usage`]：它们是累计快照（agent 循环也是
//! “最后一个为准”）。这里转发全部事件，流结束时记录最后一个快照；调用方提前丢弃
//! 接收端时同样记录——那些 token 已经花掉了。

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::{EventReceiver, LlmRequest, Provider, ProviderError, ProviderEvent};
use crate::config::models::{CostDef, Protocol};
use crate::llm::types::Usage;

/// 转发通道的容量；与各协议实现的事件通道同量级，背压照常传导到上游。
const FORWARD_CAPACITY: usize = 256;

/// 记账的去处（运行时是 [`crate::budget::Budget`]）。
#[async_trait]
pub trait UsageSink: Send + Sync {
    /// 记录一次请求的用量；`cost` 是发出这次请求的模型的价格。
    async fn record(&self, cost: &CostDef, usage: &Usage);
}

/// 给 provider 加上计量。
pub struct Metered {
    inner: Arc<dyn Provider>,
    cost: CostDef,
    sink: Arc<dyn UsageSink>,
}

impl Metered {
    /// 包装 `inner`；用量按 `cost` 计价后交给 `sink`。
    pub fn wrap(
        inner: Arc<dyn Provider>,
        cost: CostDef,
        sink: Arc<dyn UsageSink>,
    ) -> Arc<dyn Provider> {
        Arc::new(Self { inner, cost, sink })
    }
}

#[async_trait]
impl Provider for Metered {
    fn protocol(&self) -> Protocol {
        self.inner.protocol()
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    async fn stream(&self, request: LlmRequest) -> Result<EventReceiver, ProviderError> {
        // 请求本身失败（连接、鉴权、状态码）时没有用量可记，原样返回错误。
        let mut upstream = self.inner.stream(request).await?;
        let (tx, rx) = mpsc::channel(FORWARD_CAPACITY);
        let sink = Arc::clone(&self.sink);
        let cost = self.cost;
        tokio::spawn(async move {
            let mut latest = None;
            while let Some(event) = upstream.recv().await {
                if let ProviderEvent::Usage(usage) = &event {
                    latest = Some(*usage);
                }
                // 下游已经不要了：停止转发，但仍要记下已经产生的用量。
                if tx.send(event).await.is_err() {
                    break;
                }
            }
            if let Some(usage) = latest.filter(|usage| *usage != Usage::default()) {
                sink.record(&cost, &usage).await;
            }
        });
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 按顺序吐出给定事件的假 provider。
    struct Scripted(Vec<ProviderEvent>);

    #[async_trait]
    impl Provider for Scripted {
        fn protocol(&self) -> Protocol {
            Protocol::Anthropic
        }
        fn model_id(&self) -> &str {
            "scripted"
        }
        async fn stream(&self, _request: LlmRequest) -> Result<EventReceiver, ProviderError> {
            let (tx, rx) = mpsc::channel(16);
            for event in self.0.clone() {
                tx.send(event).await.unwrap();
            }
            Ok(rx)
        }
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(f64, Usage)>>);

    #[async_trait]
    impl UsageSink for Recorder {
        async fn record(&self, cost: &CostDef, usage: &Usage) {
            self.0.lock().unwrap().push((cost.input, *usage));
        }
    }

    fn usage(input: u64, output: u64) -> ProviderEvent {
        ProviderEvent::Usage(Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::default()
        })
    }

    /// 最小请求：计量不看请求内容。
    fn request() -> LlmRequest {
        LlmRequest {
            model: "scripted".to_owned(),
            system: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: 16,
            thinking: Default::default(),
            server_tools: Vec::new(),
        }
    }

    async fn drain(mut rx: EventReceiver) -> usize {
        let mut count = 0;
        while rx.recv().await.is_some() {
            count += 1;
        }
        count
    }

    #[tokio::test]
    async fn the_last_usage_snapshot_is_recorded_once_with_the_models_price() {
        let recorder = Arc::new(Recorder::default());
        let provider = Metered::wrap(
            Arc::new(Scripted(vec![
                usage(100, 0),
                ProviderEvent::TextDelta("hi".to_owned()),
                usage(100, 7),
            ])),
            CostDef {
                input: 3.0,
                ..CostDef::default()
            },
            recorder.clone(),
        );
        let rx = provider.stream(request()).await.unwrap();
        // 事件原样转发。
        assert_eq!(drain(rx).await, 3);
        // 等转发任务记账。
        for _ in 0..50 {
            if !recorder.0.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let records = recorder.0.lock().unwrap();
        assert_eq!(records.len(), 1, "cumulative snapshots must not be summed");
        assert_eq!(records[0].0, 3.0);
        assert_eq!(records[0].1.output_tokens, 7);
    }

    #[tokio::test]
    async fn usage_is_recorded_even_when_the_caller_stops_listening() {
        let recorder = Arc::new(Recorder::default());
        let provider = Metered::wrap(
            Arc::new(Scripted(vec![
                usage(50, 5),
                ProviderEvent::TextDelta("x".into()),
            ])),
            CostDef::default(),
            recorder.clone(),
        );
        drop(provider.stream(request()).await.unwrap());
        for _ in 0..50 {
            if !recorder.0.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(recorder.0.lock().unwrap().len(), 1);
    }
}
