# TODO

剩余问题；已完成项删除（git 有记录）。当前交接计划见
`plan/2026-11-06-image-tools-and-review-handoff.md`。

## 构建规则

- 子代理禁止运行任何 `cargo` 命令；构建只在主 checkout 用默认 `target/` 统一执行。
- 不使用 C: 盘放构建产物；G: 空间不足时先清 worktree target / incremental，再报告。
- 本机 cargo 访问 crates.io 的 TLS/schannel 失败；需要拉依赖时用交接计划 §5 的 rsproxy
  `--config` 命令，不要把镜像配置提交进仓库。

## 当前基线

- HEAD `9f52e00`（图片代码 `48fe69f` + 交接文档）；`cargo test --locked --all-targets` = 377 passed（365 lib + 12 integration），
  2 ignored；clippy/fmt 通过。
- 本地 tag `v0.2.0-alpha.35` 落后 HEAD 4 个提交；尚未 push/release/deploy。
- 服务器 `hw1` 的 `sbot.service` 仍停止；NapCat 未登录（QR 登录失败）。

## 待用户确认

- [ ] 审核新的 `image.*` 5 个 action（`info`/`transform`/`convert`/`text`/`fonts`）的命名与参数；
      确认前不要继续扩展图片 API。完整表见交接计划 §2。
- [ ] 是否授权 `git push origin main --tags`、GitHub release、部署 hw1。

## 图片

- [ ] 真实 `get_image` 契约未实测：NapCat 从 2026-11-05 05:22 起退出登录，WS 握手 reset；
      需用户扫码登录后重试。
- [ ] `inbound_image_prompts` 的真实 HTTP 下载（QQ CDN）未测；需登录后的图片消息。
- [ ] `image.text`/`image.fonts` 只在 Windows（有系统字体）上验证过；Linux 服务器无字体，
      需在安装字体后再验证，或接受“返回空列表/明确错误”的行为。
- [ ] 不在仓库内置字体，也不引入字体资产；`image.text` 只使用系统字体（用户明确要求）。

## 结构（按交接计划 §3.1/§3.2 执行）

- [ ] `app/assembly.rs`（1017 生产行）拆分：`open_stores`、`assemble_tools`、`build_agent`、
      hot reload；拆前先补装配层测试。
- [ ] `agent/runner.rs`：`loop_config`/`run_subagent` 重复、预算跳过图片持久化重复、
      跳过检查与 `injected.rs` 重复、重复 `debug!("turn completed")`、`commit_turn` 参数对象化。
- [ ] `agent/loop.rs`：`run_with_steering`（244 行）提取预算/输入闸/请求/输出闸/终止分支；
      stream/render 逻辑移出。
- [ ] `monitor/mod.rs`：拆 predicate/cron/delivery/config；`Monitor` 23 字段分组。
- [ ] `vfs/sqlite.rs`：`edit_text`/`read` 委托、usage SQL、`write_blob_tx`、
      `copy_node` staging 漂移。
- [ ] `tools/api/policy.rs`：`run_napcat` 223 行按顺序拆 guard 阶段，保留顺序测试。
- [ ] 冗余：Monitor spec 解析（约 110–130 行）、图片 mime/压缩/附件 helper（45–55 行）、
      provider `finish`（25–30 行）、session/messages 信封与写模式（50–60 行）、
      `complete_text` 错误映射、`self_message_from_params`/`parse_segment`、
      at/timer 解析、config JSON 读取。
- [ ] 断裂空格错误串：`policy.rs:92/112/125/231`、`action_gate.rs:663/680/706`、
      `web.rs:345/351`、`assembly.rs:416`；加 grep 级检查。
- [ ] 8 个零引用 `pub fn` 删除或补文档；`prompt/mod.rs:37` 的 `Box::leak` 视情况改 `OnceLock`。

## 文档

- [ ] `docs/development/prompt-maintenance.md:59` 与 `docs/guide/tools-and-vfs.md:82` 的
      “来源门控/唯一写入口”承诺未实现；删除承诺或实现 `/MEMORY.md` 写保护与 source 过滤。
- [ ] `sbot status` 文档按实际 JSON 重写（`README.md:109`、`docs/guide/usage.md:12/30-34`）。
- [ ] 默认值/热更新：`configuration.md:44`（max_steps 64）、`:48`（retry_max_ms 30000）、
      `usage.md:106` 与 `configuration.md:82`（filter+queue）统一。
- [ ] `security.md:211` 与 `limits.md:21` 的周期检查前缀描述统一为“复用 system 前缀”。
- [ ] `docs/development/README.md:30` v1–v12、`:47` 删除 ScopeCache 先例、`:41` 补 `stream`；
      `README.md:183` 目录树改 `app/`。
- [ ] `config.example.toml:45` compaction_model 说明改为“非空时用于压缩摘要”；
      `docs/guide/README.md:18` 工具数、`:64-65` 残缺句子。
- [ ] 源码注释：`tools/mod.rs:8-9` Permission、`swarm/mod.rs:9` 单层、`hooks.rs:3-5`、
      `prompt/mod.rs:31`、`config/mod.rs:112`、`tools/api.rs:5/220` 命名空间数量、
      `agent.compact chat` 语义、`runner.rs:158`、`turn_buffer.rs:65-66`。
- [ ] `.agents/plan/refactor-2.md` 标记 superseded（tag/per_chat/resync/control_slots 已过期）。

## 发布 / 外部

- [ ] push tag / GitHub release / 服务器部署需用户确认；服务器 `sbot.service` 仍停止。
- [ ] 发布前重指 `v0.2.0-alpha.35` 到最终 HEAD，重跑 release build；
      release build 上次验证早于 image 依赖与 `48fe69f`。
- [ ] 历史重写后其他 clone 必须重新 clone 或
      `git fetch --all --prune --tags --force && git reset --hard origin/main`；旧 SHA 失效。
