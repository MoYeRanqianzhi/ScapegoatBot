// src/swarm/runner.rs
// 子代理运行器（SubAgentRunner）
// 子代理的核心 agentic 循环实现：接收任务 → LLM 调用 → 工具分发 → 重复直到 Finish
//
// 与主代理 AgentRunner 的主要区别：
// - 无去抖/批量逻辑（子代理只处理单一任务）
// - 无 QQ 消息通道（子代理不直接接收 QQ 消息）
// - 无会话持久化（对话仅保存在内存中）
// - 有深度限制的嵌套 spawn 能力（子代理可以创建更深层的子代理）
// - 通过 Finish 工具的 result 字段向父代理传递结果

// ============================================================================
// 导入
// ============================================================================

use crate::providers::traits::{
    ChatMessage, ChatRequest, Provider, ToolSpec,   // LLM 交互类型
};
use crate::swarm::dispatch::ToolDispatcher;       // 工具分发器
use crate::swarm::types::{
    AgentMessage, AgentStatus, SwarmNotification,   // 蜂群通信类型
};
use crate::swarm::SwarmManager;                    // 蜂群管理器
use std::sync::Arc;                                // 原子引用计数
use tokio::sync::mpsc;                             // 异步通道

// ============================================================================
// 子代理运行器
// ============================================================================

/// 子代理运行器
/// 持有独立的 Provider、对话历史和工具分发器
/// 在独立的 tokio 任务中运行 agentic 循环
pub struct SubAgentRunner {
    /// LLM 提供商实例（独立于主代理的提供商）
    provider: Box<dyn Provider>,
    /// 使用的模型名称（用于 LLM API 调用）
    model: String,
    /// 系统指令角色名称（"system" 或 "developer"）
    system_role_name: String,
    /// 最大工具调用步数（安全上限，防止无限循环）
    max_steps: u32,
    /// 工具分发器（持有 workspace_tools、workspace_session、mcp_manager 等）
    dispatcher: ToolDispatcher,
    /// LLM 可调用的工具规范列表
    tool_specs: Arc<Vec<ToolSpec>>,
    /// 对话历史（仅内存，不持久化）
    conversation: Vec<ChatMessage>,
    /// 系统提示词
    system_prompt: String,
    /// 接收父代理消息的通道接收端
    rx: mpsc::UnboundedReceiver<AgentMessage>,
    /// 运行状态的共享引用（与 SwarmManager 中的 AgentHandle 共享）
    status: Arc<std::sync::Mutex<AgentStatus>>,
    /// 完成通知的发送通道（子代理 → 主代理）
    notify_tx: mpsc::UnboundedSender<SwarmNotification>,
    /// 蜂群管理器引用（用于子代理的 SpawnAgent 等蜂群工具）
    swarm: Arc<SwarmManager>,
    /// 当前嵌套深度（主代理 spawn 的子代理 depth=1，子代理 spawn 的 depth=2，...）
    depth: u32,
    /// 子代理名称
    name: String,
    /// 模型显示名称（用于通知和日志）
    model_display: String,
}

