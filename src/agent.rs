// src/agent.rs
// Agent 核心循环模块
// 接收消息事件 → 批量去抖 → 构建上下文 → 调用 LLM → 执行工具调用 → 发送消息
//
// 架构：
// - 事件监听器通过 mpsc 通道将消息事件发送给 Agent
// - Agent 收到第一条消息后启动去抖窗口，批量收集消息
// - 批量消息格式化为一条 user 消息，追加到对话历史
// - 进入 agentic 循环：压缩 → 构建请求 → 调用 LLM → 分发工具调用 → 重复直到 end_turn
// - 工具调用结果追加到对话历史，作为 tool 消息回传给 LLM

use crate::agent_state::AgentState;
use crate::compact::{self, CompactionState};
use crate::config::AppConfig;
use crate::prompt::{self, AgentsMd, PromptConfig};
use crate::providers::anthropic::AnthropicProvider;
use crate::providers::openai::OpenAiProvider;
use crate::providers::traits::{ChatMessage, ChatRequest, ImageData, Provider, ToolSpec};
use crate::storage::session::SessionManager;
use crate::tools::ToolContext;
use crate::tools::workspace::{WorkspaceToolSession, WorkspaceTools};
use napcat_link::MessageSegment;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

// ============================================================================
// 消息事件
// ============================================================================

/// 传递给 Agent 的消息事件通知
/// 由事件监听器在存储消息后发送
#[derive(Debug, Clone)]
pub struct MessageEvent {
    /// 格式化后的消息文本（与存储到 MD 文件的内容一致）
    pub formatted: String,
}

// ============================================================================
// 工具执行结果
// ============================================================================

/// 工具执行的结构化结果
/// 支持纯文本和多模态（文本 + 图片）两种返回形式
/// ReadImage 等多模态工具返回带图片的结果，其他工具返回纯文本
struct ToolResult {
    /// 文本输出（所有工具都会产生文本输出）
    text: String,
    /// 附带的图片列表（为空表示纯文本结果）
    images: Vec<ImageData>,
}

impl ToolResult {
    /// 创建纯文本结果（大多数工具使用此方法）
    fn text(s: impl Into<String>) -> Self {
        Self {
            text: s.into(),
            images: Vec::new(),
        }
    }

    /// 创建带图片的多模态结果
    fn with_images(s: impl Into<String>, images: Vec<ImageData>) -> Self {
        Self {
            text: s.into(),
            images,
        }
    }
}

// ============================================================================
// Agent 运行器
// ============================================================================

/// Agent 运行器
/// 持有 LLM 提供商、对话历史、工具上下文等全部运行时状态
/// 通过 `run()` 进入主循环，处理消息事件直到通道关闭
pub struct AgentRunner {
    // ----- LLM 相关 -----
    /// LLM 提供商（动态分发：OpenAI 或 Anthropic）
    provider: Box<dyn Provider>,
    /// 使用的模型名称
    model: String,
    /// 系统指令角色名称（"system" 或 "developer"）
    system_role_name: String,
    /// 推理努力程度（可选）
    reasoning_effort: Option<String>,
    /// 最大工具调用步数
    max_steps: u32,

    // ----- 配置 -----
    /// 全局应用配置
    config: Arc<AppConfig>,

    // ----- 上下文 -----
    /// 工具执行上下文（API + 白名单 + Skill）
    tool_ctx: Arc<ToolContext>,
    /// 工作区文件工具（cap-std 沙箱）
    workspace_tools: WorkspaceTools,
    /// 工作区工具会话状态（Read/Edit 前置条件追踪）
    workspace_session: WorkspaceToolSession,
    /// Agent 运行状态（活跃/暂停）
    agent_state: Arc<AgentState>,

    // ----- 会话 -----
    /// 会话管理器（JSONL 持久化 + 内存缓存 + SQLite 索引）
    /// 替代之前的 compaction_state + conversation 字段
    session: SessionManager,
    /// 系统提示词（启动时构建，每轮刷新时间）
    system_prompt: String,

    // ----- 通道 -----
    /// 接收消息事件的通道
    rx: mpsc::UnboundedReceiver<MessageEvent>,

    // ----- 工具定义 -----
    /// LLM 可调用的工具规范列表
    tool_specs: Vec<ToolSpec>,
}

