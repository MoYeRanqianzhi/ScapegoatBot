---
name: release-status
description: 0.2 当前发布状态、已实现能力与真实边界；接手下一轮工作或判断某项是否可用时先读
metadata:
  type: project
  scope: 整个 0.2 分支，随版本更新
  status: active
  last_verified: 2026-11-04
---

当前版本：`0.2.0-alpha.24`（`main`）。数据库迁移到 v11；测试 346 项通过，
`cargo fmt --check`、`clippy -D warnings` 通过；CI 已接入 GitHub Actions。

alpha.13 修掉了 2026-11-03 审查的 P0/P1 主体；alpha.14 补齐了当时列为未实现的项；
alpha.15 做了第二轮审查，修掉新代码的边界问题（复盘基线、发送记录、总超时、
保留天数、工具结果上限、receive_scopes 强制）。alpha.16 把 `[budget]`、
`BeforeSend` hook、`max_schema_tokens`、monitor `cc`/`on_missing`、`get_msg`
读范围从“只解析”接到执行路径。alpha.17 接完其余事件 hook、`max_state_tokens`、
默认 thinking，并删除两个无效字段；新增本地 WebSocket 模拟 NapCat 的集成测试。

- `[gate]`：trusted/auto/approve、速率、重复/跨群同文/@all、`llm_review`、
  IPC 审批（`sbot gate pending/approve/reject`）。
- owner intent：runtime 证据（控制通道 / 主人消息 / origin chat），跨 chat 与
  扩张动作强制，收缩与同 chat 回复豁免。
- 安全分类器 `backend = "llm"`；周期安全检查（high+complied → hold，
  critical → pause agent）。
- 压缩：模型窗口减 reserve 的阈值、旧工具结果裁剪、memory flush、post-compact
  索引注入、独立 compaction model。
- 记忆复盘 idle/every_turn 的真实语义；monitor catch_up；出站账本启动核对；
  Download 的公网 DNS 解析器；WebFetch final_url 复核。
- 结构：views 拆成 6 个子模块、ScopeCache、open_stores；全仓 rustfmt；CI。
- alpha.15：压缩后记忆复盘恢复；门禁只在发送成功后记账（短应答豁免）；master
  私聊通知免 owner intent；审查/判定总超时；`context_window = 0` 回退；保留天数
  与 `max_result_bytes` 真正执行；`receive_scopes` 覆盖视图与 API 查询。

仍存在的边界（细节见 `docs/guide/limits.md`）：

- `approve` 只走本地 IPC，没有 master QQ nonce；队列在内存里，重启丢失。
- `auto` 规则是启发式（没有跨 chat n-gram、媒体来源检查）。
- 周期检查使用独立窗口，不与主请求共享前缀缓存。
- 群级破坏动作（全员禁言/退群/解散）没有用户目标，不做 owner 保护。
- WebFetch 的抓取由后端完成，内容在 final_url 复核前已被取走。
- `approve` 已有 master 私聊 nonce 与数据库持久化（只存参数哈希）。
- `auto` 已有跨 chat 5-gram 与媒体来源（`media_origins`）拦截。
- 周期检查复用主 system 前缀（消息窗口独立）；`auto` 门禁已覆盖
  `context_sources`（Grep 不按来源拆分）。
- WebFetch 已支持 `local = true` 的本地逐跳抓取；默认后端模式仍是后端抓取后复核。
- `[hooks.events]` 的 rewrite 只在 BeforeSend；SessionEnd/Stop/ConfigChange 的 inject 无去处。
- `Agent message` 已能注入运行中的子代理（回合边界排空），结束后历史写回实例。
- NapCat 模拟测试已覆盖全部 `napcat.*` ActionSpec 的 wire action 与参数形状。
- 真实 API 未重跑；NapCat 协议用本地 WebSocket 模拟覆盖。

**Evidence:** `CHANGELOG.md` 的 alpha.14 条目、`cargo test --all-targets` 输出、
`.github/workflows/ci.yml`、`docs/guide/limits.md`。

**Recheck when:** 发布新 alpha、实现上述边界、或改变安全默认值（shadow/enforce）。

**Why:** 审查结论与后续实现分散在两次会话里；新会话需要知道哪些能力真的可用、
哪些只是“解析了配置”。

**How to apply:** 先跑三项检查确认基线；不要把 limits 里的边界在文档/提交里描述成
已支持；新增能力时同步 `docs/guide`、`limits.md` 与 `.agents/TODO.md`。
