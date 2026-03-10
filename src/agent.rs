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
use crate::providers::openai::OpenAiProvider;
use crate::providers::traits::{ChatMessage, ChatRequest, Provider, ToolSpec};
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
// Agent 运行器
// ============================================================================

/// Agent 运行器
/// 持有 LLM 提供商、对话历史、工具上下文等全部运行时状态
/// 通过 `run()` 进入主循环，处理消息事件直到通道关闭
pub struct AgentRunner {
    // ----- LLM 相关 -----
    /// OpenAI 兼容 LLM 提供商
    provider: OpenAiProvider,
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

    // ----- 压缩 -----
    /// 上下文压缩状态（累积摘要）
    compaction_state: CompactionState,

    // ----- 对话 -----
    /// 对话历史（不含系统提示，系统提示在请求时注入）
    conversation: Vec<ChatMessage>,
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
        // 从配置创建 LLM 提供商
        let provider = OpenAiProvider::new(
            Some(&config.llm.base_url),
            if config.llm.api_key.is_empty() {
                None
            } else {
                Some(&config.llm.api_key)
            },
            None,
        );

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

        // 获取 Skill 列表用于系统提示
        let skill_items = tool_ctx.skills.list();

        // 构建系统提示
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
            compaction_state: CompactionState::default(),
            conversation: Vec::new(),
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

        // 追加到对话历史
        self.conversation.push(ChatMessage::user(user_content));

        // 刷新系统提示中的日期时间
        prompt::refresh_prompt_datetime(&mut self.system_prompt);

        // 构建系统消息（使用配置的角色名称）
        let system_message = ChatMessage {
            role: self.system_role_name.clone(),
            content: self.system_prompt.clone(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        };

        // Agentic 循环
        let mut steps = 0u32;
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
            match compact::maybe_compact_history(
                &self.provider,
                &self.model,
                &self.system_role_name,
                &system_message,
                self.reasoning_effort.as_deref(),
                &mut self.compaction_state,
                &mut self.conversation,
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
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("上下文压缩失败（继续处理）: {}", e);
                }
            }

            // 构建请求消息
            let request_messages = compact::build_request_messages(
                &system_message,
                &self.compaction_state,
                &self.conversation,
            );

            let request = ChatRequest {
                messages: &request_messages,
                tools: Some(&self.tool_specs),
            };

            // 调用 LLM
            tracing::debug!("Agent 调用 LLM (step {}/{})", steps, self.max_steps);
            let response = self.provider.chat(request, &self.model, 0.7).await?;

            // 处理响应
            if response.has_tool_calls() {
                // LLM 请求执行工具调用
                let assistant_content = response.text.unwrap_or_default();
                // 记录助手消息（含工具调用）
                self.conversation.push(ChatMessage::assistant_with_tool_calls(
                    assistant_content,
                    response.tool_calls.clone(),
                ));

                // 逐个执行工具调用
                for tc in &response.tool_calls {
                    tracing::info!("Agent 执行工具: {} (id={})", tc.name, tc.id);
                    let result = self.dispatch_tool(&tc.name, &tc.arguments).await;
                    tracing::debug!("工具 {} 结果: {:.200}", tc.name, result);
                    // 追加工具结果到对话
                    self.conversation
                        .push(ChatMessage::tool_result(&tc.id, result));
                }
            } else {
                // LLM 结束轮次（无工具调用）
                let text = response.text.unwrap_or_default();
                if !text.is_empty() {
                    self.conversation.push(ChatMessage::assistant(&text));
                    tracing::debug!("Agent 结束轮次，文本: {:.200}", text);
                } else {
                    tracing::debug!("Agent 结束轮次（无输出）");
                }
                break;
            }
        }

        Ok(())
    }

    /// 分发工具调用到具体的实现函数
    /// 解析 JSON 参数，路由到对应的工具处理函数
    async fn dispatch_tool(&mut self, name: &str, arguments: &str) -> String {
        // 解析参数 JSON
        let args: serde_json::Value = match serde_json::from_str(arguments) {
            Ok(v) => v,
            Err(e) => return format!("参数解析失败: {e}"),
        };

        match name {
            // ----- 文件工具 -----
            "Read" => {
                let path = match args.get("path").and_then(|v| v.as_str()) {
                    Some(p) => p,
                    None => return "缺少参数: path".to_string(),
                };
                match self
                    .workspace_tools
                    .read_file(&mut self.workspace_session, path)
                {
                    Ok(content) => content,
                    Err(e) => format!("Read 失败: {e}"),
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
                match self.workspace_tools.write_file(path, content) {
                    Ok(msg) => msg,
                    Err(e) => format!("Write 失败: {e}"),
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
                match self.workspace_tools.edit_file(
                    &mut self.workspace_session,
                    path,
                    old_text,
                    new_text,
                    replace_all,
                ) {
                    Ok(msg) => msg,
                    Err(e) => format!("Edit 失败: {e}"),
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
                let result =
                    crate::tools::message::send_group_message(&self.tool_ctx, group_id, segments)
                        .await;
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
                let result = crate::tools::message::send_private_message(
                    &self.tool_ctx,
                    user_id,
                    segments,
                )
                .await;
                result.output
            }

            "DeleteMessage" => {
                let message_id = match args.get("message_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: message_id".to_string(),
                };
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
                let result =
                    crate::tools::message::group_poke(&self.tool_ctx, group_id, user_id).await;
                result.output
            }

            "FriendPoke" => {
                let user_id = match args.get("user_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: user_id".to_string(),
                };
                let result = crate::tools::message::friend_poke(&self.tool_ctx, user_id).await;
                result.output
            }

            // ----- 查询工具 -----
            "GetFriendList" => {
                let result = crate::tools::query::get_friend_list(&self.tool_ctx).await;
                result.output
            }

            "GetGroupList" => {
                let result = crate::tools::query::get_group_list(&self.tool_ctx).await;
                result.output
            }

            "GetGroupMemberList" => {
                let group_id = match args.get("group_id").and_then(|v| v.as_i64()) {
                    Some(id) => id,
                    None => return "缺少参数: group_id".to_string(),
                };
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
                let result =
                    crate::tools::skill::execute_skill(&self.tool_ctx, skill_name, skill_path);
                result.output
            }

            _ => format!("未知工具: {name}"),
        }
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

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

// ============================================================================
// 工具规范定义
// ============================================================================

/// 构建所有内置工具的 JSON Schema 规范
/// 这些规范通过 OpenAI function calling 协议传递给 LLM
fn build_tool_specs() -> Vec<ToolSpec> {
    vec![
        // ===== 文件工具 =====
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
    ]
}
