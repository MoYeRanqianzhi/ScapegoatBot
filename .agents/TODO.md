# TODO

按优先级排列；每项都应在完成后更新 `CHANGELOG.md` 与 `docs/guide/limits.md`。
已完成项直接删除，不保留历史（git 有记录）。

## 安全能力

- [ ] 实现 `[gate]`：`trusted` / `auto` / `approve` 三种模式、`max_per_minute` 与
      `llm_review`；审批通道走 IPC，超时 fail-closed。改完把 `limits.md` 对应行删掉。
- [ ] 周期安全检查：按 `[security].check_every_tokens` 在回合边界跑独立模型检查，
      结果不进主 transcript；超时放行、连续失败告警（设计 §5.10 / prompt catalog §11）。
- [ ] 实现 `[security.guard].backend = "llm"` 或用主 provider 做二次判定；
      实现后允许配置该值，并补分类器失败路径测试。
- [ ] 评估 `requires_owner_intent`：要么实现发送前 owner 证据检查，要么从注册表删除，
      避免继续以元数据冒充防护。

## 数据与运行

- [ ] `[compaction]` 剩余调优键：`reserve_tokens`（结合模型窗口）、`prune*`（旧工具结果
      裁剪）、`memory_flush` / `post_compact_recall`、`compaction_model`。
- [ ] `[memory].idle_minutes` 与 `review = every_turn` 的真实语义（当前共用一个定时循环）。
- [ ] `[monitor.lifecycle].catch_up`：停机期间的事件补投或合并。
- [ ] WebFetch：逐跳复核重定向，或改为本地抓取；DNS rebinding 用 resolver 固定 IP。
- [ ] 收发账本 `outbound_intents` 目前只写不读：启动时核对 pending，避免“已发送但被当失败重试”。

## 结构

- [ ] `src/tools/views.rs`（约 1600 行）按 messages/inbox/sessions/config/system
      拆成 `views/` 子模块；方法逐个改 `pub(super)`。
- [ ] `Runner` 仍是 20+ 字段的聚合体：把 per-chat 的 session/history 缓存与压缩状态
      抽成独立结构；`execute` / `run_injected` / `run_subagent` 的 AgentLoop 组装可再合并。
- [ ] 评估 `App::Runtime` 的启动流程（175 行）能否按“存储 → 安全 → QQ → agent”继续拆小。

## 工程

- [ ] 加 CI（GitHub Actions）：`cargo test --all-targets`、`cargo clippy -- -D warnings`、
      `cargo fmt --check`；真实 API 测试保持 `#[ignore]`。
- [ ] `cargo fmt` 纳入提交前检查（当前仓库未统一格式化）。
- [ ] 观察 alpha.13 的压缩与恢复路径在真实流量下的表现，补压测/长跑用例。

## 文档

- [ ] 每个发布版本同步 `docs/development/live-testing.md`（记录版本号与实测命令）。
- [ ] 若实现上述任一配置项，先改 `docs/guide/configuration.md`，再删
      `docs/guide/limits.md` 的对应行。
