// src/compact.rs
// 上下文压缩模块
// 当对话上下文估算 token 数超过阈值时，将较旧的历史消息压缩为摘要
//
// 设计参考 SA (StudyAdministrator) 的 compact.rs，适配 ScapegoatBot 更简单的消息模型：
// - ScapegoatBot 的 ChatMessage 只有 role + content（无 request_usage / tool_calls）
// - 使用纯启发式 chars/4 规则估算 token（带 1.2x 安全边际）
// - 保留最新的对话后缀，将较旧的前缀压缩为中文摘要
// - 摘要作为合成的 user 消息注入到系统提示之后
// - 支持增量更新：后续压缩会更新已有摘要而不是从头生成

use crate::config::CompactionConfig;
use crate::providers::traits::{ChatMessage, Provider};
use anyhow::Context as _;
use std::fmt::Write as _;

// ============================================================================
// 常量
// ============================================================================

/// 摘要注入前缀 — 包裹在 <summary> 标签中以便模型识别
pub const COMPACTION_SUMMARY_PREFIX: &str =
    "此前的对话历史已被压缩为以下摘要：\n\n<summary>\n";

/// 摘要注入后缀
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

/// 摘要生成的系统提示
const SUMMARIZATION_SYSTEM_PROMPT: &str =
    "你是一个上下文摘要助手。你的任务是为另一个 AI 代理生成结构化的上下文检查点摘要，\
    使其能够在不丢失关键信息的情况下继续工作。\
    必须使用中文输出。保持简洁、精准。";

/// 首次摘要的用户提示模板
const SUMMARIZATION_PROMPT: &str = r#"上方的消息是一段需要摘要的对话。请创建一份结构化的上下文检查点摘要，供另一个 LLM 用来接续工作。

严格使用以下格式：

## 目标
[用户想要完成什么？如果会话涉及多个任务，可以列出多项。]

## 约束与偏好
- [用户提到的任何约束、偏好或要求]
- [如果没有提到，写"（无）"]

## 进度
### 已完成
- [x] [已完成的任务/变更]

### 进行中
- [ ] [当前正在做的工作]

### 阻塞
- [阻碍进展的问题（如有）]

## 关键决策
- **[决策内容]**：[简要理由]

## 下一步
1. [按优先级排列的待办事项]

## 关键上下文
- [继续工作所需的数据、示例或参考资料]
- [如果没有，写"（无）"]

保持每个章节简洁。必须保留完整的文件路径、函数名和错误信息原文。"#;

/// 增量更新已有摘要的用户提示模板
const UPDATE_SUMMARIZATION_PROMPT: &str = r#"上方的消息是需要合入现有摘要的新对话内容。现有摘要在 <previous-summary> 标签中提供。

根据新信息更新现有的结构化摘要。规则：
- 保留现有摘要中的所有信息
- 从新消息中补充新的进度、决策和上下文
- 更新"进度"章节：将已完成的条目从"进行中"移至"已完成"
- 根据实际进展更新"下一步"
- 必须保留完整的文件路径、函数名和错误信息原文
- 如果某些内容已经过时或不再相关，可以移除

严格使用以下格式：

## 目标
[保留已有目标，如果任务范围扩大则补充新目标]

## 约束与偏好
- [保留已有项，补充新发现的约束或偏好]

## 进度
### 已完成
- [x] [包含之前已完成的和新完成的条目]

### 进行中
- [ ] [当前工作 - 根据进展更新]

### 阻塞
- [当前阻塞项 - 已解决的移除]

## 关键决策
- **[决策内容]**：[简要理由]（保留所有已有决策，补充新决策）

## 下一步
1. [根据当前状态更新]

## 关键上下文
- [保留重要上下文，按需补充新内容]

保持每个章节简洁。必须保留完整的文件路径、函数名和错误信息原文。"#;

/// 分割轮前缀摘要的用户提示模板
const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = r#"这是一个因过长而被截断的轮次的前半段。后半段（近期工作）已被保留。

请对前半段生成摘要，为保留的后半段提供上下文：

## 原始请求
[用户在这个轮次中要求做什么？]

## 前期进展
- [前半段中的关键决策和已完成的工作]

## 后半段所需上下文
- [理解保留的后半段内容所需的信息]

保持简洁。只聚焦于理解后半段所必需的信息。"#;

