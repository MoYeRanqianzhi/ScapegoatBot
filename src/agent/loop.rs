//! Agent 循环：显式状态机 + 事件流。
//!
//! 设计依据：设计 §3.2 / §3.4。
//!
//! 结构（简化自 pi）：
//! ```text
//! outer: 追加初始消息 → inner
//! inner: 准备请求 → 输入闸 → 流式请求 → 输出闸
//!        → 无工具调用则结束；有则执行工具、追加结果、注入 steering，继续
//!        结束后若有 follow-up 消息，再开一轮 outer
//! ```
//!
//! 两个不变量：
//! 1. 工具调用与工具结果必须成对进入历史；失败也要写一条错误结果。
//! 2. 部分流不落历史；只有完整解析出的助手消息才追加。

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::providers::{
    ContentBlock, LlmRequest, Provider, ProviderError, ProviderEvent, ProviderMessage, Role,
    ServerToolKind, SystemBlock, ThinkingOptions,
};
use crate::security::Checkpoint;
use crate::security::Gate;

use super::event::AgentEvent;
use super::message::{
    AgentMessage, AssistantBlock, AssistantMessage, StopReason, ToolCall, ToolResult,
    ToolResultMessage, Usage,
};
use super::{ExecutionMode, ToolExecutor};

/// 重试策略。
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub attempts: u32,
    pub base_ms: u64,
    pub max_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            base_ms: 400,
            max_ms: 30_000,
        }
    }
}

/// 循环配置。
#[derive(Debug, Clone)]
pub struct LoopConfig {
    /// API model id。
    pub model: String,
    /// 系统提示块。
    pub system: Vec<SystemBlock>,
    pub max_tokens: u32,
    pub thinking: ThinkingOptions,
    pub server_tools: Vec<ServerToolKind>,
    pub max_steps: u32,
    pub retry: RetryPolicy,
    /// 输入闸的 state 截断长度（字符）。
    pub max_state_chars: usize,
    /// 是否裁剪较早的工具结果（降低长回合的成本）。
    pub prune: bool,
    /// 单条旧工具结果保留的字符数。
    pub prune_soft_limit: usize,
    /// 最近多少个“回合起点”不裁剪。
    pub prune_keep_turns: u32,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            system: Vec::new(),
            max_tokens: 8_192,
            thinking: ThinkingOptions::default(),
            server_tools: Vec::new(),
            max_steps: 64,
            retry: RetryPolicy::default(),
            max_state_chars: 32_000,
            prune: false,
            prune_soft_limit: 4_000,
            prune_keep_turns: 3,
        }
    }
}

/// 运行结果。
#[derive(Debug, Clone, PartialEq)]
pub struct LoopOutcome {
    pub stop_reason: StopReason,
    pub usage: Usage,
    pub turns: u32,
}

/// Steering / follow-up 队列。
///
/// 回合输入由聊天队列、控制通道与子代理共用这一接口。
#[derive(Debug, Default)]
pub struct SteeringQueue {
    queue: VecDeque<AgentMessage>,
}

impl SteeringQueue {
    pub fn push(&mut self, message: AgentMessage) {
        self.queue.push_back(message);
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// 取出全部待注入消息。
    pub fn drain(&mut self) -> Vec<AgentMessage> {
        self.queue.drain(..).collect()
    }
}

/// 回退模型：主模型失败后按顺序尝试。
///
/// 每个回退项持有自己的 provider（协议与密钥可能不同）与输出上限。
#[derive(Clone)]
pub struct FallbackProvider {
    /// `provider/id` 形式的模型名，仅用于日志与请求体。
    pub model: String,
    pub provider: Arc<dyn Provider>,
    /// 该模型的最大输出 token；实际请求取当前请求与该值的较小者。
    pub max_tokens: u32,
}

/// Agent 循环。
pub struct AgentLoop {
    provider: Arc<dyn Provider>,
    /// 主模型失败后的回退链；空表示不回退。
    fallbacks: Vec<FallbackProvider>,
    tools: Arc<dyn ToolExecutor>,
    gate: Arc<Gate>,
    config: LoopConfig,
}

impl AgentLoop {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<dyn ToolExecutor>,
        gate: Arc<Gate>,
        config: LoopConfig,
    ) -> Self {
        Self::new_with_fallbacks(provider, Vec::new(), tools, gate, config)
    }

    /// 带回退链构造。
    pub fn new_with_fallbacks(
        provider: Arc<dyn Provider>,
        fallbacks: Vec<FallbackProvider>,
        tools: Arc<dyn ToolExecutor>,
        gate: Arc<Gate>,
        config: LoopConfig,
    ) -> Self {
        Self {
            provider,
            fallbacks,
            tools,
            gate,
            config,
        }
    }

    /// 运行一次 agent。
    ///
    /// `history` 是可变的全量历史；`prompts` 是本次触发追加的消息；
    /// `steering` 在循环中被消费，剩余消息作为 follow-up 开新轮。
    pub async fn run(
        &self,
        history: &mut Vec<AgentMessage>,
        prompts: Vec<AgentMessage>,
        steering: &mut SteeringQueue,
        events: &mpsc::Sender<AgentEvent>,
    ) -> LoopOutcome {
        self.run_with_steering(history, prompts, steering, None, events)
            .await
    }

