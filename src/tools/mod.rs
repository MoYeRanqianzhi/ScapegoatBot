// src/tools/mod.rs
// 工具模块
// 将 NapCatLink API 映射为 LLM 可调用的内置工具
// 在工具执行层进行白名单检查
// 通过 cap-std 限制文件工具在 workspace 内

pub mod message;
pub mod query;
pub mod search;
pub mod skill;
pub mod social;
pub mod workspace;

use crate::skills::SkillRegistry;
use crate::storage::images::ImageStore;
use crate::storage::message_index::MessageIndex;
use crate::whitelist::Whitelist;
use napcat_link::OneBotApi;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

// ============================================================================
// 工具执行上下文
// ============================================================================

/// 工具执行上下文
/// 包含所有工具执行所需的共享资源
/// 通过 Arc 在多个异步任务间共享
pub struct ToolContext {
    /// NapCatLink API 聚合器
    pub api: OneBotApi,
    /// 白名单检查器
    pub whitelist: Arc<Whitelist>,
    /// Agent 发送的消息 ID 集合
    /// 用于 master=self 时过滤：收到自己 QQ 发的消息时，
    /// 如果 message_id 在此集合中说明是 agent 发的，不是主人亲自发的
    pub agent_sent_ids: Arc<Mutex<HashSet<i64>>>,
    /// 消息图片注册表（SQLite 持久化）
    /// 接收消息时从 MessageSegment::Image 中提取 URL 并存入数据库
    /// ReadImage 工具通过 message_id + index 安全查找，避免 LLM 注入任意 URL
    pub image_store: Arc<ImageStore>,
    /// 消息索引数据库（SQLite 持久化）
    /// 存储 message_id → (file_path, line_number) 的快速映射
    /// GetMessage / SearchMessages 工具通过此索引定位消息原始内容
    pub message_index: Arc<MessageIndex>,
    /// 消息存储根目录（如 ./workspace/messages）
    /// GetMessage 工具通过此路径拼接 JSONL 文件的完整路径以读取原始内容
    pub messages_dir: PathBuf,
    /// Skill 注册表（通过 cap-std 沙箱保护每个 skill 目录）
    pub skills: Arc<SkillRegistry>,
}

impl ToolContext {
    /// 创建工具执行上下文
    ///
    /// # 参数
    /// - `api`: NapCatLink API 聚合器
    /// - `whitelist`: 消息白名单
    /// - `skills`: Skill 注册表
    /// - `image_store`: 消息图片注册表（SQLite 持久化）
    /// - `message_index`: 消息索引数据库（SQLite 持久化）
    /// - `messages_dir`: 消息存储根目录路径
    pub fn new(
        api: OneBotApi,
        whitelist: Whitelist,
        skills: SkillRegistry,
        image_store: ImageStore,
        message_index: MessageIndex,
        messages_dir: PathBuf,
    ) -> Self {
        Self {
            api,
            whitelist: Arc::new(whitelist),
            agent_sent_ids: Arc::new(Mutex::new(HashSet::new())),
            image_store: Arc::new(image_store),
            message_index: Arc::new(message_index),
            messages_dir,
            skills: Arc::new(skills),
        }
    }

    /// 记录一个由 agent 发送的消息 ID
    pub fn record_agent_sent(&self, message_id: i64) {
        if let Ok(mut ids) = self.agent_sent_ids.lock() {
            ids.insert(message_id);
        }
    }

    /// 检查消息是否由 agent 发送
    pub fn is_agent_sent(&self, message_id: i64) -> bool {
        self.agent_sent_ids
            .lock()
            .map(|ids| ids.contains(&message_id))
            .unwrap_or(false)
    }

    /// 注册消息中的图片 URL（接收消息时调用）
    /// 委托给 SQLite 持久化的 ImageStore
    pub fn register_message_images(&self, message_id: i64, urls: &[String]) {
        self.image_store.register(message_id, urls);
    }

    /// 查找消息中指定索引的图片 URL（ReadImage 工具调用）
    /// 返回 None 表示消息不存在或索引越界
    pub fn get_image_url(&self, message_id: i64, index: usize) -> Option<String> {
        self.image_store.get_url(message_id, index)
    }

    /// 获取消息中的图片数量（用于错误提示）
    pub fn get_image_count(&self, message_id: i64) -> usize {
        self.image_store.get_count(message_id)
    }
}

// ============================================================================
// 工具执行结果
// ============================================================================

/// 工具执行结果
/// 包含成功时的文本输出或失败时的错误消息
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// 是否成功
    pub success: bool,
    /// 输出内容（成功时为结果描述，失败时为错误消息）
    pub output: String,
}

impl ToolResult {
    /// 创建成功结果
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            success: true,
            output: output.into(),
        }
    }

    /// 创建失败结果
    pub fn err(output: impl Into<String>) -> Self {
        Self {
            success: false,
            output: output.into(),
        }
    }
}