/// JSON 请求外壳的固定 token 开销估算
const REQUEST_WRAPPER_TOKENS: usize = 32;

/// 启发式 token 估算的安全边际系数（6/5 = 1.2x）
/// chars/4 倾向于低估代码、JSON 和工具密集型消息
const SAFETY_MARGIN_NUMERATOR: usize = 6;
const SAFETY_MARGIN_DENOMINATOR: usize = 5;

// ============================================================================
// 压缩状态
// ============================================================================

/// 压缩的持久内存状态，在一个 Agent 任务的生命周期内保持
#[derive(Debug, Clone, Default)]
pub struct CompactionState {
    /// 最新的检查点摘要（如果已压缩过）
    summary: Option<String>,
}

impl CompactionState {
    /// 获取当前的检查点摘要
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    /// 设置新的检查点摘要
    pub fn set_summary(&mut self, summary: String) {
        self.summary = Some(summary);
    }
}

// ============================================================================
// 压缩报告
// ============================================================================

/// 一次成功压缩后的报告
#[derive(Debug, Clone)]
pub struct CompactionReport {
    /// 压缩前的估算总 token 数
    pub tokens_before: usize,
    /// 压缩后的估算总 token 数
    pub tokens_after: usize,
    /// 被摘要化的消息数量
    pub summarized_messages: usize,
    /// 保留原样的消息数量
    pub kept_messages: usize,
    /// 是否在某个用户轮次中间进行了分割
    pub split_turn: bool,
}

// ============================================================================
// 内部数据结构
// ============================================================================

/// 压缩准备结果（在实际调用摘要模型之前计算）
#[derive(Debug, Clone)]
struct PreparedCompaction {
    /// 需要压缩为摘要的消息
    messages_to_summarize: Vec<ChatMessage>,
    /// 保留原样的消息
    kept_messages: Vec<ChatMessage>,
    /// 被分割的轮次的前缀消息（需要单独摘要）
    turn_prefix_messages: Vec<ChatMessage>,
    /// 上次的检查点摘要（如果有）
    previous_summary: Option<String>,
    /// 是否分割了某个轮次
    split_turn: bool,
}

/// 切割点计算结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CutPointResult {
    /// 第一个应该保留原样的消息索引
    first_kept_index: usize,
    /// 包含 first_kept_index 的轮次的起始索引（分割时有值）
    turn_start_index: Option<usize>,
    /// 是否在轮次中间切割
    split_turn: bool,
}

// ============================================================================
// 公开 API
// ============================================================================

/// 构建发送给 LLM 的完整请求消息列表
///
/// 系统提示始终在最前面。如果有压缩摘要，紧随其后注入为合成 user 消息。
/// 然后追加保留的真实对话消息。
pub fn build_request_messages(
    system_message: &ChatMessage,
    state: &CompactionState,
    conversation_messages: &[ChatMessage],
) -> Vec<ChatMessage> {
    let mut messages = Vec::with_capacity(
        1 + conversation_messages.len() + usize::from(state.summary().is_some()),
    );
    // 系统提示始终在首位
    messages.push(system_message.clone());
    // 如果存在压缩摘要，注入为合成 user 消息
    if let Some(summary) = state.summary() {
        messages.push(ChatMessage::user(format!(
            "{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}"
        )));
    }
    // 追加真实对话消息
    messages.extend(conversation_messages.iter().cloned());
    messages
}

/// 估算完整请求的近似 token 数
/// 包括系统提示 + 摘要 + 对话消息
pub fn estimate_request_tokens(
    system_message: &ChatMessage,
    state: &CompactionState,
    conversation_messages: &[ChatMessage],
) -> usize {
    let request_messages = build_request_messages(system_message, state, conversation_messages);
    apply_safety_margin(
        REQUEST_WRAPPER_TOKENS + estimate_context_tokens(&request_messages),
    )
}

/// 估算一组消息的近似 token 总数
pub fn estimate_context_tokens(messages: &[ChatMessage]) -> usize {
    messages.iter().map(estimate_tokens).sum()
}

