# 真实 API 与实机测试记录

> 2026-10-30，`0.2.0-alpha.12`。本文只记录可复现的命令与结果，不包含任何凭据；
> 测试 key 只放在本机 `.agents/MEMORY.local.md` 或 `auth.json`。

## 1. 安全分类器（Jev）

```bash
SBOT_LIVE_JEV_BASE_URL=https://jev.anyrouter.mom \
SBOT_LIVE_JEV_API_KEY=<local key> \
  cargo test --test live_providers jev -- --ignored --nocapture
```

结果：

```
jev benign verdict: score 0.01, dangerous false
jev credential verdict: score 0.87, dangerous true
test jev_classifier_separates_benign_from_credentials ... ok
```

## 2. 主模型（OpenAI Chat Completions 协议）

本机 `127.0.0.1:32100` 提供 OpenAI 兼容端点（模型 `deepseek-flash`）。
`43210` 在测试时返回 502（上游 worker 故障），因此改用 32100。

```bash
SBOT_LIVE_BASE_URL=http://127.0.0.1:32100 \
SBOT_LIVE_API_KEY=<local key> \
SBOT_LIVE_MODEL=deepseek-flash \
  cargo test --test live_providers chat -- --ignored --nocapture
```

结果：

```
live chat response: "pong"
test chat_provider_streams_from_a_real_endpoint ... ok
```

## 3. 守护进程 + 真实模型回合

`SBOT_HOME=G:\sbot-e2e-live`，`models.json` 指向
`http://127.0.0.1:32100`（`api = "chat"`，模型 `deepseek-flash`），
`auth.json` 明文保存本机 key，NapCat 不可用时守护进程照常启动。

```bash
SBOT_HOME=... sbot start
SBOT_HOME=... sbot chat "Reply with exactly: live-ok"
# 等待后台控制回合完成
SBOT_HOME=... sbot stop
```

日志与落盘结果：

```
INFO control turn finished stop=EndTurn turns=1
data/sessions/20261029T221048Z-800.jsonl  4 lines, 2258 bytes
schema_version = 8
```

`4 lines` = 文件头 + 控制输入 + 模型回复（+ runtime/工具消息，按回合实际内容）。
这证明真实模型调用经过了 agent loop、输出闸、transcript 加密写入与回合提交。

## 4. 完整 QQ 管线（真实 OneBot 协议 + 真实模型）

NapCat 未安装，因此用仓库内的 `examples/mock_onebot` 启动真实的 OneBot 11
WebSocket 服务端；守护进程使用真实 `deepseek-flash` 模型。

```bash
cargo run --example mock_onebot -- 39001   # 本地 OneBot 服务端
# sbot.toml 的 [napcat].url = "ws://127.0.0.1:39001"
sbot start
# mock 在 get_version_info 后 3 秒推送一条群消息
```

实测 mock 收到的 action：

```
get_login_info
get_version_info
send_group_msg {"group_id":42,"message":[{"type":"text","data":{"text":"在，怎么了"}}]}
mark_group_msg_as_read {"group_id":42}
```

守护进程日志：`connected to NapCat qq=10000 nickname=MockBot`、
`listening for QQ events`；transcript 17 行、加密落盘。
这验证了 WebSocket 连接、登录信息、入站事件归一化、准入过滤、按 chat 队列、
真实模型回合、`API` 发送、发送账本与已读标记的完整链路。

同时 `tests/live_napcat.rs` 以真实 WebSocket 服务端验证：
`get_login_info` → `send_group_msg`（参数与 message_id 校验）→ 收到群消息事件
并通过 `normalize` 得到 `ChatId::Group(42)`、发送者 10001、文本 `hello`。

## 5. `sbot doctor` / `sbot backup`

```bash
sbot doctor    # 12 ok, 2 warning, 0 error（schema v8, per_chat 模式）
sbot backup --output <dir>
```

备份包含 `data.db`、`key`、配置、消息、分段与 blob；警告明确“归档包含主密钥”。

## 6. 未通过的外部条件

> 2026-10-30 用户确认：**接受协议级验证**，不要求连接腾讯 QQ 服务器。
> 因此真实 QQ 登录/收发由「真实 OneBot 11 WebSocket + 真实模型」的完整链路替代，
> 作为本版本的验收口径。

- 没有真实 QQ 账号/NapCat 登录；QQ 协议层由上述 mock
  OneBot 服务端完整验证（真实 WebSocket、真实模型、真实发送）。
- `127.0.0.1:43210` 的 `/v1/models` 与 `/v1/chat/completions` 返回 502，
  是端点自身故障；同一 key 在 `32100` 正常。
