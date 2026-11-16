---
name: live-testing
description: 怎么用真实模型、真实 Jev 分类器与本地 OneBot 服务端做端到端验证，以及用户接受的验收口径；跑 live 测试或要证明“整条链路通了”时读
metadata:
  type: reference
  scope: tests/live_providers.rs、examples/mock_onebot.rs、守护进程的端到端验证
  status: active
  last_verified: 2026-11-17
---

**验收口径**（用户 2026-10-30）：接受协议级验证，不要求连接腾讯 QQ 服务器。真实 QQ 登录 / 收发
由“真实的 OneBot 11 WebSocket + 真实模型”的完整链路替代。

端点与凭据各人自备，不写进任何共享记录。

**真实 provider 与分类器**（默认忽略，要显式给环境变量；变量名以 `tests/live_providers.rs`
为准）：

```bash
SBOT_LIVE_BASE_URL=... SBOT_LIVE_API_KEY=... SBOT_LIVE_MODEL=... \
  cargo test --test live_providers chat -- --ignored --nocapture
SBOT_LIVE_JEV_BASE_URL=... SBOT_LIVE_JEV_API_KEY=... \
  cargo test --test live_providers jev -- --ignored --nocapture
```

**守护进程 + 真实模型，不要 NapCat。** 用一个临时的 `SBOT_HOME`，`models.json` / `auth.json`
指向真实端点。NapCat 连不上时 agent 不会建起来（`agent.state = waiting_for_napcat`，
`sbot chat` 返回 `not_ready`），所以这条路也要先起下面的 mock。

**完整 QQ 管线。** `examples/mock_onebot.rs` 是一个真的 OneBot 11 WebSocket 服务端：

```bash
cargo run --example mock_onebot -- 39001
# 临时 SBOT_HOME 的 sbot.toml：[napcat].url = "ws://127.0.0.1:39001"
SBOT_HOME=<临时目录> sbot start
SBOT_HOME=<临时目录> sbot chat "Reply with exactly: live-ok"
```

判据：守护进程日志里有 `connected to NapCat`、`agent runtime ready`；`sbot chat` 之后有
`control turn finished stop=EndTurn`；mock 一侧打印它收到的 action（`get_login_info`、
`send_group_msg` …）；`data/sessions/` 下出现加密的分段文件。这一条覆盖 WebSocket 连接、
事件归一化、准入过滤、队列、模型回合、`API` 发送、发送账本与落盘。

不需要任何外部条件的同类检查随 `cargo test` 跑：`tests/onebot_mock_client.rs`、
`tests/napcat_api.rs`。

用完删掉临时的 `SBOT_HOME`（里面有明文 key）。

**Evidence:** 原 `docs/development/live-testing.md`（alpha.12 的实测记录，2026-11-17 随目录
删除，git 历史可查）；环境变量名、example 与测试文件于 2026-11-17 核对存在；日志原文在
2026-11-16 写 skill 时对照代码核实过。alpha.12 之后“NapCat 不可用时照常能跑控制回合”不再
成立（现在要等 NapCat 连上才建 agent），所以上面要求先起 mock。

**Recheck when:** live 测试的环境变量改名；mock 的启动方式变化；agent 的启动条件变化。