impl AgentRunner {
    /// 创建 Agent 运行器
    ///
    /// # 参数
    /// - `config`: 全局应用配置
    /// - `tool_ctx`: 工具执行上下文
    /// - `agent_state`: Agent 运行状态
    /// - `rx`: 消息事件接收通道
    pub fn new(
        config: Arc<AppConfig>,
        tool_ctx: Arc<ToolContext>,
        agent_state: Arc<AgentState>,
        rx: mpsc::UnboundedReceiver<MessageEvent>,
    ) -> anyhow::Result<Self> {
        // 从配置创建 LLM 提供商（根据 provider 字段选择具体实现）
        // 凭据处理：空字符串视为 None
        let credential: Option<&str> = if config.llm.api_key.is_empty() {
            None
        } else {
            Some(&config.llm.api_key)
        };

        // 工厂构造：根据 provider 配置选择 OpenAI 或 Anthropic
        let provider: Box<dyn Provider> =
            match config.llm.provider.to_ascii_lowercase().as_str() {
                // Anthropic Messages API（Claude 系列模型）
                "anthropic" => Box::new(AnthropicProvider::new(
                    Some(&config.llm.base_url),
                    credential,
                    config.llm.effective_anthropic_max_tokens(),
                    config.llm.timeout_secs,
                )),
                // OpenAI 兼容 API（默认，含 ChatCompletions / Responses 双协议）
                _ => Box::new(OpenAiProvider::new(
                    Some(&config.llm.base_url),
                    credential,
                    config.llm.max_tokens, // 从配置读取（之前硬编码 None）
                    config.llm.timeout_secs,
                    &config.llm.wire_api,
                )),
            };

        // 同步加载 AGENTS.md
        let agents_md_path =
            std::path::PathBuf::from(&config.storage.workspace_dir).join("AGENTS.md");
        let agents_md = match std::fs::read_to_string(&agents_md_path) {
            Ok(content) => AgentsMd {
                path: agents_md_path,
                content,
                found: true,
            },
            Err(_) => AgentsMd {
                path: agents_md_path,
                content: String::new(),
                found: false,
            },
        };

        // 创建工作区文件工具（cap-std 沙箱）
        let workspace_dir = Path::new(&config.storage.workspace_dir);
        let workspace_tools = WorkspaceTools::new(workspace_dir)?;

        // 初始化会话管理器（JSONL 持久化 + SQLite 索引）
        let session = SessionManager::open(workspace_dir)?;

        // 获取 Skill 列表用于系统提示
        let skill_items = tool_ctx.skills.list();

        // 构建系统提示（含前一个会话引用）
        let prompt_config = PromptConfig {
            workspace_dir,
            model_name: &config.llm.model,
            agents_md: &agents_md,
            extra_system_prompt: None,
            master_display: config.identity.master_display(),
            skills: &skill_items,
            send_groups: &config.whitelist.send_groups,
            send_privates: &config.whitelist.send_privates,
            recv_groups: &config.whitelist.recv_groups,
            recv_privates: &config.whitelist.recv_privates,
            prev_session_path: session.prev_session_path(),
            ai_boot: true,
            unprocessed_ranges: vec![],
        };
        let system_prompt = prompt::build_system_prompt(&prompt_config);

        // 构建工具规范
        let tool_specs = build_tool_specs();

        Ok(Self {
            provider,
            model: config.llm.model.clone(),
            system_role_name: config.llm.effective_system_role_name().to_string(),
            reasoning_effort: config.llm.effective_reasoning_effort().map(str::to_string),
            max_steps: config.llm.max_steps,
            config,
            tool_ctx,
            workspace_tools,
            workspace_session: WorkspaceToolSession::default(),
            agent_state,
            session,
            system_prompt,
            rx,
            tool_specs,
        })
    }