    /// 与 [`AgentLoop::run`] 相同，但额外接受一个实时 steering 通道。
    ///
    /// 队列在回合运行期间把同 chat 的新消息送进来；每个请求边界都会被排空，
    /// 对应设计 §3.4 的 `steer` 语义。
    pub async fn run_with_steering(
        &self,
        history: &mut Vec<AgentMessage>,
        prompts: Vec<AgentMessage>,
        steering: &mut SteeringQueue,
        mut incoming: Option<&mut mpsc::UnboundedReceiver<AgentMessage>>,
        events: &mpsc::Sender<AgentEvent>,
    ) -> LoopOutcome {
        let mut total_usage = Usage::default();
        let mut turns = 0u32;
        let mut pending = prompts;
        // 自上次输入闸以来新进入上下文的文本；输入闸只检查这段增量。
        let mut input_delta = String::new();

        send(events, AgentEvent::AgentStart).await;

        'outer: loop {
            for message in pending.drain(..) {
                input_delta.push_str(&render_message(&message));
                history.push(message);
            }

            loop {
                drain_incoming(steering, &mut incoming);
                if self.config.prune {
                    prune_tool_results(
                        history,
                        self.config.prune_keep_turns as usize,
                        self.config.prune_soft_limit,
                    );
                }
                turns += 1;
                if turns > self.config.max_steps {
                    send(
                        events,
                        AgentEvent::Error {
                            message: format!("step limit {} reached", self.config.max_steps),
                        },
                    )
                    .await;
                    return conclude(events, total_usage, turns, StopReason::Error).await;
                }
                send(events, AgentEvent::TurnStart { turn: turns }).await;

                // 输入闸：检查这一步新进入上下文的 token。
                if !input_delta.is_empty() {
                    let state = render_state(history, self.config.max_state_chars);
                    let decision = self
                        .gate
                        .check(Checkpoint::Input, &state, &input_delta)
                        .await;
                    send(
                        events,
                        AgentEvent::Security {
                            checkpoint: super::event::SecurityCheckpoint::Input,
                            allowed: decision.allowed,
                            reason: decision.reason.clone(),
                        },
                    )
                    .await;
                    input_delta.clear();
                    if !decision.allowed {
                        return conclude(events, total_usage, turns, StopReason::Aborted).await;
                    }
                }

                let request = self.build_request(history);
                send(events, AgentEvent::MessageStart).await;

                let assistant = match self.request_with_retry(&request, events).await {
                    Ok(message) => message,
                    Err(error) => {
                        send(
                            events,
                            AgentEvent::Error {
                                message: error.to_string(),
                            },
                        )
                        .await;
                        return conclude(events, total_usage, turns, StopReason::Error).await;
                    }
                };

                // 输出闸：在任何副作用之前检查全部输出（文本、思考、工具参数）。
                let output = render_assistant(&assistant);
                let state = render_state(history, self.config.max_state_chars);
                let decision = self.gate.check(Checkpoint::Output, &state, &output).await;
                send(
                    events,
                    AgentEvent::Security {
                        checkpoint: super::event::SecurityCheckpoint::Output,
                        allowed: decision.allowed,
                        reason: decision.reason.clone(),
                    },
                )
                .await;
                if !decision.allowed {
                    return conclude(events, total_usage, turns, StopReason::Aborted).await;
                }

                total_usage.add(assistant.usage);
                let stop_reason = assistant.stop_reason;
                let has_calls = !assistant.tool_calls().is_empty();

                // provider 侧拒绝是安全事件：不重试、不换模型，直接 hold。
                if stop_reason == StopReason::Refusal {
                    if let Some(refusal) = &assistant.refusal {
                        warn!(
                            category = %refusal.category,
                            explanation = %refusal.explanation,
                            "provider refused the request"
                        );
                    }
                    self.gate
                        .force_hold("provider returned stop_reason=refusal");
                    let calls: Vec<ToolCall> =
                        assistant.tool_calls().into_iter().cloned().collect();
                    history.push(AgentMessage::Assistant(assistant));
                    append_interrupted(history, &calls, "the provider refused the turn");
                    return conclude(events, total_usage, turns, StopReason::Refusal).await;
                }

                if stop_reason == StopReason::Error {
                    let calls: Vec<ToolCall> =
                        assistant.tool_calls().into_iter().cloned().collect();
                    history.push(AgentMessage::Assistant(assistant));
                    append_interrupted(history, &calls, "the provider returned an error");
                    return conclude(events, total_usage, turns, StopReason::Error).await;
                }

                if stop_reason == StopReason::MaxTokens && has_calls {
                    // 工具参数可能被截断，全部调用都不执行。
                    let calls: Vec<ToolCall> =
                        assistant.tool_calls().into_iter().cloned().collect();
                    history.push(AgentMessage::Assistant(assistant));
                    for call in calls {
                        let result = ToolResultMessage {
                            tool_call_id: call.id,
                            tool_name: call.name,
                            content: "Tool call was not executed because the response hit \
                                      max_tokens; its arguments may be truncated."
                                .to_owned(),
                            is_error: true,
                        };
                        input_delta.push_str(&render_message(&AgentMessage::Tool(result.clone())));
                        history.push(AgentMessage::Tool(result));
                    }
                    return conclude(events, total_usage, turns, StopReason::MaxTokens).await;
                }

                let calls: Vec<ToolCall> = assistant.tool_calls().into_iter().cloned().collect();
                let server_pause = stop_reason == StopReason::PauseTurn;
                history.push(AgentMessage::Assistant(assistant));

                // 服务端工具暂停：追加响应内容后继续请求，不结束回合。
                if server_pause && calls.is_empty() {
                    continue;
                }

                if calls.is_empty() {
                    // 没有工具调用：看看是否有 follow-up；有就开新轮。
                    let followups = steering.drain();
                    if followups.is_empty() {
                        break 'outer;
                    }
                    pending = followups;
                    break;
                }

                let results = self.execute_tools(&calls, events).await;
                for result in results {
                    input_delta.push_str(&render_message(&AgentMessage::Tool(result.clone())));
                    history.push(AgentMessage::Tool(result));
                }

                // 工具之后注入 steering，下一次请求前生效。
                drain_incoming(steering, &mut incoming);
                for message in steering.drain() {
                    input_delta.push_str(&render_message(&message));
                    history.push(message);
                }
            }

            if pending.is_empty() {
                break 'outer;
            }
        }

