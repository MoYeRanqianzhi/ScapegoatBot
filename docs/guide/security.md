# 安全模型

ScapegoatBot 的安全目标是：**模型可以自由行动，但不能悄悄把敏感内容送出去，
也不能在被判定危险后继续对外操作**。它不是操作系统级沙箱，也不防同机管理员。

## 两条闸

| 闸 | 时机 | 检查对象 |
|----|------|----------|
| 输入闸 | 每次模型请求发出前 | 这一步新进入上下文的文本（消息、工具结果、steering） |
| 输出闸 | 模型产出之后、任何副作用之前 | 文本、思考、工具调用参数 |

两层判定：

1. **规则层**（确定性正则）：凭据形态（`sk-…`、`api_key=…`）、提示注入话术、
   索要系统提示。规则命中**不受概率阈值影响**。
2. **Jev 分类器**（可选）：一次请求问一个 `noul` 问题，返回危险概率；
   达到 `threshold`（默认 0.9）即危险。超时、HTTP 失败、响应不可解析都按
   “无法判定即不安全”处理。

`mode = "shadow"` 只记录、不拦截；`mode = "enforce"` 命中即写
`~/.sbot/SECURITY_HOLD` 并进入 hold。

### 默认配置是什么

默认 `mode = "shadow"`、`backend = "jev"`、`base_url` 为空。也就是说：

- 开箱只跑规则层，且只记录、不拦截；
- 想真正拦截，必须配 `mode = "enforce"`，并二选一：
  - 配好 Jev：`base_url` + `auth.json` 的 `security.guard.api_key`；
  - 只要规则层：显式 `backend = "off"`。

**enforce 下没有可用分类器时，闸会 hold 每一个新回合**（fail-closed）。
启动与 `sbot doctor` 都会明确提示。`backend = "llm"` 尚未实现，配置校验会直接拒绝。

### Jev 配置

```toml
[security.guard]
enabled = true
mode = "enforce"
backend = "jev"
base_url = "https://your-jev-endpoint"
threshold = 0.9
timeout_ms = 800
```

```json
{ "security": { "guard": { "api_key": "sk-..." } } }
```

请求发往 `{base_url}/v1/systemone`，响应里的 `answers.<name>.noul` 作为概率。

## hold

命中后：

- 写 `~/.sbot/SECURITY_HOLD`（重启仍然有效）；
- 所有 `napcat.*` 动作返回 `Disabled during hold`，包括拉黑、禁言、踢人；
- `Download`、`WebSearch`、`WebFetch`、`MCP` 同样拒绝；
- `agent.*` 与 `api.*` 只读查询仍可用，agent 可以看状态、写记忆、自查。

hold **没有超时、没有自动恢复**。

### 恢复

```bash
sbot security resume
```

或 agent 自己调用 `API` 的 `agent.resume`。恢复流程：

1. 取最近一次被判危险的原始内容（内存里，不写磁盘）+ 当前历史尾部 + hold 原因；
2. 用同样的规则层/分类器复核；
3. 通过才删除 `SECURITY_HOLD`，否则继续 hold，并返回原因。

`shadow` 模式下没有真的拦截过内容，但 rescue 仍然要过真实复核，不会再因为
固定字符串而一键清除。

`provider stop_reason = refusal` 会直接 `force_hold`，不重试、不换模型。

## 主人保护

目标为 `owner_qq` / `master_qq` 的 destructive action 无条件拒绝：

- 踢人、禁言、改名片、给/撤管理；
- 目标 QQ 可以是数字或数字字符串；解析失败按拒绝处理，不会跳过保护；
- 全员禁言、退群、解散群、群公告这类**群级动作没有用户目标**，不做主人保护。
  如果这些动作风险不可接受，请在 `[agent].send_scopes` / `[filter]` 与提示词层面约束。

`requires_owner_intent` 目前只是注册表元数据（`api.list` / `api.help` 可见），
逐动作审批尚未实现。

## 发送范围

`[agent].send_scopes` 控制能发到哪些 chat：

- 空列表 = 禁止一切发送（fail-closed）；
- `["*"]` = 不限制；
- 否则只允许列出的 `group:<gid>` / `private:<qq>`。

## 数据与密钥

- 消息 JSONL、transcript、blob 内容用 XChaCha20-Poly1305；不同用途的密钥由
  HKDF 从主密钥派生。
- 文件头记录算法；改 `[storage].encryption` 不会让旧历史失效。
- `~/.sbot/key` 是唯一秘密，权限 0600；`sbot backup` **包含它**。
- 索引（SQLite）与路径是明文元数据：能看出“谁在什么时候聊过”，看不到内容。
- 威胁边界：不防同机管理员、不防进程内存读取、不防拿到 key 的人。

## 已知未实现项

| 项 | 现状 |
|----|------|
| `[gate].mode`（trusted/auto/approve）与 `max_per_minute` | 只解析，不执行 |
| `[gate].llm_review` | 只解析，不执行 |
| `[security].check_every_tokens` 周期检查 | 只解析，不执行 |
| `[security.guard]` 的 `max_state_tokens` / `delta_every_tokens` | 预留，闸使用固定窗口 |
| `requires_owner_intent` | 只作为元数据 |
| WebFetch 的重定向与最终地址复核 | 由你配置的后端负责 |

这些项在 [limits.md](limits.md) 也列出；启用前请不要把它们当成已有的防护。