    /// Agent 主循环
    /// 持续接收消息事件 → 批量去抖 → 处理轮次
    /// 直到消息通道关闭
    pub async fn run(mut self) {
        tracing::info!("Agent 循环已启动");

        loop {
            // 等待第一条消息
            let first = match self.rx.recv().await {
                Some(msg) => msg,
                None => {
                    tracing::info!("Agent 消息通道已关闭，退出循环");
                    break;
                }
            };

            // 去抖：等待一小段时间，收集更多消息
            let mut batch = vec![first];
            let debounce = tokio::time::Duration::from_secs(3);
            let deadline = tokio::time::Instant::now() + debounce;

            loop {
                tokio::select! {
                    msg = self.rx.recv() => {
                        match msg {
                            Some(m) => batch.push(m),
                            None => break,
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => break,
                }
            }

            // 检查 Agent 是否活跃
            if !self.agent_state.is_active() {
                tracing::debug!("Agent 已暂停，跳过 {} 条消息", batch.len());
                continue;
            }

            // 检查 LLM 是否已配置
            if !self.config.llm.is_configured() {
                tracing::warn!("LLM 未配置（base_url 或 api_key 为空），跳过处理");
                continue;
            }

            tracing::info!("Agent 开始处理 {} 条消息", batch.len());
        // 输出每条消息的摘要
        for (i, msg) in batch.iter().enumerate() {
            // 截取前 200 字符作为预览
            let preview: String = msg.formatted.chars().take(200).collect();
            tracing::info!("  消息[{}]: {}", i + 1, preview);
        }

            // 处理轮次
            if let Err(e) = self.process_turn(batch).await {
                tracing::error!("Agent 处理轮次失败: {}", e);
            }
        }

        tracing::info!("Agent 循环已退出");
    }

    /// 处理一批消息事件
    /// 将消息批次格式化为用户消息 → 进入 agentic 循环
    async fn process_turn(&mut self, batch: Vec<MessageEvent>) -> anyhow::Result<()> {
        // 将批次中的所有消息格式化为一条 user 消息
        let user_content = if batch.len() == 1 {
            batch[0].formatted.clone()
        } else {
            batch
                .iter()
                .map(|e| e.formatted.as_str())
                .collect::<Vec<_>>()
                .join("\n\n---\n\n")
        };

        // 双写到会话（JSONL 文件 + 内存缓存）
        self.session.push(ChatMessage::user(user_content))?;

        // 刷新系统提示中的日期时间
        prompt::refresh_prompt_datetime(&mut self.system_prompt);

        // 构建系统消息（使用配置的角色名称）
        let system_message = ChatMessage {
            role: self.system_role_name.clone(),
            content: self.system_prompt.clone(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            images: Vec::new(),
        };

        // Agentic 循环
        let mut steps = 0u32;
        // Finish 工具作为显式停止钩子：只有 LLM 调用 Finish 才能结束轮次
        let mut finish_called = false;
        // 临时提醒消息：当 LLM 未调用任何工具时，在下一次请求中临时追加此提醒
        // **不写入 session**，避免断流/重试场景下对话历史被污染
        // （断流可能连续发生成百上千次，如果每次都写入对话，会导致
        //   对话中累积大量重复的系统提醒，浪费 token 并干扰模型理解）
        let mut retry_reminder: Option<ChatMessage> = None;
        loop {
            if steps >= self.max_steps {
                tracing::warn!(
                    "Agent 达到最大步数限制 ({})，强制停止",
                    self.max_steps
                );
                break;
            }
            steps += 1;

            // 尝试压缩上下文（如果超过阈值）
            // 从会话首条消息提取临时 CompactionState（解析 <summary> 标签）
            let mut temp_state = extract_compaction_state(self.session.messages());
            match compact::maybe_compact_history(
                &*self.provider,
                &self.model,
                &self.system_role_name,
                &system_message,
                self.reasoning_effort.as_deref(),
                &mut temp_state,
                self.session.messages_mut(),
                &self.config.compaction,
            )
            .await
            {
                Ok(Some(report)) => {
                    tracing::info!(
                        "上下文已压缩: {} → {} tokens（摘要 {} 条，保留 {} 条）",
                        report.tokens_before,
                        report.tokens_after,
                        report.summarized_messages,
                        report.kept_messages,
                    );

                    // 取出被截断后的保留消息
                    let kept = std::mem::take(self.session.messages_mut());

                    // 构建新会话的初始消息
                    let mut initial = Vec::new();
                    if let Some(summary) = temp_state.summary() {
                        // 摘要 + 前一个会话的文件路径引用
                        initial.push(ChatMessage::user(format!(
                            "{}{}{}\n\n上一条完整会话记录: {}",
                            compact::COMPACTION_SUMMARY_PREFIX,
                            summary,
                            compact::COMPACTION_SUMMARY_SUFFIX,
                            self.session.current_file_path(),
                        )));
                    }
                    initial.extend(kept);

                    // 创建新会话（旧会话文件保留完整历史）
                    self.session.rotate(initial)?;

                    // 更新系统提示中的会话引用
                    prompt::update_prompt_session_ref(
                        &mut self.system_prompt,
                        self.session.prev_session_path(),
                    );
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("上下文压缩失败（继续处理）: {}", e);
                }
            }

            // 构建请求消息
            // 摘要已经嵌入在 session.messages() 的首条消息中，无需 CompactionState 注入
            let mut request_messages =
                Vec::with_capacity(1 + self.session.messages().len());
            request_messages.push(system_message.clone());
            request_messages.extend(self.session.messages().iter().cloned());

            // 如果上一轮 LLM 未调用工具，临时追加提醒到请求体
            // .take() 消费并清除提醒，确保每次只追加一条，不会累积
            if let Some(reminder) = retry_reminder.take() {
                request_messages.push(reminder);
            }

            let request = ChatRequest {
                messages: &request_messages,
                tools: Some(&self.tool_specs),
            };

            // 调用 LLM
            tracing::info!(
                "Agent 调用 LLM (step {}/{}, 会话 {} 条消息, 请求 {} 条消息)",
                steps, self.max_steps, self.session.messages().len(), request_messages.len()
            );
            let response = self.provider.chat(request, &self.model, 0.7).await?;

            // 处理响应
            if response.has_tool_calls() {
                // LLM 请求执行工具调用
                let assistant_content = response.text.unwrap_or_default();
                // 如果助手附带了文本，输出它
                if !assistant_content.is_empty() {
                    tracing::info!("LLM 附带文本: {}", assistant_content);
                }
                // 记录助手消息（含工具调用）— 双写到会话
                self.session.push(ChatMessage::assistant_with_tool_calls(
                    assistant_content,
                    response.tool_calls.clone(),
                ))?;

                // 逐个执行工具调用
                for tc in &response.tool_calls {
                    // Finish 工具：显式停止钩子，不实际执行，仅标记结束
                    if tc.name == "Finish" {
                        // 提取 reason 参数用于日志
                        let reason = serde_json::from_str::<serde_json::Value>(&tc.arguments)
                            .ok()
                            .and_then(|v| v.get("reason").and_then(|r| r.as_str()).map(|s| s.to_string()))
                            .unwrap_or_else(|| "(无)".to_string());
                        tracing::info!("Finish({})", reason);
                        // 回传工具结果以保持对话完整性 — 双写到会话
                        self.session
                            .push(ChatMessage::tool_result(&tc.id, "轮次已结束。".to_string()))?;
                        finish_called = true;
                        continue;
                    }

                    tracing::debug!(
                        "工具调用: id={}, name={}, args_len={}",
                        tc.id, tc.name, tc.arguments.len()
                    );
                    let result = self.dispatch_tool(&tc.name, &tc.arguments).await;
                    tracing::debug!(
                        "工具结果 ({}): {}",
                        tc.name,
                        &result.text[..result.text.len().min(200)]
                    );
                    // 追加工具结果到会话（支持多模态图片返回）— 双写
                    if result.images.is_empty() {
                        self.session
                            .push(ChatMessage::tool_result(&tc.id, result.text))?;
                    } else {
                        self.session.push(
                            ChatMessage::tool_result_with_images(
                                &tc.id,
                                result.text,
                                result.images,
                            ),
                        )?;
                    }
                }

                // 如果本批工具调用中包含 Finish，结束循环
                if finish_called {
                    break;
                }
            } else {
                // LLM 结束轮次但没有调用 Finish（纯文本输出，常见于断流/截断）
                // 将 LLM 的文本回复记录到会话（这是真实输出）— 双写
                let text = response.text.unwrap_or_default();
                if !text.is_empty() {
                    self.session.push(ChatMessage::assistant(&text))?;
                }
                tracing::info!(
                    "Agent step {} 未调用 Finish，设置临时提醒 (文本: {} 字符)",
                    steps,
                    text.len()
                );
                // 设置临时提醒，仅在下一次请求构建时追加到请求体
                // **不写入 session**：避免断流场景下对话历史被污染
                retry_reminder = Some(ChatMessage::user(
                    "[系统提醒] 你没有调用任何工具就结束了。你的纯文本输出不会被任何人看到。\
                    请继续使用工具完成任务。当你确认所有工作已完成时，调用 Finish 工具结束本轮。".to_string()
                ));
                // 继续循环，不 break
            }
        }

        Ok(())
    }

    /// 分发工具调用到具体的实现函数
    /// 解析 JSON 参数，路由到对应的工具处理函数
    /// 返回 ToolResult 支持纯文本和多模态（文本 + 图片）两种结果
    async fn dispatch_tool(&mut self, name: &str, arguments: &str) -> ToolResult {
        // 解析参数 JSON
        let args: serde_json::Value = match serde_json::from_str(arguments) {
            Ok(v) => v,
            Err(e) => {
                // 输出原始参数字符串，方便诊断 SSE 累积问题
                tracing::error!(
                    "工具 {} 参数解析失败: {}\n  原始参数: {:?}",
                    name, e, arguments
                );
                return ToolResult::text(format!("参数解析失败: {e}"));
            }
        };

        match name {
            // ReadImage 特殊处理：返回多模态结果（文本 + 图片）
            "ReadImage" => self.dispatch_read_image(&args).await,
            // 其他所有工具：委托给纯文本分发函数
            _ => ToolResult::text(self.dispatch_tool_text(name, &args).await),
        }
    }

    /// 分发纯文本工具调用（内部实现）
    /// 所有只返回文本结果的工具在此路由
    async fn dispatch_tool_text(&mut self, name: &str, args: &serde_json::Value) -> String {
        match name {
            // ----- 文件工具 -----
            "Ls" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("/");
                match self.workspace_tools.list_dir(path) {
                    Ok(listing) => {
                        let line_count = listing.lines().count();
                        tracing::info!("Ls({})\n  └ {} 个条目", path, line_count);
                        listing
                    }
                    Err(e) => {
                        tracing::warn!("Ls({}) 失败: {}", path, e);
                        format!("Ls 失败: {e}")
                    }
                }
            }

            "Read" => {
                let path = match args.get("path").and_then(|v| v.as_str()) {
                    Some(p) => p,
                    None => return "缺少参数: path".to_string(),
                };
                match self
                    .workspace_tools
                    .read_file(&mut self.workspace_session, path)
                {
                    Ok(content) => {
                        // codex 风格: Read(path) → 行数/字节
                        tracing::info!(
                            "Read({})\n  └ {} 行, {} 字节",
                            path, content.lines().count(), content.len()
                        );
                        content
                    }
                    Err(e) => {
                        tracing::warn!("Read({})\n  └ 失败: {}", path, e);
                        format!("Read 失败: {e}")
                    }
                }
            }

            "Write" => {
                let path = match args.get("path").and_then(|v| v.as_str()) {
                    Some(p) => p,
                    None => return "缺少参数: path".to_string(),
                };
                let content = match args.get("content").and_then(|v| v.as_str()) {
                    Some(c) => c,
                    None => return "缺少参数: content".to_string(),
                };
                // codex 风格: Write(path) 换行跟内容预览
                let preview_lines = format_content_preview(content, 8);
                tracing::info!("Write({})\n{}", path, preview_lines);
                match self.workspace_tools.write_file(path, content) {
                    Ok(msg) => msg,
                    Err(e) => {
                        tracing::warn!("Write({}) 失败: {}", path, e);
                        format!("Write 失败: {e}")
                    }
                }
            }

            "Edit" => {
                let path = match args.get("path").and_then(|v| v.as_str()) {
                    Some(p) => p,
                    None => return "缺少参数: path".to_string(),
                };
                let old_text = match args.get("old_text").and_then(|v| v.as_str()) {
                    Some(t) => t,
                    None => return "缺少参数: old_text".to_string(),
                };
                let new_text = match args.get("new_text").and_then(|v| v.as_str()) {
                    Some(t) => t,
                    None => return "缺少参数: new_text".to_string(),
                };
                let replace_all = args
                    .get("replace_all")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // codex 风格: Edit(path) 换行跟 -/+ 差异
                let diff = format_edit_diff(old_text, new_text, 6);
                tracing::info!(
                    "Edit({}{})\n{}",
                    path,
                    if replace_all { ", all" } else { "" },
                    diff
                );
                match self.workspace_tools.edit_file(
                    &mut self.workspace_session,
                    path,
                    old_text,
                    new_text,
                    replace_all,
                ) {
                    Ok(msg) => msg,
                    Err(e) => {
                        tracing::warn!("Edit({}) 失败: {}", path, e);
                        format!("Edit 失败: {e}")
                    }
                }
            }

            // ----- 消息工具 -----
            "SendGroupMessage" => {
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: group_id".to_string(),
                };
                let segments = match parse_message_segments(&args, "message") {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                let preview = format_segments_preview(&segments);
                tracing::info!("SendGroupMessage(群 {})\n  └ {}", group_id, preview);
                let result =
                    crate::tools::message::send_group_message(&self.tool_ctx, group_id, segments)
                        .await;
                if !result.success {
                    tracing::warn!("  └ 失败: {}", result.output);
                }
                result.output
            }

            "SendPrivateMessage" => {
                let user_id = match args.get("user_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: user_id".to_string(),
                };
                let segments = match parse_message_segments(&args, "message") {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                let preview = format_segments_preview(&segments);
                tracing::info!("SendPrivateMessage(用户 {})\n  └ {}", user_id, preview);
                let result = crate::tools::message::send_private_message(
                    &self.tool_ctx,
                    user_id,
                    segments,
                )
                .await;
                if !result.success {
                    tracing::warn!("  └ 失败: {}", result.output);
                }
                result.output
            }

            "DeleteMessage" => {
                let message_id = match args.get("message_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: message_id".to_string(),
                };
                tracing::info!("DeleteMessage({})", message_id);
                let result =
                    crate::tools::message::delete_message(&self.tool_ctx, message_id).await;
                result.output
            }

            "SetMsgEmojiLike" => {
                let message_id = match args.get("message_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: message_id".to_string(),
                };
                let emoji_id = match args.get("emoji_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: emoji_id".to_string(),
                };
                tracing::info!("SetMsgEmojiLike(msg={}, emoji={})", message_id, emoji_id);
                let result = crate::tools::message::set_msg_emoji_like(
                    &self.tool_ctx,
                    message_id,
                    emoji_id,
                )
                .await;
                result.output
            }

            "GroupPoke" => {
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: group_id".to_string(),
                };
                let user_id = match args.get("user_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: user_id".to_string(),
                };
                tracing::info!("GroupPoke(群 {}, 用户 {})", group_id, user_id);
                let result =
                    crate::tools::message::group_poke(&self.tool_ctx, group_id, user_id).await;
                result.output
            }

            "FriendPoke" => {
                let user_id = match args.get("user_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: user_id".to_string(),
                };
                tracing::info!("FriendPoke(用户 {})", user_id);
                let result = crate::tools::message::friend_poke(&self.tool_ctx, user_id).await;
                result.output
            }

            // ----- 查询工具 -----
            "GetFriendList" => {
                tracing::info!("GetFriendList()");
                let result = crate::tools::query::get_friend_list(&self.tool_ctx).await;
                result.output
            }

            "GetGroupList" => {
                tracing::info!("GetGroupList()");
                let result = crate::tools::query::get_group_list(&self.tool_ctx).await;
                result.output
            }

            "GetGroupMemberList" => {
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: group_id".to_string(),
                };
                tracing::info!("GetGroupMemberList(群 {})", group_id);
                let result =
                    crate::tools::query::get_group_member_list(&self.tool_ctx, group_id).await;
                result.output
            }

            "GetGroupMemberInfo" => {
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: group_id".to_string(),
                };
                let user_id = match args.get("user_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: user_id".to_string(),
                };
                tracing::info!("GetGroupMemberInfo(群 {}, 用户 {})", group_id, user_id);
                let result = crate::tools::query::get_group_member_info(
                    &self.tool_ctx,
                    group_id,
                    user_id,
                )
                .await;
                result.output
            }

            // ----- 社交工具 -----
            "HandleFriendRequest" => {
                let flag = match args.get("flag").and_then(|v| v.as_str()) {
                    Some(f) => f,
                    None => return "缺少参数: flag".to_string(),
                };
                let approve = match args.get("approve").and_then(|v| v.as_bool()) {
                    Some(a) => a,
                    None => return "缺少参数: approve".to_string(),
                };
                let remark = args.get("remark").and_then(|v| v.as_str());
                let action = if approve { "同意" } else { "拒绝" };
                tracing::info!("HandleFriendRequest({}, {})", flag, action);
                let result = crate::tools::social::handle_friend_request(
                    &self.tool_ctx,
                    flag,
                    approve,
                    remark,
                )
                .await;
                result.output
            }

            "HandleGroupRequest" => {
                let flag = match args.get("flag").and_then(|v| v.as_str()) {
                    Some(f) => f,
                    None => return "缺少参数: flag".to_string(),
                };
                let sub_type = match args.get("sub_type").and_then(|v| v.as_str()) {
                    Some(s) => s,
                    None => return "缺少参数: sub_type".to_string(),
                };
                let approve = match args.get("approve").and_then(|v| v.as_bool()) {
                    Some(a) => a,
                    None => return "缺少参数: approve".to_string(),
                };
                let reason = args.get("reason").and_then(|v| v.as_str());
                let action = if approve { "同意" } else { "拒绝" };
                tracing::info!("HandleGroupRequest({}, {}, {})", flag, sub_type, action);
                let result = crate::tools::social::handle_group_request(
                    &self.tool_ctx,
                    flag,
                    sub_type,
                    approve,
                    reason,
                )
                .await;
                result.output
            }

            // ----- Skill 工具 -----
            "Skill" => {
                let skill_name = match args.get("name").and_then(|v| v.as_str()) {
                    Some(n) => n,
                    None => return "缺少参数: name".to_string(),
                };
                let skill_path = args.get("path").and_then(|v| v.as_str());
                tracing::info!(
                    "Skill({}{})",
                    skill_name,
                    skill_path.map(|p| format!(", {}", p)).unwrap_or_default()
                );
                let result =
                    crate::tools::skill::execute_skill(&self.tool_ctx, skill_name, skill_path);
                result.output
            }

            // ----- 搜索工具 -----
            "GetMessage" => {
                let message_id = match args.get("message_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: message_id".to_string(),
                };
                tracing::info!("GetMessage({})", message_id);
                let result =
                    crate::tools::search::get_message(&self.tool_ctx, message_id);
                if !result.success {
                    tracing::warn!("GetMessage({}) 失败: {}", message_id, result.output);
                }
                result.output
            }

            "SearchMessages" => {
                let user_id = args.get("user_id").and_then(|v| v.as_i64());
                let group_id = args.get("group_id").and_then(|v| v.as_i64());
                let message_type = args
                    .get("message_type")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let after = args.get("after").and_then(|v| v.as_i64());
                let before = args.get("before").and_then(|v| v.as_i64());
                let limit = args.get("limit").and_then(|v| v.as_u64()).map(|l| l as u32);
                let offset = args.get("offset").and_then(|v| v.as_u64()).map(|o| o as u32);
                tracing::info!(
                    "SearchMessages(user={:?}, group={:?}, type={:?}, after={:?}, before={:?})",
                    user_id, group_id, message_type, after, before
                );
                let result = crate::tools::search::search_messages(
                    &self.tool_ctx,
                    user_id,
                    group_id,
                    message_type,
                    after,
                    before,
                    limit,
                    offset,
                );
                result.output
            }

            "Grep" => {
                let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
                    Some(p) => p,
                    None => return "缺少参数: pattern".to_string(),
                };
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or(".");
                let file_ext = args.get("file_ext").and_then(|v| v.as_str());
                let case_insensitive = args
                    .get("case_insensitive")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                tracing::info!(
                    "Grep(\"{}\", path={}, ext={:?}, ci={})",
                    pattern, path, file_ext, case_insensitive
                );
                match self.workspace_tools.grep(pattern, path, file_ext, case_insensitive) {
                    Ok(output) => {
                        // 统计匹配数用于日志
                        let match_count = output.lines().count().saturating_sub(2);
                        tracing::info!("  └ {} 处匹配", match_count);
                        output
                    }
                    Err(e) => {
                        tracing::warn!("Grep 失败: {}", e);
                        format!("Grep 失败: {e}")
                    }
                }
            }

            _ => format!("未知工具: {name}"),
        }
    }

    /// 执行 ReadImage 工具：通过消息 ID 查找并下载图片，返回多模态结果
    /// 图片 URL 从接收消息时注册的安全注册表中查找，LLM 无法注入任意 URL
    ///
    /// # 安全保障
    /// - URL 来源：仅从已接收消息的 MessageSegment::Image 中提取
    /// - LLM 只能提供 message_id 和 index，无法直接指定 URL
    /// - 图片大小限制 20 MB，防止内存溢出
    /// - 下载超时 30 秒，防止阻塞 Agent 循环
    async fn dispatch_read_image(&self, args: &serde_json::Value) -> ToolResult {
        // 提取 message_id 参数
        let message_id = match args.get("message_id").and_then(|v| v.as_i64()) {
            Some(id) => id,
            None => return ToolResult::text("缺少参数: message_id"),
        };

        // 图片索引，默认 0（第一张图片）
        let index = args
            .get("index")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        // 从安全注册表中查找图片 URL
        let url = match self.tool_ctx.get_image_url(message_id, index) {
            Some(u) => u,
            None => {
                // 区分"消息无图片"和"索引越界"两种情况
                let count = self.tool_ctx.get_image_count(message_id);
                if count == 0 {
                    return ToolResult::text(format!(
                        "消息 {} 中没有已注册的图片。只能查看当前会话期间接收到的消息中的图片。",
                        message_id
                    ));
                } else {
                    return ToolResult::text(format!(
                        "索引超出范围：消息 {} 共有 {} 张图片（索引 0~{}），请求的索引为 {}",
                        message_id,
                        count,
                        count - 1,
                        index
                    ));
                }
            }
        };

        tracing::info!("ReadImage(msg={}, index={}, url={})", message_id, index, url);

        // 构建带超时的 HTTP 客户端（每次调用独立创建，ReadImage 调用频率低）
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
        {
            Ok(c) => c,
            Err(e) => return ToolResult::text(format!("创建 HTTP 客户端失败: {e}")),
        };

        // 从已验证的 URL 下载图片
        let response = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("ReadImage(msg={}) 下载失败: {}", message_id, e);
                return ToolResult::text(format!("图片下载失败: {e}"));
            }
        };

        // 检查 HTTP 响应状态
        if !response.status().is_success() {
            return ToolResult::text(format!(
                "图片下载失败: HTTP {}",
                response.status()
            ));
        }

        // 从 Content-Type 头推断 MIME 类型，回退到 URL 扩展名推断
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|ct| ct.to_str().ok())
            .map(|ct| ct.split(';').next().unwrap_or(ct).trim().to_string())
            .unwrap_or_else(|| guess_image_mime(&url));

        // 读取图片字节
        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("ReadImage(msg={}) 读取响应体失败: {}", message_id, e);
                return ToolResult::text(format!("图片数据读取失败: {e}"));
            }
        };

        // 图片大小限制（20 MB），防止下载超大图片撑爆上下文
        const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
        if bytes.len() > MAX_IMAGE_BYTES {
            return ToolResult::text(format!(
                "图片过大（{:.1} MB，上限 20 MB）",
                bytes.len() as f64 / (1024.0 * 1024.0)
            ));
        }

        // Base64 编码
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);

        let size_kb = bytes.len() as f64 / 1024.0;
        tracing::info!(
            "ReadImage(msg={}, index={})\n  └ {} ({:.1} KB)",
            message_id, index, mime, size_kb
        );

        // 构造多模态结果（文本描述 + 图片数据）
        ToolResult::with_images(
            format!(
                "已加载图片: 消息 {} 第 {} 张 ({}, {:.1} KB)",
                message_id, index, mime, size_kb
            ),
            vec![ImageData {
                mime,
                base64: encoded,
            }],
        )
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 从 URL 路径扩展名推断图片 MIME 类型
/// 用于 Content-Type 响应头缺失时的回退判断
fn guess_image_mime(url: &str) -> String {
    // 取 URL 路径部分（去掉查询参数和 fragment）
    let path = url.split('?').next().unwrap_or(url);
    let path = path.split('#').next().unwrap_or(path);
    // 取最后一个 '.' 之后的扩展名
    if let Some(ext) = path.rsplit('.').next() {
        match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => return "image/jpeg".to_string(),
            "png" => return "image/png".to_string(),
            "gif" => return "image/gif".to_string(),
            "webp" => return "image/webp".to_string(),
            "bmp" => return "image/bmp".to_string(),
            "svg" => return "image/svg+xml".to_string(),
            _ => {}
        }
    }
    // 默认假设 JPEG（QQ 图片最常见的格式）
    "image/jpeg".to_string()
}

