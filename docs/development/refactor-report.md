# 重构率报告（0.1 → 0.2）

> 基线：`.agents/plan/2026-10-30-refactor-baseline.md`，旧实现提交 `17c9db0`。
> 对象：`src/**/*.rs`。新实现统计于 2026-10-30，版本 `0.2.0-alpha.8`。

## 结论

| 指标 | 数值 | 说明 |
|------|-----:|------|
| 旧代码总量 | 19,715 行 / 41 文件 | 基线口径 |
| 复用（reused） | **0 行（0.0%）** | 旧模块划分、数据结构与调用方式全部放弃 |
| 重写（rewritten/split/merged） | **19,217 行（97.5%）** | 职责相同、实现重写 |
| 废弃（dropped） | **498 行（2.5%）** | 对应职责在 0.2 中尚未提供 |
| 新实现总量 | **30,647 行 / 62 文件** | 含内联单元测试 |
| 无旧对应职责的新增模块 | 约 **10,052 行（32.8%）** | VFS、IPC、安全闸、monitor、备份/诊断等 |

**结构性重写率 = 100%**（复用 0 行）。这与设计 §14「旧 0.1 归档，不迁移」一致；本报告用于量化改动，而不是以复用率评价质量。

## 旧文件 → 新归属

判定口径：`reused` ≤20% 改动；`rewritten` 职责相同但实现重写；`split`/`merged` 拆分或合并；`dropped` 0.2 未提供该职责。

| 旧文件（行数） | 判定 | 新归属 |
|----------------|------|--------|
| `agent.rs`（1790） | rewritten | `agent/loop.rs`、`agent/runner.rs`（pi 风格内/外循环、steering、输出闸） |
| `agent_state.rs`（39） | merged | `agent/event.rs` |
| `cli.rs`（83） | rewritten | `cli/mod.rs`、`cli/backup.rs`、`cli/doctor.rs` |
| `compact.rs`（768） | rewritten | `session/compaction.rs`（CJK 估算、回合边界切割、结构化摘要） |
| `config.rs`（1148） | rewritten | `config/*`（严格 schema、deny_unknown_fields、字段路径校验） |
| `control.rs`（95） | rewritten | `ipc/*`（长度前缀 JSON、幂等表） |
| `control_server.rs`（172） | rewritten | `ipc/{windows,unix}.rs`（命名管道/UDS） |
| `group_role.rs`（260） | dropped | 群角色同步未在 0.2 实现 |
| `main.rs`（1252） | rewritten | `main.rs` + `app.rs`（薄入口 + 运行时装配） |
| `mcp/manager.rs`（365） | rewritten | `mcp/*`（stdio JSON-RPC、工具发现） |
| `mcp/mod.rs`（157） | rewritten | 同上 |
| `mcp/transport.rs`（211） | rewritten | 同上（stdio 传输） |
| `message_format.rs`（463） | rewritten | `qq/format.rs`、`qq/event.rs` |
| `prompt.rs`（1076） | rewritten | `prompt/*` + `assets/system_prompt_v4.template.txt`（静态/动态边界、hooks） |
| `providers/anthropic.rs`（1534） | rewritten | `providers/anthropic.rs`（新请求体、自适应 thinking、SSE） |
| `providers/mod.rs`（64） | rewritten | `providers/mod.rs` |
| `providers/openai.rs`（1299） | rewritten | `providers/{chat,openai}.rs` |
| `providers/traits.rs`（538） | rewritten | `providers/mod.rs`（新事件模型、fallback） |
| `skills/mod.rs`（519） | rewritten | `tools/skill.rs` + VFS `/skills` |
| `storage/cursor.rs`（108） | rewritten | `qq/resync.rs` |
| `storage/images.rs`（238） | dropped | 由 VFS `blobs/` 承担 |
| `storage/jsonl.rs`（30） | rewritten | `storage/messages.rs`、`session/mod.rs` |
| `storage/markdown.rs`（154） | rewritten | `tools/views.rs`、`vfs/dynamic.rs` |
| `storage/message_index.rs`（606） | rewritten | `storage/messages.rs` |
| `storage/mod.rs`（208） | rewritten | `storage/mod.rs`（迁移 v1–v6、canary） |
| `storage/session.rs`（709） | rewritten | `session/*`（分段链、turn buffer、压缩检查点） |
| `swarm/dispatch.rs`（1212） | rewritten | `swarm/*` + `tools/agent.rs`（单层子代理） |
| `swarm/mod.rs`（631） | rewritten | 同上 |
| `swarm/prompt.rs`（262） | rewritten | `swarm::SUBAGENT_COMMON` |
| `swarm/runner.rs`（425） | rewritten | `Runner::run_subagent` |
| `swarm/tools.rs`（412） | rewritten | `ToolRegistry::filtered` + 类型工具集 |
| `swarm/types.rs`（131） | rewritten | `swarm::AgentType` / `AgentDefinition` |
| `tools/fetch.rs`（467） | rewritten | `tools/web.rs`、`tools/net.rs`（SSRF 策略） |
| `tools/message.rs`（175） | merged | `tools/api.rs` |
| `tools/mod.rs`（148） | rewritten | `tools/mod.rs`（Tool trait、权限声明、执行模式） |
| `tools/query.rs`（151） | merged | `tools/fs.rs` |
| `tools/search.rs`（167） | rewritten | `tools/web.rs` |
| `tools/skill.rs`（37） | rewritten | `tools/skill.rs` |
| `tools/social.rs`（113） | merged | `tools/api.rs` |
| `tools/workspace.rs`（1388） | rewritten | `tools/fs.rs` + `vfs/*` |
| `whitelist.rs`（110） | rewritten | `qq/filter.rs` + `[filter]`（语义改为拒收） |

