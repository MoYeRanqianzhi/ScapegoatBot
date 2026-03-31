// src/swarm/tools.rs
// 蜂群子代理工具定义与分发模块
// 定义 SpawnAgent / MessageAgent / CheckAgent / WaitAgent 四个蜂群工具的 ToolSpec
// 并实现统一的分发函数，将 LLM 的工具调用路由到 SwarmManager 的对应方法
//
// 这些工具只有在 config.swarm.enabled = true 时才会注册到 LLM 的工具列表中

// ============================================================================
// 导入
// ============================================================================

use crate::config::AppConfig;                    // 全局应用配置
use crate::providers::traits::ToolSpec;          // 工具规范定义
use crate::swarm::SwarmManager;                  // 蜂群管理器
use crate::swarm::types::SpawnConfig;            // 子代理生成配置
use std::sync::Arc;                              // 原子引用计数

// ============================================================================
// 工具规范构建
// ============================================================================

/// 构建蜂群工具的 ToolSpec 列表
/// 仅当 config.swarm.enabled = true 时返回工具定义
/// 否则返回空列表（LLM 看不到蜂群工具，无法创建子代理）
///
/// # 参数
/// - `config`: 全局应用配置（检查 swarm.enabled 和 models 列表）
///
/// # 返回
/// 蜂群工具的 ToolSpec 列表（0 或 4 个工具）
pub fn build_swarm_tool_specs(config: &AppConfig) -> Vec<ToolSpec> {
    // 蜂群未启用时不注册任何工具
    if !config.swarm.enabled {
        return Vec::new();
    }

    // 构建可用模型列表描述（用于 SpawnAgent 的 model 参数说明）
    // 获取主模型名称
    let primary_name = &config.llm.primary_model;
    let model_list_desc = if config.models.is_empty() {
        // 未配置任何模型（理论上不会发生，llm.toml 至少有 primary_model）
        "未配置任何模型。".to_string()
    } else {
        // 列出所有模型，标注主模型
        let mut desc = "可用模型列表：\n".to_string();
        for m in &config.models {
            let primary_mark = if m.name == *primary_name { "（主模型）" } else { "" };
            desc.push_str(&format!(
                "- `{}`{} ({}): {}\n",
                m.name, primary_mark, m.display_name, m.description
            ));
        }
        desc.push_str(&format!("不指定 model 时使用主模型 \"{}\"。", primary_name));
        desc
    };

    vec![
        // ===== SpawnAgent：生成子代理 =====
        ToolSpec {
            name: "SpawnAgent".to_string(),
            description: format!(
                "生成一个子代理来并行执行任务。子代理拥有独立的 LLM 对话和工具访问权限，\
                 完成后通过 Finish 工具返回结果。可指定不同的模型。\n\n{}",
                model_list_desc
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "子代理名称（必须唯一，用于后续的 MessageAgent / CheckAgent / WaitAgent 引用）"
                    },
                    "task": {
                        "type": "string",
                        "description": "分配给子代理的任务描述（尽量详细具体，子代理会作为独立任务执行）"
                    },
                    "model": {
                        "type": "string",
                        "description": "使用的模型名称（引用 [[models]] 配置中的 name 字段，不指定则使用主 LLM 配置）"
                    },
                    "context": {
                        "type": "string",
                        "description": "额外上下文信息（可选，帮助子代理理解任务背景）"
                    }
                },
                "required": ["name", "task"]
            }),
        },
        // ===== MessageAgent：向子代理发送消息 =====
        ToolSpec {
            name: "MessageAgent".to_string(),
            description: "向指定的运行中子代理发送消息。消息会在子代理下一次 LLM 调用前\
                          作为 user 消息注入其对话历史。"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "目标子代理名称"
                    },
                    "message": {
                        "type": "string",
                        "description": "要发送的消息内容"
                    }
                },
                "required": ["name", "message"]
            }),
        },
        // ===== CheckAgent：查询子代理状态 =====
        ToolSpec {
            name: "CheckAgent".to_string(),
            description: "查询子代理的当前状态（运行中 / 已完成 / 已失败）。\
                          可查询单个子代理或列出所有子代理状态。"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "要查询的子代理名称（不指定且 list_all=true 时列出所有子代理）"
                    },
                    "list_all": {
                        "type": "boolean",
                        "description": "是否列出所有子代理状态（默认 false）"
                    }
                },
                "required": []
            }),
        },
        // ===== WaitAgent：等待子代理完成 =====
        ToolSpec {
            name: "WaitAgent".to_string(),
            description: "阻塞等待子代理完成并返回其结果。可等待单个、多个或全部子代理。\
                          注意：等待期间当前代理不会处理其他消息。"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "等待单个子代理的名称"
                    },
                    "names": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "等待多个子代理的名称列表"
                    },
                    "all": {
                        "type": "boolean",
                        "description": "是否等待所有运行中的子代理（默认 false）"
                    }
                },
                "required": []
            }),
        },
    ]
}

