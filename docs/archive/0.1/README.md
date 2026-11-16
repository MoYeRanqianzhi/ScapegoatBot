# 0.1 归档

这里保留 0.1 时期的资产，**不参与 0.2 的构建或运行**：

- `llm.example.toml`：0.1 的模型与提供方配置（`[[models]]` + 内联 key）。
  0.2 改用 `models.json` + `auth.json`。
- `prompt.md`：0.1 的系统提示词。0.2 使用 `assets/system_prompt_v5.txt`。
- `workspace/`：0.1 的工作区模板（`SOUL.md`、`AGENTS.md` 等）。0.2 不读取宿主机上的
  这些文件：人设与规则写在 VFS 里（`/AGENTS.md`、`/USER.md`、`/MEMORY.md`，其余文件由
  agent 列在 `/hooks/context.toml`），见 `sbot docs sbot-use persona-memory`
  （[persona-memory.md](../../../skills/sbot-use/references/persona-memory.md)）。
  格式也不兼容：0.2 的 `/MEMORY.md` 是一行一条的索引，条目在 `/memory/` 下（agent 用文件
  工具维护），不是这里按章节手写的样子；不要直接拷进 VFS。

现行配置样例是 `skills/sbot-deploy/references/sbot.example.toml`；使用说明是 `sbot docs`，
源文件在 [skills/](../../../skills/)。