/// 估算单条消息的近似 token 数
/// 使用 chars/4 启发式规则（通用、轻量、提供商无关）
/// 包含 tool_calls 和 tool_call_id 的额外开销
pub fn estimate_tokens(message: &ChatMessage) -> usize {
    let mut chars = message.role.len() + message.content.len();
    // 计入工具调用的额外内容
    for tc in &message.tool_calls {
        chars += tc.id.len() + tc.name.len() + tc.arguments.len();
    }
    if let Some(id) = &message.tool_call_id {
        chars += id.len();
    }
    chars.div_ceil(4)
}

/// 尝试压缩对话历史（如果 token 数超过阈值）
///
/// `conversation_messages` 仅包含系统提示之后的真实对话消息。
/// 此函数不会修改系统提示，也不会在存储的对话中注入合成摘要。
/// 合成摘要只在调用方通过 `build_request_messages` 构建实际请求时才会出现。
pub async fn maybe_compact_history(
    provider: &dyn Provider,
    model: &str,
    system_role_name: &str,
    system_message: &ChatMessage,
    reasoning_effort: Option<&str>,
    state: &mut CompactionState,
    conversation_messages: &mut Vec<ChatMessage>,
    cfg: &CompactionConfig,
) -> anyhow::Result<Option<CompactionReport>> {
    // 总开关检查
    if !cfg.enabled {
        return Ok(None);
    }

    // 估算当前 token 数，未达阈值则跳过
    let initial_tokens_before = estimate_request_tokens(
        system_message, state, conversation_messages,
    );
    if initial_tokens_before < cfg.trigger_tokens {
        return Ok(None);
    }

    let mut total_summarized_messages = 0usize;
    let mut split_turn = false;

    // 压缩循环：可能需要多轮压缩才能降到阈值以下
    loop {
        let total_tokens_before = estimate_request_tokens(
            system_message, state, conversation_messages,
        );
        // 已降到阈值以下，返回报告
        if total_tokens_before < cfg.trigger_tokens {
            return Ok(Some(CompactionReport {
                tokens_before: initial_tokens_before,
                tokens_after: total_tokens_before,
                summarized_messages: total_summarized_messages,
                kept_messages: conversation_messages.len(),
                split_turn,
            }));
        }

        // 准备压缩：确定哪些消息需要摘要、哪些保留
        let prepared = prepare_compaction(conversation_messages, state, cfg)
            .ok_or_else(|| anyhow::anyhow!(
                "估算请求大小为 {total_tokens_before} tokens，但已无更多可压缩的历史"
            ))?;

        // 生成主历史摘要
        let history_summary = if prepared.messages_to_summarize.is_empty() {
            prepared.previous_summary.clone()
                .unwrap_or_else(|| "No prior history.".to_string())
        } else {
            generate_summary(
                provider, model, system_role_name, reasoning_effort,
                &prepared.messages_to_summarize,
                cfg.reserve_summary_tokens,
                prepared.previous_summary.as_deref(),
            ).await?
        };

        // 如果分割了某个轮次，还需要为前缀生成单独摘要
        let final_summary = if prepared.split_turn && !prepared.turn_prefix_messages.is_empty() {
            let turn_prefix_summary = generate_turn_prefix_summary(
                provider, model, system_role_name, reasoning_effort,
                &prepared.turn_prefix_messages,
                cfg.reserve_summary_tokens,
            ).await?;
            format!(
                "{history_summary}\n\n---\n\n**Turn Context (split turn):**\n\n{turn_prefix_summary}"
            )
        } else {
            history_summary
        };

        // 验证压缩确实减少了估算 token 数（防止无限循环）
        let tokens_before_heuristic = apply_safety_margin(
            REQUEST_WRAPPER_TOKENS
                + estimate_context_tokens(
                    &build_request_messages(system_message, state, conversation_messages),
                ),
        );
        let mut next_state = state.clone();
        next_state.set_summary(final_summary.clone());
        let tokens_after_heuristic = apply_safety_margin(
            REQUEST_WRAPPER_TOKENS
                + estimate_context_tokens(
                    &build_request_messages(system_message, &next_state, &prepared.kept_messages),
                ),
        );
        if tokens_after_heuristic >= tokens_before_heuristic {
            anyhow::bail!(
                "压缩未能减少估算 token 数: {} -> {} tokens",
                tokens_before_heuristic, tokens_after_heuristic
            );
        }

        // 应用压缩结果
        total_summarized_messages +=
            prepared.messages_to_summarize.len() + prepared.turn_prefix_messages.len();
        split_turn |= prepared.split_turn;
        *conversation_messages = prepared.kept_messages;
        state.set_summary(final_summary);
    }
}