// ============================================================================
// 蜂群工具分发
// ============================================================================

/// 分发蜂群工具调用到 SwarmManager 的对应方法
/// 由 AgentRunner 和 SubAgentRunner 在检测到蜂群工具名时调用
///
/// # 参数
/// - `swarm`: 蜂群管理器的 Arc 引用
/// - `tool_name`: 工具名称（"SpawnAgent" / "MessageAgent" / "CheckAgent" / "WaitAgent"）
/// - `args`: 工具参数（已解析的 JSON Value）
/// - `parent_depth`: 调用方的嵌套深度（主代理为 0，每层子代理 +1）
///
/// # 返回
/// 工具执行结果的文本描述
pub async fn dispatch_swarm_tool(
    swarm: &Arc<SwarmManager>,
    tool_name: &str,
    args: &serde_json::Value,
    parent_depth: u32,
) -> String {
    match tool_name {
        // ===== SpawnAgent：生成子代理 =====
        "SpawnAgent" => handle_spawn_agent(swarm, args, parent_depth).await,
        // ===== MessageAgent：向子代理发送消息 =====
        "MessageAgent" => handle_message_agent(swarm, args).await,
        // ===== CheckAgent：查询子代理状态 =====
        "CheckAgent" => handle_check_agent(swarm, args).await,
        // ===== WaitAgent：等待子代理完成 =====
        "WaitAgent" => handle_wait_agent(swarm, args).await,
        // 未知的蜂群工具名（不应到达此处）
        _ => format!("未知的蜂群工具: {tool_name}"),
    }
}

// ============================================================================
// 各工具的具体处理函数
// ============================================================================

/// 处理 SpawnAgent 工具调用
/// 解析参数并调用 SwarmManager::spawn() 创建子代理
async fn handle_spawn_agent(
    swarm: &Arc<SwarmManager>,
    args: &serde_json::Value,
    parent_depth: u32,
) -> String {
    // 提取必需参数：子代理名称
    let name = match args.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return "缺少参数: name".to_string(),
    };
    // 提取必需参数：任务描述
    let task = match args.get("task").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => return "缺少参数: task".to_string(),
    };
    // 提取可选参数：备选模型名称
    let model_name = args
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    // 提取可选参数：额外上下文
    let context = args
        .get("context")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // 构建子代理生成配置
    let spawn_config = SpawnConfig {
        name,
        task,
        model_name,
        context,
    };

    // 记录日志
    tracing::info!(
        "SpawnAgent(name={}, model={:?})",
        spawn_config.name,
        spawn_config.model_name,
    );

    // 调用 SwarmManager 生成子代理
    match SwarmManager::spawn(swarm.clone(), spawn_config, parent_depth).await {
        Ok(msg) => msg,
        Err(e) => {
            tracing::warn!("SpawnAgent 失败: {}", e);
            format!("生成子代理失败: {e}")
        }
    }
}

/// 处理 MessageAgent 工具调用
/// 向指定子代理发送用户消息
async fn handle_message_agent(
    swarm: &Arc<SwarmManager>,
    args: &serde_json::Value,
) -> String {
    // 提取必需参数：目标子代理名称
    let name = match args.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return "缺少参数: name".to_string(),
    };
    // 提取必需参数：消息内容
    let message = match args.get("message").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => return "缺少参数: message".to_string(),
    };

    // 记录日志
    tracing::info!("MessageAgent(name={}, msg_len={})", name, message.len());

    // 发送消息（message_agent 现在是 async 方法）
    match swarm.message_agent(name, message).await {
        Ok(()) => format!("已向子代理 `{name}` 发送消息"),
        Err(e) => {
            tracing::warn!("MessageAgent({name}) 失败: {e}");
            format!("向子代理 `{name}` 发送消息失败: {e}")
        }
    }
}

