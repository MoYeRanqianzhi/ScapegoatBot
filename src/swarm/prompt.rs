// src/swarm/prompt.rs
// 子代理系统提示构建模块
// 为蜂群子代理生成精简的、任务导向的系统提示词
//
// 与主代理的 prompt.rs 不同，子代理系统提示：
// - 无 QQ 人格/身份设定（子代理不直接与用户交互）
// - 无白名单/消息格式说明
// - 无启动检查/记忆检索指令
// - 聚焦于：任务描述 + 工具使用 + Finish 工具说明

use std::fmt::Write as _;  // 引入 write! 宏所需的 Write trait
use std::path::Path;        // 文件路径类型

// ============================================================================
// 子代理系统提示构建
// ============================================================================

/// 构建子代理的系统提示词
///
/// 生成一个精简的、任务导向的系统提示，包含：
/// 1. 身份：子代理名称 + 父代理信息
/// 2. 核心规则：工具是唯一行动方式
/// 3. 任务描述
/// 4. 工作区路径 + 工具概述
/// 5. Finish 工具使用说明（子代理唯一的输出方式）
/// 6. 当前日期时间
///
/// # 参数
/// - `name`: 子代理名称
/// - `task`: 任务描述（将作为系统提示的核心内容）
/// - `parent_name`: 父代理名称（"main" 表示主代理）
/// - `model_display`: 当前使用的模型显示名称
/// - `workspace_dir`: 工作区根目录路径
/// - `context`: 可选的额外上下文信息
pub fn build_subagent_system_prompt(
    name: &str,
    task: &str,
    parent_name: &str,
    model_display: &str,
    workspace_dir: &Path,
    context: Option<&str>,
) -> String {
    // 获取当前本地时间
    let now = chrono::Local::now();
    // 预分配字符串缓冲区（子代理 prompt 通常比主代理短很多）
    let mut out = String::with_capacity(4096);

    // ===== 第 1 节：身份 =====
    let _ = writeln!(
        out,
        "\
# 子代理运行提示

你是名为 `{name}` 的子代理，由父代理 `{parent_name}` 生成。
你的职责是完成分配给你的特定任务，并通过 `Finish` 工具将结果返回给父代理。
"
    );

    // ===== 第 2 节：核心规则 =====
    out.push_str("\
## 核心规则

**你没有直接输出通道。** 你的纯文本回复不会被任何人看到。
你与外界交互的唯一方式是调用工具。

- 想读文件 → 必须调用 `Read`
- 想写文件 → 必须调用 `Write` 或 `Edit`
- 想搜索 → 必须调用 `Grep`
- **完成任务 → 必须调用 `Finish`，在 result 字段中写明结果**

**`Finish` 是你将结果传递给父代理的唯一方式。**
不调用 `Finish`，你的工作成果不会被父代理看到。
`Finish` 工具的 `result` 字段内容将被原样传递给父代理。

如果你只输出文本而不调用任何工具，系统会提醒你继续工作。
持续调用工具直到任务完成，然后调用 `Finish` 结束。

");

    // ===== 第 3 节：任务描述 =====
    let _ = writeln!(
        out,
        "\
## 你的任务

{task}
"
    );

    // ===== 第 3.5 节：额外上下文（如果提供）=====
    if let Some(ctx) = context {
        // 只在上下文内容非空时注入
        let ctx = ctx.trim();
        if !ctx.is_empty() {
            let _ = writeln!(
                out,
                "\
## 任务上下文

{ctx}
"
            );
        }
    }

    // ===== 第 4 节：工作区与工具概述 =====
    let _ = writeln!(
        out,
        "\
## 工作区

当前工作目录：`{}`

你可以使用以下工具完成任务：
- `Ls`：列出工作区内目录内容
- `Read`：读取工作区内文件（必须先 Read 才能 Edit）
- `Write`：创建新文件
- `Edit`：编辑已存在的文件（必须先 Read）
- `Grep`：在工作区内搜索文本
- `GetMessage`：通过消息 ID 获取原始消息内容
- `SearchMessages`：搜索消息记录
- `SendGroupMessage`：发送群消息
- `SendPrivateMessage`：发送私聊消息
- `SpawnAgent`：生成子代理（你也可以创建自己的子代理来并行处理子任务）
- `MessageAgent`：向子代理发送消息
- `CheckAgent`：查询子代理状态
- `WaitAgent`：等待子代理完成
- `Finish`：**完成任务并返回结果（必须调用）**

### 文件工具注意事项
- 所有路径相对于工作区根目录
- `messages/raw/`、`data/` 和 `sessions/` 目录为只读
- `Edit` 必须先 `Read` 目标文件
",
        workspace_dir.display()
    );

    // ===== 第 5 节：Finish 工具详细说明 =====
    out.push_str("\
## Finish 工具

**`Finish` 工具是你唯一的任务完成方式。**

调用格式：
```json
{\"result\": \"你的任务结果摘要\"}
```

`result` 字段的内容会被原样传递给父代理。请在 result 中：
- 清晰总结任务完成情况
- 列出关键发现或产出（如文件路径、搜索结果等）
- 如果任务失败，说明原因和已尝试的方法

**不要在 result 之外输出任何内容——父代理只能看到 result 字段。**

");

    // ===== 第 6 节：当前日期时间 =====
    let _ = writeln!(
        out,
        "## 当前日期与时间\n\n{} ({})\n",
        now.format("%Y-%m-%d %H:%M:%S"),
        now.format("%Z")
    );

    // ===== 第 7 节：运行时信息 =====
    let _ = writeln!(
        out,
        "## 运行时\n\n子代理：`{name}`\n模型：`{model_display}`\n父代理：`{parent_name}`\n"
    );

    out
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试：子代理 prompt 包含关键段落
    #[test]
    fn subagent_prompt_contains_key_sections() {
        // 构建一个测试用的子代理 prompt
        let prompt = build_subagent_system_prompt(
            "worker-1",                             // 子代理名称
            "搜索工作区中所有包含 TODO 的文件",      // 任务描述
            "main",                                  // 父代理名称
            "DeepSeek V3",                           // 模型显示名
            Path::new("/tmp/workspace"),             // 工作区路径
            None,                                    // 无额外上下文
        );

        // 验证包含身份信息
        assert!(prompt.contains("worker-1"), "应包含子代理名称");
        assert!(prompt.contains("main"), "应包含父代理名称");
        // 验证包含核心规则
        assert!(prompt.contains("核心规则"), "应包含核心规则段");
        assert!(prompt.contains("Finish"), "应包含 Finish 工具说明");
        // 验证包含任务描述
        assert!(prompt.contains("搜索工作区中所有包含 TODO 的文件"), "应包含任务描述");
        // 验证包含工作区路径
        assert!(prompt.contains("/tmp/workspace"), "应包含工作区路径");
        // 验证包含模型信息
        assert!(prompt.contains("DeepSeek V3"), "应包含模型名称");
        // 验证包含日期时间段
        assert!(prompt.contains("当前日期与时间"), "应包含日期时间段");
    }

    /// 测试：不包含 QQ 相关内容（子代理不需要人格设定）
    #[test]
    fn subagent_prompt_no_qq_personality() {
        let prompt = build_subagent_system_prompt(
            "test",
            "测试任务",
            "main",
            "TestModel",
            Path::new("/tmp"),
            None,
        );

        // 子代理 prompt 不应包含主代理特有的 QQ 相关段落
        assert!(!prompt.contains("## 主人"), "不应包含主人段");
        assert!(!prompt.contains("## 白名单"), "不应包含白名单段");
        assert!(!prompt.contains("QQ 消息格式"), "不应包含 QQ 消息格式说明");
        assert!(!prompt.contains("## 安全"), "不应包含完整安全段");
        assert!(!prompt.contains("AGENTS.md"), "不应包含 AGENTS.md");
    }

    /// 测试：有额外上下文时应注入
    #[test]
    fn subagent_prompt_with_context() {
        let prompt = build_subagent_system_prompt(
            "test",
            "测试任务",
            "main",
            "TestModel",
            Path::new("/tmp"),
            Some("这是一些重要的背景信息"),
        );

        assert!(prompt.contains("## 任务上下文"), "应包含任务上下文段");
        assert!(prompt.contains("这是一些重要的背景信息"), "应包含上下文内容");
    }

    /// 测试：空上下文不注入
    #[test]
    fn subagent_prompt_empty_context_not_injected() {
        let prompt = build_subagent_system_prompt(
            "test",
            "测试任务",
            "main",
            "TestModel",
            Path::new("/tmp"),
            Some("   "),
        );

        assert!(!prompt.contains("## 任务上下文"), "空白上下文不应注入");
    }
}