impl SubAgentRunner {
    /// 创建子代理运行器
    ///
    /// # 参数
    /// - `provider`: LLM 提供商实例
    /// - `model`: 模型名称
    /// - `system_role_name`: 系统指令角色名
    /// - `max_steps`: 最大步数
    /// - `dispatcher`: 工具分发器
    /// - `tool_specs`: 工具规范列表
    /// - `system_prompt`: 系统提示词
    /// - `task`: 任务描述（作为首条 user 消息）
    /// - `rx`: 父代理消息接收通道
    /// - `status`: 运行状态共享引用
    /// - `notify_tx`: 完成通知发送通道
    /// - `swarm`: 蜂群管理器引用
    /// - `depth`: 嵌套深度
    /// - `name`: 子代理名称
    /// - `model_display`: 模型显示名称
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Box<dyn Provider>,
        model: String,
        system_role_name: String,
        max_steps: u32,
        dispatcher: ToolDispatcher,
        tool_specs: Arc<Vec<ToolSpec>>,
        system_prompt: String,
        task: String,
        rx: mpsc::UnboundedReceiver<AgentMessage>,
        status: Arc<std::sync::Mutex<AgentStatus>>,
        notify_tx: mpsc::UnboundedSender<SwarmNotification>,
        swarm: Arc<SwarmManager>,
        depth: u32,
        name: String,
        model_display: String,
    ) -> Self {
        // 将任务描述作为首条 user 消息放入对话历史
        let conversation = vec![ChatMessage::user(task)];

        Self {
            provider,
            model,
            system_role_name,
            max_steps,
            dispatcher,
            tool_specs,
            conversation,
            system_prompt,
            rx,
            status,
            notify_tx,
            swarm,
            depth,
            name,
            model_display,
        }
    }

    /// 运行子代理的 agentic 循环
    /// 这是子代理的核心逻辑，在独立的 tokio 任务中执行
    ///
    /// 循环流程：
    /// 1. 检查父代理消息（try_recv 非阻塞）
    /// 2. 检查步数限制
    /// 3. 构建请求消息（system + conversation）
    /// 4. 调用 LLM
    /// 5. 处理工具调用（Finish / 蜂群工具 / 普通工具）
    /// 6. 无工具调用时设置重试提醒
    /// 7. Finish 被调用时跳出循环
    /// 8. 发送完成通知
    pub async fn run(mut self) {
        tracing::info!(
            "子代理 `{}` (depth={}, model={}) 开始运行",
            self.name, self.depth, self.model_display
        );

        // 步数计数器
        let mut steps = 0u32;
        // Finish 工具是否被调用
        let mut finish_called = false;
        // 完成结果（Finish 的 result 参数值）
        let mut finish_result = String::new();
        // 临时重试提醒（LLM 未调用工具时追加，不写入 conversation）
        let mut retry_reminder: Option<ChatMessage> = None;
        // 是否因关闭指令退出
        let mut shutdown_requested = false;

        loop {
            // ===== 第 1 步：检查父代理消息 =====
            // 使用 try_recv 非阻塞接收，不阻塞 agentic 循环
            while let Ok(msg) = self.rx.try_recv() {
                match msg {
                    AgentMessage::UserMessage(text) => {
                        // 将父代理消息注入对话历史
                        tracing::info!(
                            "子代理 `{}` 收到父代理消息 ({}字符)",
                            self.name, text.len()
                        );
                        self.conversation.push(ChatMessage::user(
                            format!("[父代理消息] {}", text)
                        ));
                    }
                    AgentMessage::Shutdown => {
                        // 收到关闭指令，优雅退出
                        tracing::info!("子代理 `{}` 收到关闭指令", self.name);
                        shutdown_requested = true;
                    }
                }
            }

            // 如果收到关闭指令，立即退出
            if shutdown_requested {
                finish_result = "子代理被父代理关闭".to_string();
                break;
            }

            // ===== 第 2 步：检查步数限制 =====
            if steps >= self.max_steps {
                tracing::warn!(
                    "子代理 `{}` 达到最大步数 ({})，强制停止",
                    self.name, self.max_steps
                );
                finish_result = format!(
                    "子代理达到最大步数限制 ({})，被强制停止",
                    self.max_steps
                );
                break;
            }
            steps += 1;

            // ===== 第 3 步：构建请求消息 =====
            // [系统消息, ...对话历史, (可选)重试提醒]
            let mut request_messages =
                Vec::with_capacity(1 + self.conversation.len() + 1);
            // 系统消息使用配置的角色名称
            request_messages.push(ChatMessage {
                role: self.system_role_name.clone(),
                content: self.system_prompt.clone(),
                tool_calls: Vec::new(),
                tool_call_id: None,
                images: Vec::new(),
            });
            // 追加对话历史
            request_messages.extend(self.conversation.iter().cloned());
            // 追加重试提醒（如果有）
            if let Some(reminder) = retry_reminder.take() {
                request_messages.push(reminder);
            }

            // 构建请求
            let request = ChatRequest {
                messages: &request_messages,
                tools: Some(&self.tool_specs),
            };

            // ===== 第 4 步：调用 LLM =====
            tracing::info!(
                "子代理 `{}` 调用 LLM (step {}/{}, 对话 {} 条, 请求 {} 条)",
                self.name, steps, self.max_steps,
                self.conversation.len(), request_messages.len()
            );

            let response = match self.provider.chat(request, &self.model, 0.7).await {
                Ok(r) => r,
                Err(e) => {
                    // LLM 调用失败，记录错误并退出
                    tracing::error!("子代理 `{}` LLM 调用失败: {}", self.name, e);
                    // 更新状态为失败
                    {
                        let mut st = self.status.lock().unwrap_or_else(|e| e.into_inner());
                        *st = AgentStatus::Failed {
                            error: format!("LLM 调用失败: {e}"),
                        };
                    }
                    // 发送失败通知
                    let _ = self.notify_tx.send(SwarmNotification {
                        agent_name: self.name.clone(),
                        model_display: self.model_display.clone(),
                        result: format!("LLM 调用失败: {e}"),
                        success: false,
                    });
                    return; // 直接返回，不执行后续清理
                }
            };

            // ===== 第 5 步：处理响应 =====
            if response.has_tool_calls() {
                // LLM 请求执行工具调用
                let assistant_content = response.text.unwrap_or_default();
                if !assistant_content.is_empty() {
                    tracing::info!("子代理 `{}` LLM 附带文本: {}", self.name, assistant_content);
                }
                // 记录助手消息（含工具调用）到对话历史
                self.conversation.push(ChatMessage::assistant_with_tool_calls(
                    assistant_content,
                    response.tool_calls.clone(),
                ));

                // 逐个处理工具调用
                for tc in &response.tool_calls {
                    // ===== Finish 工具：提取结果，标记完成 =====
                    if tc.name == "Finish" {
                        // 解析 Finish 参数，提取 result 字段
                        let result = serde_json::from_str::<serde_json::Value>(&tc.arguments)
                            .ok()
                            .and_then(|v| {
                                v.get("result")
                                    .and_then(|r| r.as_str())
                                    .map(|s| s.to_string())
                            })
                            .unwrap_or_else(|| {
                                // 如果没有 result 字段，尝试 reason 字段（兼容）
                                serde_json::from_str::<serde_json::Value>(&tc.arguments)
                                    .ok()
                                    .and_then(|v| {
                                        v.get("reason")
                                            .and_then(|r| r.as_str())
                                            .map(|s| s.to_string())
                                    })
                                    .unwrap_or_else(|| "(无结果)".to_string())
                            });
                        tracing::info!("子代理 `{}` 调用 Finish({})", self.name, result);
                        finish_result = result;
                        finish_called = true;
                        // 记录工具结果到对话历史（保持对话完整性）
                        self.conversation.push(ChatMessage::tool_result(
                            &tc.id,
                            "任务已完成，结果已传递给父代理。".to_string(),
                        ));
                        continue;
                    }

                    // ===== 蜂群工具：路由到 SwarmManager =====
                    if matches!(
                        tc.name.as_str(),
                        "SpawnAgent" | "MessageAgent" | "CheckAgent" | "WaitAgent"
                    ) {
                        // 解析工具参数
                        let args: serde_json::Value = match serde_json::from_str(&tc.arguments) {
                            Ok(v) => v,
                            Err(e) => {
                                // 参数解析失败
                                self.conversation.push(ChatMessage::tool_result(
                                    &tc.id,
                                    format!("参数解析失败: {e}"),
                                ));
                                continue;
                            }
                        };
                        tracing::debug!(
                            "子代理 `{}` 蜂群工具调用: {} (depth={})",
                            self.name, tc.name, self.depth
                        );
                        // 调用蜂群工具分发函数（传入当前深度）
                        let result = crate::swarm::tools::dispatch_swarm_tool(
                            &self.swarm,
                            &tc.name,
                            &args,
                            self.depth,
                        )
                        .await;
                        // 记录工具结果
                        self.conversation
                            .push(ChatMessage::tool_result(&tc.id, result));
                        continue;
                    }

                    // ===== 普通工具：委托给 ToolDispatcher =====
                    tracing::debug!(
                        "子代理 `{}` 工具调用: {} (args_len={})",
                        self.name, tc.name, tc.arguments.len()
                    );
                    let result = self.dispatcher.dispatch_tool(&tc.name, &tc.arguments).await;
                    tracing::debug!(
                        "子代理 `{}` 工具结果 ({}): {}",
                        self.name, tc.name, &result.text[..result.text.len().min(200)]
                    );
                    // 追加工具结果到对话历史（支持多模态图片返回）
                    if result.images.is_empty() {
                        self.conversation
                            .push(ChatMessage::tool_result(&tc.id, result.text));
                    } else {
                        self.conversation.push(ChatMessage::tool_result_with_images(
                            &tc.id,
                            result.text,
                            result.images,
                        ));
                    }
                }

                // 如果本批工具调用中包含 Finish，结束循环
                if finish_called {
                    break;
                }
            } else {
                // ===== 第 6 步：无工具调用时设置重试提醒 =====
                // LLM 返回纯文本但未调用任何工具
                let text = response.text.unwrap_or_default();
                if !text.is_empty() {
                    // 记录 LLM 的文本输出到对话历史
                    self.conversation.push(ChatMessage::assistant(&text));
                }
                tracing::info!(
                    "子代理 `{}` step {} 未调用工具 (文本: {} 字符)",
                    self.name, steps, text.len()
                );
                // 设置临时提醒（不写入对话历史，仅在下次请求时追加）
                retry_reminder = Some(ChatMessage::user(
                    "[系统提醒] 你没有调用任何工具就结束了。你的纯文本输出不会被任何人看到。\
                    请继续使用工具完成任务。当你确认所有工作已完成时，调用 Finish 工具（在 result 字段中写明结果）结束任务。".to_string()
                ));
                // 继续循环
            }
        }

        // ===== 第 7 步：退出后清理 =====

        // 关闭自身生成的子代理（如果有）
        // 注意：子代理也可以 spawn 子代理（嵌套），需要级联关闭

        // 更新状态
        let success = if shutdown_requested {
            // 被关闭的子代理视为成功完成
            let mut st = self.status.lock().unwrap_or_else(|e| e.into_inner());
            *st = AgentStatus::Completed {
                result: finish_result.clone(),
            };
            true
        } else if finish_called {
            // 正常完成
            let mut st = self.status.lock().unwrap_or_else(|e| e.into_inner());
            *st = AgentStatus::Completed {
                result: finish_result.clone(),
            };
            true
        } else {
            // 因步数限制等原因退出
            let mut st = self.status.lock().unwrap_or_else(|e| e.into_inner());
            *st = AgentStatus::Failed {
                error: finish_result.clone(),
            };
            false
        };

        // 发送完成通知给父代理
        let _ = self.notify_tx.send(SwarmNotification {
            agent_name: self.name.clone(),
            model_display: self.model_display.clone(),
            result: finish_result,
            success,
        });

        tracing::info!(
            "子代理 `{}` 已退出 (steps={}, success={})",
            self.name, steps, success
        );
    }
}
