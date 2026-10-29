# 重构基线（2026-10-30）

> 用途：重构完成后计算重构率——旧代码有多少被复用、多少被改写、多少被废弃、新代码新增了多少。
> 基线提交：`17c9db0`（旧实现最后一次提交，工作区干净）。

## 基线口径

- 统计对象：`src/**/*.rs`（旧实现全部 Rust 源码），共 **41 个文件、19,715 行**。
- 不把 `prompt.md`、`config.example.toml`、`llm.example.toml`、`workspace/` 模板计入代码行数，改造完成后单独对比。
- 设计依据：`.agents/plan/2026-10-28-rebuild-design.md` §14「旧 0.1 归档，不迁移」，新版本线 `0.2.0-alpha.1`，全新模块划分。

## 旧实现逐文件行数

| 文件 | 行数 | 旧职责 | 新归属（Phase） |
|------|-----:|--------|-----------------|
| `src/agent.rs` | 1790 | 单体 Agent 循环、工具调用、历史拼装 | `agent/*`（2） |
| `src/agent_state.rs` | 39 | 运行态枚举 | `agent/event.rs`（2） |
| `src/cli.rs` | 83 | 子命令定义 | `cli/*`（0） |
| `src/compact.rs` | 768 | 上下文压缩与摘要 | `session/compaction.rs`（5） |
| `src/config.rs` | 1148 | 配置结构、校验、加载 | `config/*`（0） |
| `src/control.rs` | 95 | 控制输入 | `ipc/*` + `session`（0/5） |
| `src/control_server.rs` | 172 | TCP 控制服务 | `ipc/*`（0，改管道/UDS） |
| `src/group_role.rs` | 260 | 群角色与权限 | `qq/*`（3） |
| `src/main.rs` | 1252 | 入口、装配、事件循环 | `main.rs` + `app.rs`（0） |
| `src/mcp/manager.rs` | 365 | MCP 客户端管理 | `mcp/*`（6） |
| `src/mcp/mod.rs` | 157 | MCP 类型 | `mcp/*`（6） |
| `src/mcp/transport.rs` | 211 | MCP 传输 | `mcp/*`（6） |
| `src/message_format.rs` | 463 | 消息段与信封格式 | `qq/format.rs`（3） |
| `src/prompt.rs` | 1076 | 系统提示词组装 | `prompt/*`（5） |
| `src/providers/anthropic.rs` | 1534 | Anthropic 协议 | `providers/anthropic.rs`（2） |
| `src/providers/mod.rs` | 64 | provider 入口 | `providers/mod.rs`（2） |
| `src/providers/openai.rs` | 1299 | OpenAI 协议 | `providers/{chat,openai}.rs`（2） |
| `src/providers/traits.rs` | 538 | provider trait 与类型 | `providers/mod.rs`（2） |
| `src/skills/mod.rs` | 519 | 技能加载 | `skills/*`（4） |
| `src/storage/cursor.rs` | 108 | 消息游标 | `qq/resync.rs`（3） |
| `src/storage/images.rs` | 238 | 图片注册表 | `storage/*`（1/3） |
| `src/storage/jsonl.rs` | 30 | JSONL 基础读写 | `session/jsonl.rs`（5） |
| `src/storage/markdown.rs` | 154 | Markdown 视图 | `vfs/dynamic.rs`（1/3） |
| `src/storage/message_index.rs` | 606 | 消息索引 | `storage/messages.rs`（3） |
| `src/storage/mod.rs` | 208 | 存储入口 | `storage/mod.rs`（0） |
| `src/storage/session.rs` | 709 | transcript 会话存储 | `session/*`（5） |
| `src/swarm/dispatch.rs` | 1212 | 子代理分派 | `swarm/*` + `tools/agent.rs`（6） |
| `src/swarm/mod.rs` | 631 | 子代理类型 | `swarm/*`（6） |
| `src/swarm/prompt.rs` | 262 | 子代理提示 | `prompt/*`（5） |
| `src/swarm/runner.rs` | 425 | 子代理运行 | `agent/loop.rs`（2/6） |
| `src/swarm/tools.rs` | 412 | 子代理工具 | `tools/*`（4） |
| `src/swarm/types.rs` | 131 | 子代理类型 | `swarm/defs.rs`（6） |
| `src/tools/fetch.rs` | 467 | Fetch 工具 | `web/http_fetch.rs`（4） |
| `src/tools/message.rs` | 175 | 消息工具 | `tools/api.rs`（4） |
| `src/tools/mod.rs` | 148 | 工具注册 | `tools/registry.rs`（4） |
| `src/tools/query.rs` | 151 | 查询工具 | `tools/fs/*`（4） |
| `src/tools/search.rs` | 167 | 搜索工具 | `web/*`（4） |
| `src/tools/skill.rs` | 37 | 技能工具 | `tools/skill.rs`（4） |
| `src/tools/social.rs` | 113 | 社交工具 | `tools/api.rs`（4） |
| `src/tools/workspace.rs` | 1388 | 工作区文件工具 | `tools/fs/*` + `vfs/*`（1/4） |
| `src/whitelist.rs` | 110 | 消息白名单 | `config/[filter]`（3，语义改为拒收） |
| **合计** | **19,715** | | |

## 重构率计算方式（结束时执行）

1. 以新实现的模块为单位，填一张 `旧文件 → 新文件` 的映射表，每个旧文件标注：
   - `reused`：逻辑基本照搬（≤20% 改动），计入复用；
   - `rewritten`：职责相同但实现重写；
   - `dropped`：职责被删除；
   - `split`/`merged`：拆分或合并到多处，按行数比例计入。
2. 复用率 = `reused` 旧行数 / 19,715。
3. 重写率 = `rewritten` + `split` + `merged` 旧行数 / 19,715。
4. 废弃率 = `dropped` 旧行数 / 19,715。
5. 新增率 = 新实现总行数中不对应任何旧文件的行数 / 新实现总行数。
6. 同时对比非代码资产：`prompt.md`（7,012 B）、`config.example.toml`（4,876 B）、`llm.example.toml`（6,058 B）、`README.md`、`CHANGELOG.md`、`docs/development/*`。

> 说明：设计已定「不保留旧模块划分与数据结构」，因此**结构性重写是预期结果**；本表用于量化“到底改了多少”，而不是以复用率高低评价重构质量。判定以 §12 测试策略与实机运行为准。