        // 正常结束：以最后一次没有工具调用的 turn 为准；若从未请求过模型则按 EndTurn。
        let outcome = LoopOutcome {
            stop_reason: StopReason::EndTurn,
            usage: total_usage,
            turns,
        };
        finish(events, &outcome).await;
        outcome
    }

    /// 构造请求。
    fn build_request(&self, history: &[AgentMessage]) -> LlmRequest {
        let mut system = self.config.system.clone();
        let mut messages = Vec::new();

        for message in history {
            match message {
                AgentMessage::System { text } => system.push(SystemBlock {
                    text: text.clone(),
                    cache: false,
                }),
                AgentMessage::User { text } => messages.push(ProviderMessage {
                    role: Role::User,
                    content: vec![ContentBlock::Text { text: text.clone() }],
                }),
                AgentMessage::Assistant(assistant) => {
                    let content = assistant
                        .content
                        .iter()
                        .map(|block| match block {
                            AssistantBlock::Text { text } => {
                                ContentBlock::Text { text: text.clone() }
                            }
                            AssistantBlock::Thinking { text, signature } => {
                                ContentBlock::Thinking {
                                    text: text.clone(),
                                    signature: signature.clone(),
                                }
                            }
                            AssistantBlock::ToolCall(call) => ContentBlock::ToolUse {
                                id: call.id.clone(),
                                name: call.name.clone(),
                                input: call.arguments.clone(),
                            },
                            AssistantBlock::ServerToolUse { id, name, input } => {
                                ContentBlock::ServerToolUse {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                }
                            }
                            AssistantBlock::ServerToolResult {
                                tool_use_id,
                                content,
                            } => ContentBlock::ServerToolResult {
                                tool_use_id: tool_use_id.clone(),
                                content: content.clone(),
                            },
                        })
                        .collect();
                    messages.push(ProviderMessage {
                        role: Role::Assistant,
                        content,
                    });
                }
                AgentMessage::Tool(result) => messages.push(ProviderMessage {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: result.tool_call_id.clone(),
                        content: result.content.clone(),
                        is_error: result.is_error,
                    }],
                }),
            }
        }

        LlmRequest {
            model: self.config.model.clone(),
            system,
            messages,
            tools: self.tools.specs(),
            max_tokens: self.config.max_tokens,
            thinking: self.config.thinking.clone(),
            server_tools: self.config.server_tools.clone(),
        }
    }

    /// 请求并按“主模型 → 回退链”的顺序重试。
    ///
    /// 每个候选模型先按 [`RetryPolicy`] 重试；全部用尽后换下一个模型。
    /// `Retry-After` 优先于指数退避，但仍受 `max_ms` 上限约束。
    async fn request_with_retry(
        &self,
        request: &LlmRequest,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<AssistantMessage, ProviderError> {
        let mut last_error: Option<ProviderError> = None;
        let candidates = std::iter::once((
            self.config.model.as_str(),
            Arc::clone(&self.provider),
            self.config.max_tokens,
        ))
        .chain(self.fallbacks.iter().map(|fallback| {
            (
                fallback.model.as_str(),
                Arc::clone(&fallback.provider),
                fallback.max_tokens,
            )
        }));

        for (index, (model, provider, max_tokens)) in candidates.enumerate() {
            let mut candidate = request.clone();
            candidate.model = model.to_owned();
            candidate.max_tokens = candidate.max_tokens.min(max_tokens);

            let mut attempt = 0u32;
            loop {
                match self.request_once(&candidate, events, &provider).await {
                    Ok(message) => return Ok(message),
                    Err(error) => {
                        if !error.retryable || attempt >= self.config.retry.attempts {
                            if index == 0 && !self.fallbacks.is_empty() {
                                warn!(
                                    model,
                                    %error,
                                    "primary model failed; trying the fallback chain"
                                );
                            }
                            last_error = Some(error);
                            break;
                        }
                        attempt += 1;
                        let exponential = self
                            .config
                            .retry
                            .base_ms
                            .saturating_mul(1u64 << attempt.min(6))
                            .min(self.config.retry.max_ms);
                        let backoff = error
                            .retry_after_ms
                            .unwrap_or(exponential)
                            .min(self.config.retry.max_ms);
                        debug!(attempt, backoff, %error, "retrying provider request");
                        tokio::time::sleep(Duration::from_millis(backoff)).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            ProviderError::new(
                crate::providers::ProviderErrorKind::Config,
                "no provider is configured",
            )
        }))
    }

    /// 单次流式请求。
    async fn request_once(
        &self,
        request: &LlmRequest,
        events: &mpsc::Sender<AgentEvent>,
        provider: &Arc<dyn Provider>,
    ) -> Result<AssistantMessage, ProviderError> {
        let mut receiver = provider.stream(request.clone()).await?;

        let mut blocks: Vec<BlockAccumulator> = Vec::new();
        let mut calls: BTreeMap<u32, CallAccumulator> = BTreeMap::new();
        let mut usage = Usage::default();
        let mut stop: Option<(StopReason, Option<crate::agent::message::RefusalDetails>)> = None;

        while let Some(event) = receiver.recv().await {
            match event {
                ProviderEvent::TextDelta(delta) => {
                    push_text(&mut blocks, &delta);
                    send(
                        events,
                        AgentEvent::MessageUpdate {
                            kind: super::event::DeltaKind::Text,
                            delta,
                        },
                    )
                    .await;
                }
                ProviderEvent::ThinkingDelta(delta) => {
                    push_thinking(&mut blocks, &delta);
                    send(
                        events,
                        AgentEvent::MessageUpdate {
                            kind: super::event::DeltaKind::Thinking,
                            delta,
                        },
                    )
                    .await;
                }
                ProviderEvent::ThinkingSignature(signature) => {
                    if let Some(BlockAccumulator::Thinking {
                        signature: slot, ..
                    }) = blocks.last_mut()
                    {
                        slot.push_str(&signature);
                    }
                }
                // Start 与 ArgumentsDelta 的先后顺序在不同兼容端点间不一致；
                // 用占位条目合并，避免参数先到时整段丢弃。
                ProviderEvent::ToolCallStart { index, id, name } => {
                    let call = calls.entry(index).or_default();
                    call.id = id;
                    call.name = name;
                }
                ProviderEvent::ToolCallArgumentsDelta { index, delta } => {
                    calls.entry(index).or_default().arguments.push_str(&delta);
                }
                ProviderEvent::ServerToolUse { id, name, input } => {
                    blocks.push(BlockAccumulator::ServerToolUse { id, name, input })
                }
                ProviderEvent::ServerToolResult {
                    tool_use_id,
                    content,
                } => blocks.push(BlockAccumulator::ServerToolResult {
                    tool_use_id,
                    content,
                }),
                ProviderEvent::Usage(update) => usage = update,
                ProviderEvent::Stop { reason, refusal } => {
                    stop = Some((reason, refusal));
                }
                ProviderEvent::Error(error) => {
                    send(
                        events,
                        AgentEvent::Error {
                            message: error.to_string(),
                        },
                    )
                    .await;
                    return Err(error);
                }
            }
        }

        let (stop_reason, refusal) = stop.ok_or_else(|| {
            ProviderError::new(
                crate::providers::ProviderErrorKind::Network,
                "stream ended without a stop event",
            )
        })?;

        // 工具调用按 index 顺序落到块列表末尾；文本/思考块保持到达顺序。
        for (_, call) in calls {
            if call.id.is_empty() || call.name.is_empty() {
                // 没有 id 或名字的调用无法配对，也无法执行；丢掉比把
                // 半截 tool_use 写进历史更安全。
                warn!(id = %call.id, name = %call.name, "dropping an incomplete tool call");
                continue;
            }
            let arguments = if call.arguments.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                match serde_json::from_str(&call.arguments) {
                    Ok(value) => value,
                    Err(error) => {
                        // 参数被截断/不是合法 JSON：以空对象落库，随后工具会
                        // 报参数缺失；`Value::Null` 会让部分 provider 直接 400。
                        warn!(
                            tool = %call.name,
                            %error,
                            "tool arguments are not valid JSON; using an empty object"
                        );
                        Value::Object(Default::default())
                    }
                }
            };
            blocks.push(BlockAccumulator::ToolCall(ToolCall {
                id: call.id,
                name: call.name,
                arguments,
            }));
        }

        let content = blocks
            .into_iter()
            .filter_map(BlockAccumulator::finish)
            .collect::<Vec<_>>();

        let message = AssistantMessage {
            content,
            stop_reason,
            usage,
            refusal,
        };
        send(
            events,
            AgentEvent::MessageEnd {
                message: message.clone(),
            },
        )
        .await;
        Ok(message)
    }

    /// 执行一批工具调用。
    async fn execute_tools(
        &self,
        calls: &[ToolCall],
        events: &mpsc::Sender<AgentEvent>,
    ) -> Vec<ToolResultMessage> {
        let sequential = calls
            .iter()
            .any(|call| self.tools.execution_mode(&call.name) == ExecutionMode::Sequential);

        if sequential {
            let mut results = Vec::with_capacity(calls.len());
            for call in calls {
                results.push(self.execute_one(call, events).await);
            }
            return results;
        }

        // 并行：join_all 保持输入顺序，结果与调用仍然一一对应。
        let futures = calls.iter().map(|call| self.execute_one(call, events));
        futures_util::future::join_all(futures).await
    }

    /// 执行单个工具。
    async fn execute_one(
        &self,
        call: &ToolCall,
        events: &mpsc::Sender<AgentEvent>,
    ) -> ToolResultMessage {
        send(
            events,
            AgentEvent::ToolExecutionStart {
                id: call.id.clone(),
                name: call.name.clone(),
            },
        )
        .await;

        let result = if call.arguments.is_null() {
            // 参数 JSON 解析失败：不执行，返回可读错误，保证调用与结果成对。
            ToolResult::error("Tool arguments were not valid JSON; the call was not executed.")
        } else {
            self.tools.execute(call).await
        };

        send(
            events,
            AgentEvent::ToolExecutionEnd {
                id: call.id.clone(),
                name: call.name.clone(),
                is_error: result.is_error,
            },
        )
        .await;

        ToolResultMessage {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: result.content,
            is_error: result.is_error,
        }
    }
}

