---
name: maintenance-rules
description: 改代码时必须一起改的东西与结构上的约定（迁移只追加、样例配置与内嵌文档的登记、模型面文本的语言、限制写在哪里）；动 schema、存储、工具或文档前先读
metadata:
  type: project
  scope: 整个仓库
  status: active
  last_verified: 2026-11-17
---

代码里看不出来、但每次改动都要守的约定：

- **迁移只追加。** `src/storage/mod.rs` 的 `MIGRATIONS` 末项就是当前 schema 版本；新增存储字段
  追加一项，已经发布过的迁移一个字都不改。
- **样例配置是被测试锁住的。** `skills/sbot-deploy/references/sbot.example.toml` 的每个值必须
  等于代码默认值，README 与 `skills/sbot-deploy/SKILL.md` 里的 toml 代码块必须能通过校验
  （`tests/config_example.rs`）。改配置 schema 或默认值时一起改。
- **`skills/` 下的每个文件都要在 `src/cli/docs.rs` 的 `DOCS` 里登记**，文中每条
  `sbot docs <skill> <reference>` 都要能解析（同文件的测试会抓）。文件一律 LF 换行。
- **改行为时改 skill 源文件**（它就是内嵌文档，见 [[docs-layout]]）、README 的“隐私与已知限制”
  与 CHANGELOG。未实现或刻意不支持的项集中写在
  `skills/sbot-operate/references/limits.md`；别处的文档与注释不承诺未实现的行为。
- **语言**：模型面文本（system prompt、工具描述、工具返回的错误）英文；代码注释、CLI 帮助、
  skill 正文中文；skill frontmatter 的 `description` 英文（别的 agent 靠它决定要不要加载）。
- **新增操作 QQ 的动作**必须走 `src/tools/api/policy.rs` 的检查链并在 hold 期间被拒绝
  （见 [[security-gate-semantics]]）；新增顶层工具要用户同意（见 [[tool-surface]]）。
- **结构**：不要上帝结构。单函数、单结构过大就按职责拆；先例是 `src/actions.rs`、
  `src/config/sections/`、`src/tools/views/`、`src/app/assembly/`、`src/monitor/`。
- **提交前**：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、
  `cargo test --all-targets`；CI（`.github/workflows/ci.yml`）跑同样三项再加别的平台。

**Evidence:** 这些原先写在 `docs/development/README.md` 的“结构约定”“维护约定”里（2026-11-17
随目录删除，git 历史可查）；`MIGRATIONS`、`tests/config_example.rs`、`src/cli/docs.rs` 的测试
于 2026-11-17 对照代码核实。“禁止上帝结构、冗余代码、代码和注释和文档不一致”是用户
2026-11-13 给的长期验收标准。

**Recheck when:** 样例配置或 skill 文件换了位置；测试不再锁住上面某一条。

**Why:** 这几条都是“改了一处、另一处悄悄过期”的地方。迁移改历史会让已有数据库与代码对不上；
样例与文档过期会让部署的人照着错的写。

**How to apply:** 动手前对照本条看这次改动碰到哪几项；能被测试锁住的优先加测试，而不是加
一条要人记住的规则。
