# 0.1 归档

这里保留 0.1 时期的资产，**不参与 0.2 的构建或运行**：

- `llm.example.toml`：0.1 的模型与提供方配置（`[[models]]` + 内联 key）。
  0.2 改用 `models.json` + `auth.json`。
- `prompt.md`：0.1 的系统提示词。0.2 使用
  `assets/system_prompt_v4.template.txt`。

现行配置样例在仓库根目录的 `config.example.toml`；使用说明见
[../../guide/](../../guide/README.md)。