/// 格式化文件内容预览（用于 Write 日志）
/// 每行前加 `  │ ` 前缀，最多显示 max_lines 行，超出部分用省略号
fn format_content_preview(content: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let mut out = String::new();
    for (i, line) in lines.iter().take(max_lines).enumerate() {
        // 截断过长的行
        let display: String = line.chars().take(120).collect();
        let ellipsis = if line.chars().count() > 120 { "..." } else { "" };
        if i + 1 == max_lines && total > max_lines {
            out.push_str(&format!("  │ {}{}\n", display, ellipsis));
            out.push_str(&format!("  … +{} 行", total - max_lines));
        } else {
            out.push_str(&format!("  │ {}{}\n", display, ellipsis));
        }
    }
    // 去掉末尾换行
    out.trim_end_matches('\n').to_string()
}

/// 格式化编辑差异（用于 Edit 日志）
/// 删除行前加 `  - `，新增行前加 `  + `，最多各显示 max_lines 行
fn format_edit_diff(old_text: &str, new_text: &str, max_lines: usize) -> String {
    let mut out = String::new();
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    // 删除部分
    for (i, line) in old_lines.iter().take(max_lines).enumerate() {
        let display: String = line.chars().take(120).collect();
        out.push_str(&format!("  - {}\n", display));
        if i + 1 == max_lines && old_lines.len() > max_lines {
            out.push_str(&format!("  … +{} 行删除\n", old_lines.len() - max_lines));
            break;
        }
    }
    // 新增部分
    for (i, line) in new_lines.iter().take(max_lines).enumerate() {
        let display: String = line.chars().take(120).collect();
        out.push_str(&format!("  + {}\n", display));
        if i + 1 == max_lines && new_lines.len() > max_lines {
            out.push_str(&format!("  … +{} 行新增\n", new_lines.len() - max_lines));
            break;
        }
    }
    out.trim_end_matches('\n').to_string()
}