/// 把实时通道里的消息搬进 steering 队列。
fn drain_incoming(
    steering: &mut SteeringQueue,
    incoming: &mut Option<&mut mpsc::UnboundedReceiver<AgentMessage>>,
) {
    if let Some(receiver) = incoming {
        while let Ok(message) = receiver.try_recv() {
            steering.push(message);
        }
    }
}

/// 流式块累计。
enum BlockAccumulator {
    Text(String),
    Thinking {
        text: String,
        signature: String,
    },
    ToolCall(ToolCall),
    ServerToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ServerToolResult {
        tool_use_id: String,
        content: Value,
    },
}

impl BlockAccumulator {
    fn finish(self) -> Option<AssistantBlock> {
        match self {
            Self::Text(text) if text.is_empty() => None,
            Self::Text(text) => Some(AssistantBlock::Text { text }),
            Self::Thinking { text, signature } => Some(AssistantBlock::Thinking {
                text,
                signature: if signature.is_empty() {
                    None
                } else {
                    Some(signature)
                },
            }),
            Self::ToolCall(call) => Some(AssistantBlock::ToolCall(call)),
            Self::ServerToolUse { id, name, input } => {
                Some(AssistantBlock::ServerToolUse { id, name, input })
            }
            Self::ServerToolResult {
                tool_use_id,
                content,
            } => Some(AssistantBlock::ServerToolResult {
                tool_use_id,
                content,
            }),
        }
    }
}