// ============================================================================
// 内部函数
// ============================================================================

/// 应用安全边际系数（chars/4 倾向于低估，乘以 6/5 = 1.2x）
fn apply_safety_margin(tokens: usize) -> usize {
    tokens
        .saturating_mul(SAFETY_MARGIN_NUMERATOR)
        .div_ceil(SAFETY_MARGIN_DENOMINATOR)
}

/// 准备压缩：确定切割点，分配消息到摘要组和保留组
fn prepare_compaction(
    conversation_messages: &[ChatMessage],
    state: &CompactionState,
    cfg: &CompactionConfig,
) -> Option<PreparedCompaction> {
    // 对话太短，不压缩
    if conversation_messages.len() < cfg.min_messages_to_compact {
        return None;
    }

    // 计算切割点
    let cut = find_cut_point(conversation_messages, cfg.keep_recent_tokens);
    if cut.first_kept_index == 0 {
        return None;
    }

    // 根据是否分割轮次，确定摘要范围
    let history_end = if cut.split_turn {
        cut.turn_start_index?
    } else {
        cut.first_kept_index
    };

    let messages_to_summarize = conversation_messages[..history_end].to_vec();
    let turn_prefix_messages = if cut.split_turn {
        conversation_messages[history_end..cut.first_kept_index].to_vec()
    } else {
        Vec::new()
    };
    let kept_messages = conversation_messages[cut.first_kept_index..].to_vec();

    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }

    Some(PreparedCompaction {
        messages_to_summarize,
        kept_messages,
        turn_prefix_messages,
        previous_summary: state.summary().map(str::to_string),
        split_turn: cut.split_turn,
    })
}

/// 将对话序列化为纯文本（供摘要模型作为数据处理，而非继续对话）
/// 处理工具调用消息：将工具名称列表和工具结果一并序列化
fn serialize_conversation(messages: &[ChatMessage]) -> String {
    let mut parts = Vec::new();
    for msg in messages {
        let label = match msg.role.as_str() {
            "user" => "[User]",
            "assistant" => "[Assistant]",
            "tool" => "[Tool result]",
            "system" | "developer" => "[Context]",
            _ => &msg.role,
        };
        // 工具调用请求
        if !msg.tool_calls.is_empty() {
            let tool_names: Vec<&str> = msg.tool_calls.iter().map(|tc| tc.name.as_str()).collect();
            parts.push(format!("{label}: [Called tools: {}]", tool_names.join(", ")));
        }
        // 文本内容
        let content = msg.content.trim();
        if !content.is_empty() {
            parts.push(format!("{label}: {content}"));
        }
    }
    parts.join("\n\n")
}

/// 找到所有合法的切割点索引
/// 不在 tool 消息上切割（tool 结果必须紧跟请求它的 assistant 消息）
fn find_valid_cut_points(messages: &[ChatMessage]) -> Vec<usize> {
    messages.iter().enumerate()
        .filter_map(|(i, msg)| {
            if msg.role == "tool" { None } else { Some(i) }
        })
        .collect()
}

/// 在消息列表中找到包含指定索引的 user 轮次的起始位置
fn find_turn_start_index(messages: &[ChatMessage], entry_index: usize) -> Option<usize> {
    (0..=entry_index).rev()
        .find(|&i| messages[i].role == "user")
}

/// 计算切割点：从末尾开始累积 token，直到达到 keep_recent_tokens 预算
fn find_cut_point(messages: &[ChatMessage], keep_recent_tokens: usize) -> CutPointResult {
    let cut_points = find_valid_cut_points(messages);
    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_index: 0,
            turn_start_index: None,
            split_turn: false,
        };
    }

    // 从末尾开始向前累积 token
    let mut accumulated_tokens = 0usize;
    let mut cut_index = 0usize;
    let mut hit_budget = false;

    for index in (0..messages.len()).rev() {
        accumulated_tokens += apply_safety_margin(estimate_tokens(&messages[index]));
        if accumulated_tokens >= keep_recent_tokens {
            // 找到最近的合法切割点
            cut_index = cut_points.iter().copied()
                .rfind(|&c| c <= index)
                .unwrap_or(0);
            hit_budget = true;
            break;
        }
    }

    // 所有消息的 token 总和都在预算内，不需要切割
    if !hit_budget {
        return CutPointResult {
            first_kept_index: 0,
            turn_start_index: None,
            split_turn: false,
        };
    }

    // 判断是否在轮次中间切割
    let split_turn = messages[cut_index].role != "user";
    let turn_start_index = if split_turn {
        find_turn_start_index(messages, cut_index)
    } else {
        None
    };

    CutPointResult {
        first_kept_index: cut_index,
        turn_start_index,
        split_turn: split_turn && turn_start_index.is_some(),
    }
}