/// 从工具参数 JSON 中解析消息段数组
/// `field` 为包含消息段数组的字段名（通常为 "message"）
fn parse_message_segments(
    args: &serde_json::Value,
    field: &str,
) -> Result<Vec<MessageSegment>, String> {
    let array = match args.get(field) {
        Some(v) => v,
        None => return Err(format!("缺少参数: {field}")),
    };
    serde_json::from_value::<Vec<MessageSegment>>(array.clone())
        .map_err(|e| format!("解析消息段数组失败: {e}"))
}

/// 将消息段数组格式化为人类可读的预览字符串
/// 用于日志输出，展示发送消息的具体内容
fn format_segments_preview(segments: &[MessageSegment]) -> String {
    if segments.is_empty() {
        return "(空消息)".to_string();
    }
    let mut parts = Vec::new();
    for seg in segments {
        match seg {
            // 文本消息段：直接输出文本内容
            MessageSegment::Text { text } => {
                parts.push(text.clone());
            }
            // @消息段
            MessageSegment::At { qq, .. } => {
                parts.push(format!("[@{}]", qq));
            }
            // 回复消息段
            MessageSegment::Reply { id, .. } => {
                let id_str = id.as_deref().unwrap_or("?");
                parts.push(format!("[回复:{}]", id_str));
            }
            // 表情消息段
            MessageSegment::Face { id, .. } => {
                parts.push(format!("[表情:{}]", id));
            }
            // 其他类型：利用 serde 序列化获取类型名
            other => {
                if let Ok(v) = serde_json::to_value(other) {
                    let type_name = v.get("type").and_then(|t| t.as_str()).unwrap_or("unknown");
                    parts.push(format!("[{}]", type_name));
                } else {
                    parts.push("[?]".to_string());
                }
            }
        }
    }
    parts.join("")
}