#[derive(Default)]
struct CallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

fn push_text(blocks: &mut Vec<BlockAccumulator>, delta: &str) {
    match blocks.last_mut() {
        Some(BlockAccumulator::Text(text)) => text.push_str(delta),
        _ => blocks.push(BlockAccumulator::Text(delta.to_owned())),
    }
}

fn push_thinking(blocks: &mut Vec<BlockAccumulator>, delta: &str) {
    match blocks.last_mut() {
        Some(BlockAccumulator::Thinking { text, .. }) => text.push_str(delta),
        _ => blocks.push(BlockAccumulator::Thinking {
            text: delta.to_owned(),
            signature: String::new(),
        }),
    }
}

/// 渲染一条消息为安全闸可读文本。
pub(crate) fn render_message(message: &AgentMessage) -> String {
    match message {
        AgentMessage::System { text } => format!("system: {text}\n"),
        AgentMessage::User { text } => format!("user: {text}\n"),
        AgentMessage::Assistant(assistant) => render_assistant(assistant),
        AgentMessage::Tool(tool) => format!(
            "tool {} ({}): {}\n",
            tool.tool_name,
            if tool.is_error { "error" } else { "ok" },
            tool.content
        ),
    }
}

fn render_assistant(assistant: &AssistantMessage) -> String {
    let mut out = String::new();
    for block in &assistant.content {
        match block {
            AssistantBlock::Text { text } => {
                out.push_str(text);
                out.push('\n');
            }
            AssistantBlock::Thinking { text, .. } => {
                // 思考也过输出闸：设计 §3.2 明确“全部输出”。
                out.push_str(text);
                out.push('\n');
            }
            AssistantBlock::ToolCall(call) => {
                out.push_str(&format!(
                    "tool_call {} {}\n",
                    call.name,
                    serde_json::to_string(&call.arguments).unwrap_or_default()
                ));
            }
            AssistantBlock::ServerToolUse { name, input, .. } => {
                out.push_str(&format!("server_tool {} {input}\n", name));
            }
            AssistantBlock::ServerToolResult { content, .. } => {
                out.push_str(&format!("server_result {content}\n"));
            }
        }
    }
    out
}

/// 裁剪较早的工具结果：保留最近 `keep_turns` 个回合起点之后的原文，
/// 更早的 `Tool` 消息只留前 `soft_limit` 个字符。
///
/// 只缩短内容、不删除消息：tool_use/tool_result 仍成对，provider 不会 400。
pub(crate) fn prune_tool_results(
    history: &mut [AgentMessage],
    keep_turns: usize,
    soft_limit: usize,
) {
    if soft_limit == 0 {
        return;
    }
    // 保留最后 `keep_turns` 个回合；第 N 个（从后往前）User 消息之前的内容
    // 属于更早的回合，可以裁剪。回合数不足时什么都不裁。
    let keep_turns = keep_turns.max(1);
    let mut seen = 0usize;
    let mut boundary = 0usize;
    for (index, message) in history.iter().enumerate().rev() {
        if matches!(message, AgentMessage::User { .. }) {
            seen += 1;
            if seen == keep_turns {
                boundary = index;
                break;
            }
        }
    }
    if boundary == 0 {
        return;
    }
    for message in history.iter_mut().take(boundary) {
        if let AgentMessage::Tool(result) = message
            && result.content.chars().count() > soft_limit
        {
            let mut trimmed: String = result.content.chars().take(soft_limit).collect();
            trimmed.push_str(
                "
[tool result trimmed]",
            );
            result.content = trimmed;
        }
    }
}

/// 渲染上下文作为闸的 `state`，只保留尾部。
fn render_state(history: &[AgentMessage], max_chars: usize) -> String {
    let mut out = String::new();
    for message in history.iter().rev() {
        let rendered = render_message(message);
        if out.len() + rendered.len() > max_chars {
            break;
        }
        out.insert_str(0, &rendered);
    }
    out
}

async fn send(events: &mpsc::Sender<AgentEvent>, event: AgentEvent) {
    let _ = events.send(event).await;
}

/// 把没有执行的工具调用补成错误结果，保持 `tool_use` / `tool_result` 成对。
///
/// Error / Refusal 可能发生在流已经产出 tool_use 之后；直接结束回合会把
/// 半对消息提交进 transcript，下一次请求就会被 provider 拒绝。
fn append_interrupted(history: &mut Vec<AgentMessage>, calls: &[ToolCall], reason: &str) {
    for call in calls {
        history.push(AgentMessage::Tool(ToolResultMessage {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: format!("Tool call was not executed: {reason}."),
            is_error: true,
        }));
    }
}

/// 结束回合：发 `AgentEnd` 并返回 outcome，避免每个提前返回点重复三行。
async fn conclude(
    events: &mpsc::Sender<AgentEvent>,
    usage: Usage,
    turns: u32,
    stop_reason: StopReason,
) -> LoopOutcome {
    let outcome = LoopOutcome {
        stop_reason,
        usage,
        turns,
    };
    finish(events, &outcome).await;
    outcome
}

