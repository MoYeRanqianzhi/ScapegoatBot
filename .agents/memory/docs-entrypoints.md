---
name: docs-entrypoints
description: 用户与开发文档的入口、示例配置的校验方式，以及 0.1 资产归档位置
metadata:
  type: reference
  scope: docs/、README、config.example.toml、tests/config_example.rs
  status: active
  last_verified: 2026-11-04
---

文档结构（alpha.13 起）：

- `README.md`：项目定位、能力、最小配置、CLI、安全摘要、测试与目录。
  只放“最短可用路径”，细节外链。
- `docs/guide/`：用户指南。`README`（索引）→ `install` → `configuration` →
  `usage` → `persona-memory` → `tools-and-vfs` → `security` → `troubleshooting` →
  `limits`（未实现清单）。
- `docs/development/`：协作者文档。模块地图、重构报告、实机测试、提示词维护。
- `docs/archive/0.1/`：0.1 的 `prompt.md`、`llm.example.toml`，不参与构建。
- `.agents/plan/`：设计、评审与未完成计划；`.agents/memory/`：本目录的长期结论。

示例配置是可执行文档：`config.example.toml` 由
`tests/config_example.rs` 用 `SbotConfig::load` + `validate_shape` +
`validate_with_models` 校验，并检查没有字面量凭据。**改 schema 必须同步改样例**，
否则测试会红。

**Evidence:** `tests/config_example.rs`、`docs/development/README.md` 的入口表、
`config.example.toml` 顶部注释。

**Recheck when:** 新增顶层配置节、改 CLI、或调整文档目录。

**Why:** 示例与 README 曾经整体停留在 0.1，用户照抄无法启动；把示例纳入测试是
唯一能防止再次漂移的低成本手段。

**How to apply:** 新功能先写 `docs/guide` 的对应页与 `limits` 的边界，再改
`config.example.toml`，最后跑 `cargo test --test config_example`。
