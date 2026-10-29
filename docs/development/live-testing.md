# 真实 API 与实机测试记录

> 2026-10-30，`0.2.0-alpha.10`。本文只记录可复现的命令与结果，不包含任何凭据；
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

## 4. `sbot doctor` / `sbot backup`

```bash
sbot doctor    # 12 ok, 2 warning, 0 error（schema v8, per_chat 模式）
sbot backup --output <dir>
```

备份包含 `data.db`、`key`、配置、消息、分段与 blob；警告明确“归档包含主密钥”。

## 5. 未通过的外部条件

- `127.0.0.1:43210` 的 `/v1/models` 与 `/v1/chat/completions` 返回 502，
  是端点自身故障；同一 key 在 `32100` 正常。
- NapCat 未运行（`ws://127.0.0.1:3001` 拒绝连接），QQ 收发未实机验证；
  QQ 事件归一化、过滤、队列、重连由单元测试覆盖。