async fn finish(events: &mpsc::Sender<AgentEvent>, outcome: &LoopOutcome) {
    send(
        events,
        AgentEvent::AgentEnd {
            reason: outcome.stop_reason,
            usage: outcome.usage,
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::config::models::Protocol;
    use crate::config::{GuardBackend, GuardConfig, GuardMode};
    use crate::providers::{EventReceiver, ToolSpec};

    /// 按脚本返回事件的 fake provider。
    struct FakeProvider {
        script: Mutex<VecDeque<Vec<ProviderEvent>>>,
        requests: Mutex<Vec<LlmRequest>>,
    }

    impl FakeProvider {
        fn new(script: Vec<Vec<ProviderEvent>>) -> Self {
            Self {
                script: Mutex::new(script.into()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn protocol(&self) -> Protocol {
            Protocol::Chat
        }

        fn model_id(&self) -> &str {
            "fake-model"
        }

        async fn stream(&self, request: LlmRequest) -> Result<EventReceiver, ProviderError> {
            self.requests.lock().unwrap().push(request);
            let events = self.script.lock().unwrap().pop_front().unwrap_or_else(|| {
                vec![ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                    refusal: None,
                }]
            });
            let (tx, rx) = mpsc::channel(64);
            tokio::spawn(async move {
                for event in events {
                    let _ = tx.send(event).await;
                }
            });
            Ok(rx)
        }
    }

    struct FakeTools {
        executed: Mutex<Vec<String>>,
        sequential: bool,
    }

    impl FakeTools {
        fn new(sequential: bool) -> Self {
            Self {
                executed: Mutex::new(Vec::new()),
                sequential,
            }
        }
    }

    #[async_trait]
    impl ToolExecutor for FakeTools {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "echo".to_owned(),
                description: "echo".to_owned(),
                input_schema: json!({"type":"object"}),
            }]
        }

        fn execution_mode(&self, _name: &str) -> ExecutionMode {
            if self.sequential {
                ExecutionMode::Sequential
            } else {
                ExecutionMode::Parallel
            }
        }

        async fn execute(&self, call: &ToolCall) -> ToolResult {
            self.executed.lock().unwrap().push(call.name.clone());
            ToolResult::ok(format!(
                "echo:{}",
                call.arguments["text"].as_str().unwrap_or_default()
            ))
        }
    }

    fn gate(dir: &std::path::Path, mode: GuardMode) -> Arc<Gate> {
        Arc::new(Gate::from_config(
            &GuardConfig {
                enabled: true,
                mode,
                backend: GuardBackend::Off,
                threshold: 0.9,
                ..GuardConfig::default()
            },
            None,
            None,
            crate::security::gate::hold_path(dir),
        ))
    }

    fn config() -> LoopConfig {
        LoopConfig {
            model: "fake-model".to_owned(),
            max_steps: 8,
            ..LoopConfig::default()
        }
    }

    fn text_response(text: &str) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::TextDelta(text.to_owned()),
            ProviderEvent::Usage(Usage {
                input_tokens: 10,
                output_tokens: 2,
                ..Usage::default()
            }),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
                refusal: None,
            },
        ]
    }

    fn tool_response() -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::TextDelta("calling".to_owned()),
            ProviderEvent::ToolCallStart {
                index: 0,
                id: "t1".to_owned(),
                name: "echo".to_owned(),
            },
            ProviderEvent::ToolCallArgumentsDelta {
                index: 0,
                delta: r#"{"text":"hi"}"#.to_owned(),
            },
            ProviderEvent::Stop {
                reason: StopReason::ToolUse,
                refusal: None,
            },
        ]
    }

    #[tokio::test]
    async fn simple_text_turn() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(FakeProvider::new(vec![text_response("hello")]));
        let tools = Arc::new(FakeTools::new(false));
        let agent = AgentLoop::new(
            provider.clone(),
            tools,
            gate(dir.path(), GuardMode::Shadow),
            config(),
        );
        let (tx, mut rx) = mpsc::channel(128);
        let mut history = vec![AgentMessage::user("hi")];
        let mut steering = SteeringQueue::default();
        let outcome = agent
            .run(&mut history, Vec::new(), &mut steering, &tx)
            .await;
        drop(tx);

        assert_eq!(outcome.stop_reason, StopReason::EndTurn);
        assert_eq!(outcome.usage.input_tokens, 10);
        assert_eq!(history.len(), 2);
        assert!(matches!(history[1], AgentMessage::Assistant(_)));

        let mut saw_end = false;
        while let Some(event) = rx.recv().await {
            if matches!(event, AgentEvent::AgentEnd { .. }) {
                saw_end = true;
            }
        }
        assert!(saw_end);
        // 请求里包含系统提示与工具声明。
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn tool_call_then_final_text() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(FakeProvider::new(vec![
            tool_response(),
            text_response("done"),
        ]));
        let tools = Arc::new(FakeTools::new(false));
        let agent = AgentLoop::new(
            provider.clone(),
            tools.clone(),
            gate(dir.path(), GuardMode::Shadow),
            config(),
        );
        let (tx, _rx) = mpsc::channel(128);
        let mut history = vec![AgentMessage::user("go")];
        let mut steering = SteeringQueue::default();
        let outcome = agent
            .run(&mut history, Vec::new(), &mut steering, &tx)
            .await;

        assert_eq!(outcome.stop_reason, StopReason::EndTurn);
        assert_eq!(tools.executed.lock().unwrap().as_slice(), ["echo"]);
        // user → assistant(tool) → tool result → assistant(text)
        assert_eq!(history.len(), 4);
        match &history[2] {
            AgentMessage::Tool(result) => {
                assert_eq!(result.content, "echo:hi");
                assert!(!result.is_error);
            }
            other => panic!("unexpected {other:?}"),
        }
        // 第二次请求包含工具结果。
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let has_result = requests[1].messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        });
        assert!(has_result);
    }

    #[tokio::test]
    async fn max_tokens_with_tools_never_executes() {
        let dir = tempfile::tempdir().unwrap();
        let script = vec![vec![
            ProviderEvent::ToolCallStart {
                index: 0,
                id: "t1".to_owned(),
                name: "echo".to_owned(),
            },
            ProviderEvent::ToolCallArgumentsDelta {
                index: 0,
                delta: "{\"text\":\"hi\"}".to_owned(),
            },
            ProviderEvent::Stop {
                reason: StopReason::MaxTokens,
                refusal: None,
            },
        ]];
        let provider = Arc::new(FakeProvider::new(script));
        let tools = Arc::new(FakeTools::new(false));
        let agent = AgentLoop::new(
            provider,
            tools.clone(),
            gate(dir.path(), GuardMode::Shadow),
            config(),
        );
        let (tx, _rx) = mpsc::channel(128);
        let mut history = vec![AgentMessage::user("go")];
        let outcome = agent
            .run(&mut history, Vec::new(), &mut SteeringQueue::default(), &tx)
            .await;

        assert_eq!(outcome.stop_reason, StopReason::MaxTokens);
        assert!(tools.executed.lock().unwrap().is_empty());
        match &history[2] {
            AgentMessage::Tool(result) => {
                assert!(result.is_error);
                assert!(result.content.contains("max_tokens"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn refusal_enters_hold_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let script = vec![vec![
            ProviderEvent::TextDelta("nope".to_owned()),
            ProviderEvent::Stop {
                reason: StopReason::Refusal,
                refusal: Some(crate::agent::message::RefusalDetails {
                    category: "cyber".to_owned(),
                    explanation: "policy".to_owned(),
                }),
            },
        ]];
        let provider = Arc::new(FakeProvider::new(script));
        let tools = Arc::new(FakeTools::new(false));
        let gate = gate(dir.path(), GuardMode::Enforce);
        let agent = AgentLoop::new(provider, tools, gate.clone(), config());
        let (tx, _rx) = mpsc::channel(128);
        let mut history = vec![AgentMessage::user("go")];
        let outcome = agent
            .run(&mut history, Vec::new(), &mut SteeringQueue::default(), &tx)
            .await;

        assert_eq!(outcome.stop_reason, StopReason::Refusal);
        assert!(gate.hold_active());
    }

    #[tokio::test]
    async fn output_gate_blocks_before_tool_execution() {
        let dir = tempfile::tempdir().unwrap();
        let script = vec![vec![
            ProviderEvent::TextDelta("sk-abcdefghijklmnopqrstuvwx".to_owned()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
                refusal: None,
            },
        ]];
        let provider = Arc::new(FakeProvider::new(script));
        let tools = Arc::new(FakeTools::new(false));
        let gate = gate(dir.path(), GuardMode::Enforce);
        let agent = AgentLoop::new(provider, tools.clone(), gate.clone(), config());
        let (tx, _rx) = mpsc::channel(128);
        let mut history = vec![AgentMessage::user("go")];
        let outcome = agent
            .run(&mut history, Vec::new(), &mut SteeringQueue::default(), &tx)
            .await;

        assert_eq!(outcome.stop_reason, StopReason::Aborted);
        assert!(gate.hold_active());
        assert!(tools.executed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn steering_is_injected_before_next_request() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(FakeProvider::new(vec![
            tool_response(),
            text_response("done"),
        ]));
        let tools = Arc::new(FakeTools::new(true));
        let agent = AgentLoop::new(
            provider.clone(),
            tools,
            gate(dir.path(), GuardMode::Shadow),
            config(),
        );
        let (tx, _rx) = mpsc::channel(128);
        let mut history = vec![AgentMessage::user("go")];
        let mut steering = SteeringQueue::default();
        steering.push(AgentMessage::user("also this"));

        let outcome = agent
            .run(&mut history, Vec::new(), &mut steering, &tx)
            .await;
        assert_eq!(outcome.stop_reason, StopReason::EndTurn);

        let requests = provider.requests.lock().unwrap();
        let second_has_steer = requests[1].messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text } if text == "also this"))
        });
        assert!(second_has_steer);
    }

    #[tokio::test]
    async fn retryable_error_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(FakeProvider::new(vec![
            vec![ProviderEvent::Error(ProviderError {
                kind: crate::providers::ProviderErrorKind::Server,
                message: "temporary".to_owned(),
                status: Some(500),
                retryable: true,
                retry_after_ms: None,
            })],
            text_response("recovered"),
        ]));
        let tools = Arc::new(FakeTools::new(false));
        let mut config = config();
        config.retry.base_ms = 1;
        let agent = AgentLoop::new(
            provider.clone(),
            tools,
            gate(dir.path(), GuardMode::Shadow),
            config,
        );
        let (tx, _rx) = mpsc::channel(128);
        let mut history = vec![AgentMessage::user("go")];
        let outcome = agent
            .run(&mut history, Vec::new(), &mut SteeringQueue::default(), &tx)
            .await;

        assert_eq!(outcome.stop_reason, StopReason::EndTurn);
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn falls_back_to_the_next_model() {
        let dir = tempfile::tempdir().unwrap();
        let primary = Arc::new(FakeProvider::new(vec![vec![ProviderEvent::Error(
            ProviderError::new(crate::providers::ProviderErrorKind::Auth, "no key")
                .with_status(401),
        )]]));
        let backup = Arc::new(FakeProvider::new(vec![text_response("from backup")]));
        let tools = Arc::new(FakeTools::new(false));
        let mut config = config();
        config.model = "primary-model".to_owned();
        let agent = AgentLoop::new_with_fallbacks(
            primary.clone(),
            vec![FallbackProvider {
                model: "backup-model".to_owned(),
                provider: backup.clone(),
                max_tokens: 99,
            }],
            tools,
            gate(dir.path(), GuardMode::Shadow),
            config,
        );
        let (tx, _rx) = mpsc::channel(128);
        let mut history = vec![AgentMessage::user("go")];
        let outcome = agent
            .run(&mut history, Vec::new(), &mut SteeringQueue::default(), &tx)
            .await;

        assert_eq!(outcome.stop_reason, StopReason::EndTurn);
        // 主模型只尝试一次（401 不可重试），随后回退。
        assert_eq!(primary.requests.lock().unwrap().len(), 1);
        let backup_requests = backup.requests.lock().unwrap();
        assert_eq!(backup_requests.len(), 1);
        assert_eq!(backup_requests[0].model, "backup-model");
        assert_eq!(backup_requests[0].max_tokens, 99);
        assert!(history.iter().any(|message| matches!(
            message,
            AgentMessage::Assistant(assistant) if assistant.content.iter().any(|block| matches!(
                block,
                AssistantBlock::Text { text } if text == "from backup"
            ))
        )));
    }

    fn tool_call_events() -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::ToolCallStart {
                index: 0,
                id: "c1".to_owned(),
                name: "echo".to_owned(),
            },
            ProviderEvent::ToolCallArgumentsDelta {
                index: 0,
                delta: r#"{"text":"hi"}"#.to_owned(),
            },
        ]
    }

    async fn run_script(
        script: Vec<Vec<ProviderEvent>>,
    ) -> (Vec<AgentMessage>, Arc<FakeTools>, LoopOutcome) {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(FakeProvider::new(script));
        let tools = Arc::new(FakeTools::new(false));
        let gate = gate(dir.path(), GuardMode::Shadow);
        let agent = AgentLoop::new(provider, tools.clone(), gate, config());
        let (tx, _rx) = mpsc::channel(128);
        let mut history = vec![AgentMessage::user("go")];
        let outcome = agent
            .run(&mut history, Vec::new(), &mut SteeringQueue::default(), &tx)
            .await;
        (history, tools, outcome)
    }

    #[tokio::test]
    async fn refusal_after_tool_call_still_pairs_results() {
        let mut script = tool_call_events();
        script.push(ProviderEvent::Stop {
            reason: StopReason::Refusal,
            refusal: Some(crate::agent::message::RefusalDetails {
                category: "cyber".to_owned(),
                explanation: "policy".to_owned(),
            }),
        });
        let (history, tools, outcome) = run_script(vec![script]).await;

        assert_eq!(outcome.stop_reason, StopReason::Refusal);
        assert!(tools.executed.lock().unwrap().is_empty());
        let tool_results: Vec<_> = history
            .iter()
            .filter_map(|message| match message {
                AgentMessage::Tool(result) => Some(result),
                _ => None,
            })
            .collect();
        assert_eq!(tool_results.len(), 1);
        assert!(tool_results[0].is_error);
        assert_eq!(tool_results[0].tool_call_id, "c1");
    }

    #[tokio::test]
    async fn provider_error_after_tool_call_still_pairs_results() {
        let mut script = tool_call_events();
        script.push(ProviderEvent::Stop {
            reason: StopReason::Error,
            refusal: None,
        });
        let (history, tools, outcome) = run_script(vec![script]).await;

        assert_eq!(outcome.stop_reason, StopReason::Error);
        assert!(tools.executed.lock().unwrap().is_empty());
        assert!(history.iter().any(|message| matches!(
            message,
            AgentMessage::Tool(result) if result.is_error && result.tool_call_id == "c1"
        )));
    }

    #[tokio::test]
    async fn arguments_before_start_are_not_dropped() {
        let script = vec![
            vec![
                ProviderEvent::ToolCallArgumentsDelta {
                    index: 0,
                    delta: r#"{"text":"hi"}"#.to_owned(),
                },
                ProviderEvent::ToolCallStart {
                    index: 0,
                    id: "c1".to_owned(),
                    name: "echo".to_owned(),
                },
                ProviderEvent::Stop {
                    reason: StopReason::ToolUse,
                    refusal: None,
                },
            ],
            text_response("done"),
        ];
        let (history, tools, outcome) = run_script(script).await;

        assert_eq!(outcome.stop_reason, StopReason::EndTurn);
        assert_eq!(tools.executed.lock().unwrap().len(), 1);
        assert!(history.iter().any(|message| matches!(
            message,
            AgentMessage::Tool(result) if !result.is_error && result.content == "echo:hi"
        )));
    }

    #[tokio::test]
    async fn incomplete_tool_calls_are_dropped_instead_of_persisted() {
        let script = vec![vec![
            ProviderEvent::ToolCallStart {
                index: 0,
                id: String::new(),
                name: "echo".to_owned(),
            },
            ProviderEvent::Stop {
                reason: StopReason::ToolUse,
                refusal: None,
            },
        ]];
        let (history, tools, outcome) = run_script(script).await;

        assert_eq!(outcome.stop_reason, StopReason::EndTurn);
        assert!(tools.executed.lock().unwrap().is_empty());
        assert!(!history.iter().any(|message| matches!(
            message,
            AgentMessage::Assistant(assistant)
                if assistant.content.iter().any(|block| matches!(block, AssistantBlock::ToolCall(_)))
        )));
    }

    #[test]
    fn prune_shortens_only_old_tool_results() {
        let long = "x".repeat(100);
        let mut history = vec![
            AgentMessage::user("turn 1"),
            AgentMessage::Assistant(AssistantMessage {
                content: vec![AssistantBlock::ToolCall(crate::agent::message::ToolCall {
                    id: "old".to_owned(),
                    name: "Read".to_owned(),
                    arguments: serde_json::json!({"path": "/a"}),
                })],
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
                refusal: None,
            }),
            AgentMessage::Tool(ToolResultMessage {
                tool_call_id: "old".to_owned(),
                tool_name: "Read".to_owned(),
                content: long.clone(),
                is_error: false,
            }),
            AgentMessage::user("turn 2"),
            AgentMessage::Tool(ToolResultMessage {
                tool_call_id: "recent".to_owned(),
                tool_name: "Read".to_owned(),
                content: long.clone(),
                is_error: false,
            }),
        ];

        prune_tool_results(&mut history, 1, 20);
        match &history[2] {
            AgentMessage::Tool(result) => {
                assert!(result.content.starts_with(&"x".repeat(20)));
                assert!(result.content.ends_with("[tool result trimmed]"));
            }
            other => panic!("unexpected {other:?}"),
        }
        // 最近的回合与工具结果不受影响。
        assert_eq!(history.len(), 5);
        match &history[4] {
            AgentMessage::Tool(result) => assert_eq!(result.content, long),
            other => panic!("unexpected {other:?}"),
        }
    }
}