## 计算

```
复用率   = 0 / 19,715                       = 0.0%
重写率   = 19,217 / 19,715                  = 97.5%
废弃率   = 498 / 19,715                     = 2.5%
新增率   = 10,052 / 30,647                  = 32.8%
结构重写 = (复用 0) → 全部旧职责由新代码承担  = 100%
```

废弃部分只剩群角色同步（260 行）与旧图片注册表（238 行）：群角色由 NapCat 的实时查询替代，图片由 VFS `blobs/` 承担。

## 新增职责（无旧文件对应）

| 模块 | 行数 | 职责 |
|------|-----:|------|
| `vfs/*` | 2,746 | 文本加密、1 MiB blob 分块、回收站、Read-before-Edit、配额 |
| `tools/views.rs` | 1,299 | `/messages`、`/inbox`、`/sessions`、`/config`、`/system` 动态视图 |
| `monitor/*` | 1,380 | qq/timer/internal 源、cron、batch、idle_only、backlog |
| `tools/memory.rs` | 804 | 记忆文件、索引、`memory_entries` 对账 |
| `ipc/*` | 795 | Windows 命名管道 / Unix UDS、帧协议、幂等 |
| `security/*` | 717 | 规则层 + Jev 分类器、hold、审计 |
| `session/turn_buffer.rs` | 371 | 崩溃恢复、缺失工具结果补齐 |
| `tools/download.rs` + `tools/net.rs` | 448 | 下载、SSRF 防护、域名策略 |
| `cli/backup.rs` + `cli/doctor.rs` | 878 | 在线备份、只读诊断 |
| `paths.rs` | 223 | `~/.sbot` 布局与逻辑根解析 |
| `agent/queue.rs` | 391 | 按 chat 去抖、上限、steering |
| **合计** | **10,052** | |

## 非代码资产

旧 `prompt.md`（7,012 B）、`config.example.toml`（4,876 B）、`llm.example.toml`（6,058 B）已不再作为 0.2 的输入：提示词改为 `assets/system_prompt_v4.template.txt`，配置 schema 以 `src/config/mod.rs` 与 README 为准。旧的示例文件保留在仓库中仅供历史对照，不参与构建。