/// 构建摘要生成的提示文本
fn build_summarization_prompt(
    conversation_text: &str,
    previous_summary: Option<&str>,
) -> String {
    let mut prompt = String::new();
    let _ = writeln!(prompt, "<conversation>");
    let _ = writeln!(prompt, "{conversation_text}");
    let _ = writeln!(prompt, "</conversation>");
    let _ = writeln!(prompt);

    // 如果有已有摘要，使用增量更新模板
    if let Some(prev) = previous_summary {
        let prev = prev.trim();
        if !prev.is_empty() {
            let _ = writeln!(prompt, "<previous-summary>");
            let _ = writeln!(prompt, "{prev}");
            let _ = writeln!(prompt, "</previous-summary>");
            let _ = writeln!(prompt);
        }
    }

    prompt.push_str(if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT
    } else {
        SUMMARIZATION_PROMPT
    });

    prompt
}

/// 构建分割轮前缀摘要的提示文本
fn build_turn_prefix_prompt(conversation_text: &str) -> String {
    format!(
        "<conversation>\n{conversation_text}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
    )
}

/// 调用 LLM 生成历史摘要
async fn generate_summary(
    provider: &dyn Provider,
    model: &str,
    system_role_name: &str,
    reasoning_effort: Option<&str>,
    messages_to_summarize: &[ChatMessage],
    reserve_summary_tokens: usize,
    previous_summary: Option<&str>,
) -> anyhow::Result<String> {
    let conversation_text = serialize_conversation(messages_to_summarize);
    let prompt_text = build_summarization_prompt(&conversation_text, previous_summary);
    let max_tokens = ((reserve_summary_tokens as f64) * 0.8).floor() as u32;

    // 使用 provider 的 chat_with_system 发送摘要请求
    // temperature = 0.0：摘要生成需要确定性输出
    let _ = (system_role_name, reasoning_effort, max_tokens);
    let response = provider.chat_with_system(
        Some(SUMMARIZATION_SYSTEM_PROMPT),
        &prompt_text,
        model,
        0.0,
    ).await
        .context("摘要生成请求失败")?;

    let summary = response.trim().to_string();
    if summary.is_empty() {
        anyhow::bail!("摘要生成返回了空内容");
    }

    Ok(summary)
}