/// 处理 CheckAgent 工具调用
/// 查询单个子代理状态或列出所有子代理
async fn handle_check_agent(
    swarm: &Arc<SwarmManager>,
    args: &serde_json::Value,
) -> String {
    // 检查是否请求列出所有子代理
    let list_all = args
        .get("list_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if list_all {
        // 列出所有子代理状态
        tracing::info!("CheckAgent(list_all=true)");
        let reports = swarm.list_agents().await;
        if reports.is_empty() {
            return "当前没有任何子代理。".to_string();
        }
        // 格式化为可读的状态列表
        let mut output = format!("共 {} 个子代理：\n\n", reports.len());
        for r in &reports {
            output.push_str(&format_status_report(r));
            output.push('\n');
        }
        output
    } else {
        // 查询单个子代理
        let name = match args.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => return "缺少参数: name（或设置 list_all=true 列出所有子代理）".to_string(),
        };
        tracing::info!("CheckAgent(name={})", name);
        match swarm.check_agent(name).await {
            Ok(report) => format_status_report(&report),
            Err(e) => format!("查询子代理 `{name}` 失败: {e}"),
        }
    }
}

/// 处理 WaitAgent 工具调用
/// 等待单个、多个或全部子代理完成
async fn handle_wait_agent(
    swarm: &Arc<SwarmManager>,
    args: &serde_json::Value,
) -> String {
    // 检查是否等待所有子代理
    let wait_all = args
        .get("all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if wait_all {
        // 等待所有运行中的子代理
        tracing::info!("WaitAgent(all=true)");
        match swarm.wait_all().await {
            Ok(results) => {
                if results.is_empty() {
                    return "没有运行中的子代理需要等待。".to_string();
                }
                // 格式化所有结果
                let mut output = format!("所有 {} 个子代理已完成：\n\n", results.len());
                for (name, result) in &results {
                    output.push_str(&format!("**{}**: {}\n\n", name, result));
                }
                output
            }
            Err(e) => format!("等待所有子代理失败: {e}"),
        }
    } else if let Some(names_arr) = args.get("names").and_then(|v| v.as_array()) {
        // 等待多个指定的子代理
        let names: Vec<String> = names_arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        tracing::info!("WaitAgent(names={:?})", names);
        match swarm.wait_agents(&names).await {
            Ok(results) => {
                let mut output = format!("{} 个子代理已完成：\n\n", results.len());
                for (name, result) in &results {
                    output.push_str(&format!("**{}**: {}\n\n", name, result));
                }
                output
            }
            Err(e) => format!("等待子代理失败: {e}"),
        }
    } else if let Some(name) = args.get("name").and_then(|v| v.as_str()) {
        // 等待单个子代理
        tracing::info!("WaitAgent(name={})", name);
        match swarm.wait_agent(name).await {
            Ok(result) => format!("子代理 `{name}` 已完成：\n\n{result}"),
            Err(e) => format!("等待子代理 `{name}` 失败: {e}"),
        }
    } else {
        // 没有指定任何等待目标
        "缺少参数: 请指定 name、names 或设置 all=true".to_string()
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 将 AgentStatusReport 格式化为人类可读的文本
fn format_status_report(report: &super::types::AgentStatusReport) -> String {
    // 格式化状态文本
    let status_text = match &report.status {
        super::types::AgentStatus::Running => "运行中".to_string(),
        super::types::AgentStatus::Completed { result } => {
            // 截取结果的前 200 字符作为预览
            let preview: String = result.chars().take(200).collect();
            let ellipsis = if result.chars().count() > 200 { "..." } else { "" };
            format!("已完成 — {preview}{ellipsis}")
        }
        super::types::AgentStatus::Failed { error } => {
            format!("已失败 — {error}")
        }
    };
    // 格式化运行时长
    let elapsed_secs = report.elapsed.num_seconds();
    let elapsed_text = if elapsed_secs < 60 {
        format!("{}s", elapsed_secs)
    } else {
        format!("{}m{}s", elapsed_secs / 60, elapsed_secs % 60)
    };

    format!(
        "- `{}` ({}) [{status_text}] (运行 {elapsed_text})",
        report.name, report.model_display
    )
}