// ============================================================================
// 会话状态辅助函数
// ============================================================================

/// 从会话消息列表中提取 CompactionState
/// 解析首条消息中的 `<summary>...</summary>` 标签，构建临时 CompactionState
/// 供 `maybe_compact_history` 使用（该函数需要 CompactionState 来决定是否增量更新摘要）
///
/// 如果首条消息不包含摘要标签，返回空的 CompactionState
fn extract_compaction_state(messages: &[ChatMessage]) -> CompactionState {
    let mut state = CompactionState::default();

    // 检查首条消息是否包含摘要
    if let Some(first) = messages.first() {
        if let Some(start) = first.content.find(compact::COMPACTION_SUMMARY_PREFIX) {
            let content_start = start + compact::COMPACTION_SUMMARY_PREFIX.len();
            if let Some(end) = first.content[content_start..].find(compact::COMPACTION_SUMMARY_SUFFIX)
            {
                let summary = first.content[content_start..content_start + end].to_string();
                if !summary.is_empty() {
                    state.set_summary(summary);
                }
            }
        }
    }

    state
}

// ============================================================================
// 工具规范定义
// ============================================================================

/// 构建所有内置工具的 JSON Schema 规范
/// 这些规范通过 OpenAI function calling 协议传递给 LLM
fn build_tool_specs() -> Vec<ToolSpec> {
    vec![
        // ===== 文件工具 =====
        ToolSpec {
            name: "Ls".to_string(),
            description: "列出工作区内指定目录的文件和子目录。路径相对于工作区根目录，\"/\" 或空字符串表示根目录。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "要列出的目录路径（相对于工作区根目录，\"/\" 表示根目录）"
                    }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "Read".to_string(),
            description: "读取工作区内的 UTF-8 文本文件。路径相对于工作区根目录。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "要读取的文件路径（相对于工作区根目录）"
                    }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "Write".to_string(),
            description: "创建新的 UTF-8 文本文件。如果文件已存在则失败，修改已有文件请用 Edit。"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "要创建的文件路径（相对于工作区根目录）"
                    },
                    "content": {
                        "type": "string",
                        "description": "文件内容"
                    }
                },
                "required": ["path", "content"]
            }),
        },
        ToolSpec {
            name: "Edit".to_string(),
            description: "编辑已存在的文件。必须先 Read 同一文件后才能 Edit。使用精确字符串替换。"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "要编辑的文件路径（相对于工作区根目录）"
                    },
                    "old_text": {
                        "type": "string",
                        "description": "要替换的原始文本"
                    },
                    "new_text": {
                        "type": "string",
                        "description": "替换后的新文本"
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "是否替换所有匹配（默认 false，仅替换第一处且要求唯一匹配）"
                    }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        },
        // ===== 消息工具 =====
        ToolSpec {
            name: "SendGroupMessage".to_string(),
            description: "向指定群发送消息。群号必须在发送白名单中。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "目标群号"
                    },
                    "message": {
                        "type": "array",
                        "description": "消息段数组，每个元素为 {\"type\": \"...\", \"data\": {...}}",
                        "items": {
                            "type": "object",
                            "properties": {
                                "type": { "type": "string" },
                                "data": { "type": "object" }
                            },
                            "required": ["type", "data"]
                        }
                    }
                },
                "required": ["group_id", "message"]
            }),
        },
        ToolSpec {
            name: "SendPrivateMessage".to_string(),
            description: "向指定用户发送私聊消息。用户 QQ 号必须在发送白名单中。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "user_id": {
                        "type": "integer",
                        "description": "目标用户 QQ 号"
                    },
                    "message": {
                        "type": "array",
                        "description": "消息段数组",
                        "items": {
                            "type": "object",
                            "properties": {
                                "type": { "type": "string" },
                                "data": { "type": "object" }
                            },
                            "required": ["type", "data"]
                        }
                    }
                },
                "required": ["user_id", "message"]
            }),
        },
        ToolSpec {
            name: "DeleteMessage".to_string(),
            description: "撤回一条消息。只能撤回自己发送的消息。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "message_id": {
                        "type": "integer",
                        "description": "要撤回的消息 ID"
                    }
                },
                "required": ["message_id"]
            }),
        },
        ToolSpec {
            name: "SetMsgEmojiLike".to_string(),
            description: "对一条消息添加表情回应。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "message_id": {
                        "type": "integer",
                        "description": "目标消息 ID"
                    },
                    "emoji_id": {
                        "type": "integer",
                        "description": "表情 ID"
                    }
                },
                "required": ["message_id", "emoji_id"]
            }),
        },
        ToolSpec {
            name: "GroupPoke".to_string(),
            description: "在群聊中戳一戳某人。群号必须在发送白名单中。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    },
                    "user_id": {
                        "type": "integer",
                        "description": "要戳的用户 QQ 号"
                    }
                },
                "required": ["group_id", "user_id"]
            }),
        },
        ToolSpec {
            name: "FriendPoke".to_string(),
            description: "戳一戳好友。用户 QQ 号必须在发送白名单中。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "user_id": {
                        "type": "integer",
                        "description": "要戳的用户 QQ 号"
                    }
                },
                "required": ["user_id"]
            }),
        },
        // ===== 查询工具 =====
        ToolSpec {
            name: "GetFriendList".to_string(),
            description: "获取好友列表。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolSpec {
            name: "GetGroupList".to_string(),
            description: "获取已加入的群列表。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolSpec {
            name: "GetGroupMemberList".to_string(),
            description: "获取指定群的成员列表。群号必须在接收白名单中。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    }
                },
                "required": ["group_id"]
            }),
        },
        ToolSpec {
            name: "GetGroupMemberInfo".to_string(),
            description: "获取指定群成员的详细信息。群号必须在接收白名单中。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "group_id": {
                        "type": "integer",
                        "description": "群号"
                    },
                    "user_id": {
                        "type": "integer",
                        "description": "成员 QQ 号"
                    }
                },
                "required": ["group_id", "user_id"]
            }),
        },
        // ===== 社交工具 =====
        ToolSpec {
            name: "HandleFriendRequest".to_string(),
            description: "处理好友添加请求（同意或拒绝）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "flag": {
                        "type": "string",
                        "description": "请求标识（从好友请求事件中获取）"
                    },
                    "approve": {
                        "type": "boolean",
                        "description": "是否同意"
                    },
                    "remark": {
                        "type": "string",
                        "description": "同意后的备注名称（可选）"
                    }
                },
                "required": ["flag", "approve"]
            }),
        },
        ToolSpec {
            name: "HandleGroupRequest".to_string(),
            description: "处理群添加或群邀请请求（同意或拒绝）。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "flag": {
                        "type": "string",
                        "description": "请求标识（从群请求事件中获取）"
                    },
                    "sub_type": {
                        "type": "string",
                        "description": "请求子类型：\"add\"（加群）或 \"invite\"（邀请）"
                    },
                    "approve": {
                        "type": "boolean",
                        "description": "是否同意"
                    },
                    "reason": {
                        "type": "string",
                        "description": "拒绝理由（可选，仅拒绝时有效）"
                    }
                },
                "required": ["flag", "sub_type", "approve"]
            }),
        },
        // ===== Skill 工具 =====
        ToolSpec {
            name: "Skill".to_string(),
            description: "加载已安装的技能文件。提供技能名称，可选提供技能内相对路径。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "技能名称"
                    },
                    "path": {
                        "type": "string",
                        "description": "技能内相对路径（默认为 SKILL.md）"
                    }
                },
                "required": ["name"]
            }),
        },
        // ===== 多模态工具 =====
        ToolSpec {
            name: "ReadImage".to_string(),
            description: "查看消息中的图片。通过消息 ID 定位图片并返回视觉内容。消息 ID 从消息记录的 MsgID 字段获取。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "message_id": {
                        "type": "integer",
                        "description": "包含图片的消息 ID（从消息记录的 MsgID 字段获取）"
                    },
                    "index": {
                        "type": "integer",
                        "description": "图片在消息中的位置索引（从 0 开始，默认 0 即第一张图片）"
                    }
                },
                "required": ["message_id"]
            }),
        },
        // ===== 搜索工具 =====
        ToolSpec {
            name: "GetMessage".to_string(),
            description: "通过消息 ID 获取原始消息内容（JSON 格式）。利用数据库索引快速定位，返回完整的原始数据。消息 ID 从消息记录的 MsgID 字段获取。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "message_id": {
                        "type": "integer",
                        "description": "要查找的消息 ID（从消息记录的 MsgID 字段获取）"
                    }
                },
                "required": ["message_id"]
            }),
        },
        ToolSpec {
            name: "SearchMessages".to_string(),
            description: "搜索消息记录。支持按发送者 QQ 号、群号、消息类型、时间范围等条件过滤。返回匹配消息的元数据列表。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "user_id": {
                        "type": "integer",
                        "description": "按发送者 QQ 号过滤"
                    },
                    "group_id": {
                        "type": "integer",
                        "description": "按群号过滤"
                    },
                    "message_type": {
                        "type": "string",
                        "description": "按消息类型过滤: \"group\"（群消息）或 \"private\"（私聊消息）"
                    },
                    "after": {
                        "type": "integer",
                        "description": "起始时间戳（Unix 秒，包含），只返回此时间之后的消息"
                    },
                    "before": {
                        "type": "integer",
                        "description": "截止时间戳（Unix 秒，包含），只返回此时间之前的消息"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "最大返回条数（默认 20，上限 100）"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "分页偏移量（默认 0）。翻页时使用。"
                    }
                },
                "required": []
            }),
        },
        ToolSpec {
            name: "Grep".to_string(),
            description: "在工作区文件中搜索文本。返回匹配的文件路径、行号和内容。支持指定搜索目录和文件类型过滤。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "要搜索的文本"
                    },
                    "path": {
                        "type": "string",
                        "description": "搜索起始目录（相对于工作区根，默认搜索整个工作区）"
                    },
                    "file_ext": {
                        "type": "string",
                        "description": "文件扩展名过滤（如 \"md\"、\"jsonl\"），不含前导点号"
                    },
                    "case_insensitive": {
                        "type": "boolean",
                        "description": "是否忽略大小写（默认 false）"
                    }
                },
                "required": ["pattern"]
            }),
        },
        // ===== 流程控制工具 =====
        ToolSpec {
            name: "Finish".to_string(),
            description: "结束本轮处理。当你完成了所有需要做的事情（发送消息、更新记忆等），或明确决定沉默时，必须调用此工具。不调用 Finish 则轮次不会结束。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "reason": {
                        "type": "string",
                        "description": "结束原因的简要说明（如 \"已回复群消息\"、\"决定沉默：话题与我无关\"）"
                    }
                },
                "required": ["reason"]
            }),
        },
    ]
}
