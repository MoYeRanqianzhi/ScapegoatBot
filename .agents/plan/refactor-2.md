# Refactor 2：剩余问题与加强重构

基线：main `e8da707`（365 tests passed / 2 ignored，clippy 干净）。工作树在 `.worktrees/`。

## 已核实的问题

| 问题 | 证据 | 状态 |
|------|------|------|
| steering 早退丢消息 | `agent/runner.rs:1362/1366/1370/1444/1466` 直接 return，`steering_rx` 未排空 | 并行修复中 |
| transcript 恢复硬上限 4096 | `session/mod.rs:586` 扫描 `1..=4096` | 并行修复中 |
| `image_compress` 强制 JPEG | `tools/api.rs:672` 要求 `.jpg` | 并行修复中 |
| 死代码 / `allow(dead_code)` | `qq/resync.rs` 无调用；`storage::idempotency_prune` 无调用；`lib.rs` 逐模块 allow | 并行清理中 |
| app.rs 上帝结构（1990 行） | `assemble_tools`/`build_agent`/IPC/事件处理同文件 | 并行拆分中 |
| config sections 1876 行 | `config/sections.rs` | 并行拆分中 |

## 误报（已从 TODO 删除）

- `agent.compact` 在控制/monitor 回合压 owner private：`run_injected` 本来就跑在 owner private 链（`runner.rs:1028`），回退值就是当前链，不是压错。
- per-scope 计数器：只在 `session_scope = per_chat` 时有意义；当前部署单会话，shared 由 turn_lock 串行。per_chat 是文档功能（`docs/guide/configuration.md:76`），是否删除由用户决定。

## 并行分支

| 分支 | 范围 | 互斥文件 |
|------|------|----------|
| `refactor/session-recovery` | AAD 恢复上界 + 测试 | `session/mod.rs` |
| `refactor/images` | media 格式 + 图片测试 | `tools/api.rs`、providers 测试 |
| `refactor/cleanup` | 死代码 + config sections 拆分 | `qq/`、`storage/`、`lib.rs`、`config/` |
| `refactor/app-structure` | app.rs 拆模块（纯移动） | `app.rs` |
| `refactor/runner-core` | steering 早退修复 + 回归测试 | `agent/runner.rs` |

合并顺序：session → images → cleanup → app-structure → runner-core。全部本地 merge，不 push。

## 阶段 2（合并后）

1. 统一回合并发：`Runner` 持有 `Arc<Semaphore>`（大小 `queue.max_concurrent`），
   `ChatQueue` 复用同一信号量；`run_control`/`run_event` 也从它取 permit；
   删除写死的 `control_slots = Semaphore::new(4)`。shared 模式下 turn_lock 仍保证串行，
   该改动消除 per_chat 下的超额并发与控制 turn 的独立配额。
2. 拆 `agent/runner.rs`（3132 行 / 25 字段）为 `agent/runner/` 子模块：
   `scope.rs`（ScopeCache、session/history/turn_lock）、`compaction.rs`、
   `injected.rs`（control/monitor）、`images.rs`（入站图片解析）、`review.rs`。
   Runner 只保留回合执行与编排。
3. 图片链路补测：inbound resolver、provider image body、User 图片 estimate/prune；
   真实 `get_image` 契约需启动 NapCat 实测（服务器服务当前停止，待授权）。

## 未决

- `per_chat` 是否作为产品能力保留：当前单会话部署用不到，代码与文档仍支持。
- transcript 位置完整性：候选扫描会接受被移动但可解的行；链式/签名方案需要迁移设计。
- alpha.35 未打 tag、未部署；服务器 `sbot.service` 停止。
