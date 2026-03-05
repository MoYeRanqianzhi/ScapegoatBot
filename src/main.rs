// src/main.rs
// ScapegoatBot 主入口文件
// 初始化日志系统，演示模型调用与 Skill 解析的基本使用流程

mod providers;
mod skills;

use providers::openai::OpenAiProvider;
use providers::traits::{ChatMessage, ChatRequest, Provider};
use skills::{SkillsPromptInjectionMode, load_skills, skills_to_prompt_with_mode};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 初始化 tracing 日志，从环境变量 RUST_LOG 读取日志级别
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("ScapegoatBot 启动中...");

    // ----- 演示 Skill 加载与 Prompt 注入 -----

    // 以当前工作目录作为 workspace（后续可配置）
    let workspace_dir = PathBuf::from(".");

    // 从 workspace/skills/ 目录加载所有 Skill
    let loaded_skills = load_skills(&workspace_dir);
    tracing::info!("已加载 {} 个 Skill", loaded_skills.len());

    // 将 Skill 注入到系统 Prompt 中（Full 模式：完整注入指令和工具定义）
    let skills_prompt = skills_to_prompt_with_mode(
        &loaded_skills,
        &workspace_dir,
        SkillsPromptInjectionMode::Full,
    );

    if !skills_prompt.is_empty() {
        tracing::info!("Skill Prompt 注入内容长度: {} 字符", skills_prompt.len());
    }

    // ----- 演示模型调用 -----

    // 从环境变量读取 API 配置
    let api_key = std::env::var("OPENAI_API_KEY").ok();
    let base_url = std::env::var("OPENAI_BASE_URL").ok();

    // 创建 OpenAI 兼容提供商（支持任何 OpenAI API 兼容服务）
    let provider = OpenAiProvider::new(
        base_url.as_deref(),
        api_key.as_deref(),
        None, // max_tokens_override
    );

    // 构建带有 Skill 注入的系统提示
    let system_prompt = format!(
        "你是 ScapegoatBot，一个像真人一样回复消息的 QQ 机器人。\n\
         你的回复应当自然、口语化，像一个普通的中国年轻人在聊天。\n\n\
         {}",
        skills_prompt
    );

    // 构建消息历史
    let messages = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user("你好啊，最近在干嘛？"),
    ];

    // 构建聊天请求
    let request = ChatRequest {
        messages: &messages,
        tools: None,
    };

    // 读取模型名称，默认使用 gpt-4o-mini
    let model =
        std::env::var("MODEL_NAME").unwrap_or_else(|_| "gpt-4o-mini".to_string());

    tracing::info!("正在调用模型 {} ...", model);

    // 执行模型调用
    match provider.chat(request, &model, 0.7).await {
        Ok(response) => {
            tracing::info!("模型响应: {}", response.text_or_empty());

            // 打印 Token 用量（如果提供商返回了统计数据）
            if let Some(usage) = &response.usage {
                tracing::info!(
                    "Token 用量 - 输入: {:?}, 输出: {:?}",
                    usage.input_tokens,
                    usage.output_tokens
                );
            }
        }
        Err(e) => {
            tracing::error!("模型调用失败: {}", e);
        }
    }

    Ok(())
}