/// 调用 LLM 生成分割轮前缀的摘要
async fn generate_turn_prefix_summary(
    provider: &dyn Provider,
    model: &str,
    system_role_name: &str,
    reasoning_effort: Option<&str>,
    turn_prefix_messages: &[ChatMessage],
    reserve_summary_tokens: usize,
) -> anyhow::Result<String> {
    let conversation_text = serialize_conversation(turn_prefix_messages);
    let prompt_text = build_turn_prefix_prompt(&conversation_text);
    let max_tokens = ((reserve_summary_tokens as f64) * 0.5).floor() as u32;

    // temperature = 0.0：摘要生成需要确定性输出
    let _ = (system_role_name, reasoning_effort, max_tokens);
    let response = provider.chat_with_system(
        Some(SUMMARIZATION_SYSTEM_PROMPT),
        &prompt_text,
        model,
        0.0,
    ).await
        .context("分割轮前缀摘要生成请求失败")?;

    let summary = response.trim().to_string();
    if summary.is_empty() {
        anyhow::bail!("分割轮前缀摘要返回了空内容");
    }

    Ok(summary)
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_basic() {
        // "user" (4 chars) + "hello world" (11 chars) = 15 chars => ceil(15/4) = 4
        let msg = ChatMessage::user("hello world");
        assert_eq!(estimate_tokens(&msg), 4);
    }

    #[test]
    fn estimate_tokens_empty() {
        let msg = ChatMessage::user("");
        // "user" (4 chars) + "" (0) = 4 => ceil(4/4) = 1
        assert_eq!(estimate_tokens(&msg), 1);
    }

    #[test]
    fn safety_margin_applied() {
        // 100 * 6 / 5 = 120
        assert_eq!(apply_safety_margin(100), 120);
        // 0 stays 0
        assert_eq!(apply_safety_margin(0), 0);
    }

    #[test]
    fn build_request_messages_without_summary() {
        let sys = ChatMessage::system("system prompt");
        let state = CompactionState::default();
        let conv = vec![ChatMessage::user("hi"), ChatMessage::assistant("hello")];

        let result = build_request_messages(&sys, &state, &conv);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].role, "system");
        assert_eq!(result[1].role, "user");
        assert_eq!(result[2].role, "assistant");
    }

    #[test]
    fn build_request_messages_with_summary() {
        let sys = ChatMessage::system("system prompt");
        let mut state = CompactionState::default();
        state.set_summary("历史摘要内容".to_string());
        let conv = vec![ChatMessage::user("最新消息")];

        let result = build_request_messages(&sys, &state, &conv);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].role, "system");
        // 第二条是注入的摘要
        assert_eq!(result[1].role, "user");
        assert!(result[1].content.contains("历史摘要内容"));
        assert!(result[1].content.contains(COMPACTION_SUMMARY_PREFIX.trim()));
        // 第三条是真实对话
        assert_eq!(result[2].content, "最新消息");
    }

    #[test]
    fn serialize_conversation_labels_roles() {
        let messages = vec![
            ChatMessage::user("请帮我写代码"),
            ChatMessage::assistant("好的，我来写。"),
        ];
        let text = serialize_conversation(&messages);
        assert!(text.contains("[User]: 请帮我写代码"));
        assert!(text.contains("[Assistant]: 好的，我来写。"));
    }

    #[test]
    fn serialize_conversation_skips_empty() {
        let messages = vec![
            ChatMessage::user("有内容"),
            ChatMessage::assistant("   "),
        ];
        let text = serialize_conversation(&messages);
        assert!(text.contains("[User]: 有内容"));
        // 空白内容的 assistant 消息应被跳过
        assert!(!text.contains("[Assistant]"));
    }

    #[test]
    fn find_cut_point_all_within_budget() {
        // 非常小的消息，全部在 budget 内
        let messages = vec![
            ChatMessage::user("hi"),
            ChatMessage::assistant("hello"),
        ];
        let result = find_cut_point(&messages, 100_000);
        assert_eq!(result.first_kept_index, 0);
        assert!(!result.split_turn);
    }

    #[test]
    fn find_cut_point_exceeds_budget() {
        // 创建多条大消息使其超出预算
        let mut messages = Vec::new();
        for i in 0..20 {
            messages.push(ChatMessage::user(format!("消息 {i} {}", "x".repeat(200))));
            messages.push(ChatMessage::assistant(format!("回复 {i} {}", "y".repeat(200))));
        }
        // keep_recent_tokens = 100 → 只保留最后几条
        let result = find_cut_point(&messages, 100);
        assert!(result.first_kept_index > 0);
    }

    #[test]
    fn build_summarization_prompt_first_time() {
        let prompt = build_summarization_prompt("[User]: hi", None);
        assert!(prompt.contains("<conversation>"));
        assert!(prompt.contains(SUMMARIZATION_PROMPT));
        assert!(!prompt.contains("<previous-summary>"));
    }

    #[test]
    fn build_summarization_prompt_incremental() {
        let prompt = build_summarization_prompt("[User]: hi", Some("旧摘要"));
        assert!(prompt.contains("<conversation>"));
        assert!(prompt.contains("<previous-summary>"));
        assert!(prompt.contains("旧摘要"));
        assert!(prompt.contains(UPDATE_SUMMARIZATION_PROMPT));
    }

    #[test]
    fn prepare_compaction_too_few_messages() {
        let messages = vec![
            ChatMessage::user("hi"),
            ChatMessage::assistant("hello"),
        ];
        let state = CompactionState::default();
        let cfg = CompactionConfig {
            min_messages_to_compact: 8,
            ..Default::default()
        };
        assert!(prepare_compaction(&messages, &state, &cfg).is_none());
    }
}
