# 故障排查

先跑这两条，大多数问题能直接定位：

```bash
sbot status          # 需要守护进程在跑；输出一份 JSON
sbot doctor          # 只读检查配置、凭据、密钥、数据库与索引；守护进程没跑也能用
```

`doctor` 不修数据；它报 `ERROR` 的地方按提示修。它不检查 MCP 服务器，也不连 NapCat。

日志写到 stderr，默认级别 `info`；要看下面标着“debug”的行，用环境变量 `RUST_LOG=debug`
启动。由服务管理器托管时日志在它那里（`journalctl -u sbot`）；用 `sbot restart` 拉起的进程
没有日志（输出接到空设备）。

## 守护进程起不来

前台跑 `sbot start`，错误直接打在终端上。

| 现象 | 原因与处理 |
|------|-----------|
| `another sbot instance is already running` | 同一 `SBOT_HOME` 已有进程；先 `sbot stop`，或换 `SBOT_HOME` |
| `another instance is already listening on …`（Unix）/ `cannot create pipe …`（Windows） | 另一个实例用了同一个 `[ipc].name`；给其中一个改名 |
| 配置解析或校验错误 | 严格 schema：不认识的键报 `unknown field`，校验错误带字段路径（如 `ipc.name`）；`version` 必须是 1；主模型不在 `[agent].available_models` 里也是校验错误 |
| `encryption canary does not match; refusing to open the database` | 数据库不是用这把 `key` 加密的。典型原因：换机器时没带 `key`，启动时自动生成了一把新的。**用备份里的 `key` 覆盖现在这个新文件**，不要保留新生成的 |
| `key file … has an unknown format` / 读不了 | 主密钥文件坏了，或运行用户读不了它；从备份取回，不要删了重新生成 |
| 启动后过一阵退出，退出码非 0 | 内部错误（panic）：进程立刻整个退出，由服务管理器拉起；日志最后几行是原因 |

`key` 的权限过宽（不是 0600）不会让它起不来，只有 `sbot doctor` 会报
`key permissions: … (expected 600)`。

## NapCat 连不上

`sbot status` 的 `agent.state` 是 `waiting_for_napcat`，日志反复出现：

```text
waiting for NapCat; the agent starts once it is connected and logged in
```

这一行带着原因：

| 原因字段里的话 | 含义 |
|----------------|------|
| `cannot connect to NapCat` | 连不上：NapCat 没开、地址或端口不对、防火墙 |
| `the NapCat connection attempt timed out` | 连接超时 |
| `NapCat did not answer get_login_info in time` | 连上了但不应答：多半连到了别的服务，或 token 不对 |
| `NapCat is running but no QQ account is logged in` | NapCat 在跑，QQ 没登录 |

检查：NapCat 是否启动、QQ 是否登录；OneBot 11 的 **WebSocket 服务器**是否开启，地址与
`[napcat].url` 一致；设了 token 的话是否写进 `auth.json` 的 `napcat.access_token`。

这段时间 agent 还没建起来：`sbot chat` 等命令返回 `not_ready`。守护进程一直重试，NapCat
恢复后自己接上，不需要重启 sbot。

运行中断线是另一种情况：日志 `NapCat connection lost; QQ actions fail until it is back`，
agent 还在，QQ 动作返回 `NapCat is not connected`，重连后日志 `reconnected to NapCat`。

## 不回复

按顺序看：

1. `sbot status` 的 `agent.state` 是不是 `running`。`no_provider`：日志里有
   `the primary model is unusable`，往上找 `cannot build a provider`（带原因）或
   `missing API keys in auth.json`；改好后重启。
2. `estop`、`hold` 是不是 `true`，`ai_enabled` 是不是 `false`。（`ai stop` 只在内存里，重启就
   恢复；ESTOP 是文件，重启仍在。这两种跳过回合时只记 debug 日志。）
3. 这条消息本来就不唤醒它：默认只有 @ 本账号（含 @全体）的消息与主人的消息触发回合。
4. 被 `[filter]` 丢了：看 `agent.filtered_messages` 有没有涨（对应的日志
   `event rejected by the admission filter` 是 debug 级）。
5. 预算用完了：日志 `skipping this turn`，原因是
   `daily token budget exhausted (…); next turn after local midnight`（或 `daily USD budget …`），
   或 `chat turn budget exhausted; skipping this turn`（这个会话一小时内的回合数到了上限）。
6. 回合跑了（日志 `turn completed`）但没发出来：群不在 `send_scopes` 里；`[gate]` 在等确认
   （`sbot gate pending`）；发送规则拦了（agent 得到 `blocked_by_gate: …`）；这个会话一小时
   内的发送数到了上限。
7. 都不是：它读了，决定不回。

## hold

```bash
sbot security status
```

返回 `hold`、`estop` 与最近 20 条安全事件；hold 的原因在事件里（规则命中、分类器、还是周期
检查）。`[gate]` 的每次放行也记安全事件，动作多的时候 20 条很快被占满——原因也写在
`~/.sbot/SECURITY_HOLD` 文件里，可以直接看它。

agent 被设计为自己解除 hold：下一次被唤醒时它会读 `agent.status`、隔离可疑内容、调用
`agent.resume`。你在宿主机上时可以手动触发同一次复核（需要 agent 在运行）：

```bash
sbot security resume
```

返回 `The hold stays: your current context still looks unsafe (…)` 时看括号里的原因：

- 是规则或分类器的判定：上下文里仍有被判危险的内容。让 agent 用 `agent.quarantine` 把那段
  原文换成占位符，或 `agent.clear_context` 重新开始，再复核。
- 是 `classifier unavailable: …`：分类器本身用不了或超时，隔离解决不了。检查
  `[security.guard]` 的端点与 key；`llm` 后端把 `timeout_ms` 调大。

**enforce 且没有可用的分类器**时每个回合都会被 hold（启动日志
`security.guard.mode = enforce but no classifier is configured`，`doctor` 也会提示）。出路
三选一：给 Jev 配 `base_url` 加 key；改用 `backend = "llm"`；或者显式 `backend = "off"`
只跑规则层。

## ESTOP

`~/.sbot/ESTOP` 存在即暂停新的 QQ / monitor 回合与子代理：

```bash
sbot resume          # 删除 ESTOP
```

它不挡宿主机上的 `sbot chat`（暂停期间主人仍然可以问它话）；`sbot ai stop` 才连
`sbot chat` 一起挡。两者都不影响消息接收与存储。

## 历史读不出来

- 换过机器但没带 `~/.sbot/key`：守护进程拒绝启动，见上面“起不来”的 canary 一行。
- 改过 `[storage].encryption`：密文在任何配置下都读得出；从 `none` 改回加密之后，调试期间
  明文写下的行不再读取，日志里记为跳过（`unencrypted line in an encrypted store`）。要读
  它们就把配置改回 `none` 并重启；注意这期间新写下的内容也是明文，读完改回加密。
- 消息文件数与索引里的对不上（手工动过 `data/messages/` 下的文件）：启动时发现数量不等就
  重建索引；重建时读不出的文件、解不开的行只记一条 `skipped` 警告并跳过，其余消息照常
  补回。`doctor` 报告两边的数量。
- 索引指到了别的消息上（日志 `message index line points at a different message`）：那条
  消息读出来是空的。这种情况不会自动重建。

## 回合卡住 / 很慢

- 回合到了硬上限会以错误结束，日志或 agent 看到的是：`step limit N reached`
  （`[agent].max_steps`）、`turn token budget exhausted`（`[budget].turn_tokens`）、
  `turn wall-clock budget exhausted`（`[budget].round_trip_wall_secs`）；
- provider 重试与回退链会拉长耗时：看日志里的重试与 fallback；
- 上下文过大触发压缩：压缩本身要一次模型请求；
- 主 agent 回合是串行的：前一个回合未结束时，新的唤醒会等待它完成。

## 磁盘与配额

这些是 agent 的工具调用得到的错误，不是日志行：

| agent 看到的错误 | 含义与处理 |
|------------------|-----------|
| `VFS quota exceeded (… of … bytes)` | agent 的文件到了 `[storage].max_vfs_bytes`。它看的是活跃用量（不含 `/trash`）：让 agent 删掉不用的文件，或调大上限后重启；从 `/trash` 恢复文件同样要占活跃配额 |
| `node limit exceeded (100000 nodes)` | 文件与目录的个数到了上限（不可配置） |
| `not enough free disk space (…)` | 宿主机磁盘余量低于 1 GiB，VFS 拒绝写入；先腾空间 |

用量看 `sbot status` 的 `vfs` / `messages` / `sessions`，或让 agent 读 `/system/storage`。
消息归档与对话记录超过 `max_message_bytes` / `max_session_bytes` 时自动删最旧的（消息按整月、
不删当月；对话记录删最旧的已关闭分段）。后台每 5 分钟检查一次，VFS 到配额的 90% 或磁盘
低于下限时发内部事件 `storage:quota` / `storage:disk`（可以用 monitor 订阅）。

## MCP

- 服务器启动失败只告警，不影响主进程：日志 `MCP server failed to start`（带服务器名与
  原因）。检查 `command` / `args` 能否在守护进程的环境里直接运行；
- 服务器自己的 stderr 被丢弃，看不到它打印的报错；先在终端里手工跑一遍那条命令；
- 单次请求 30 秒超时；结果被截断会显式标注。

## 命令报错

`status`、`chat` 等命令经命名管道 / Unix 套接字发给守护进程：

| 报错 | 含义 |
|------|------|
| `Is the daemon running?` | 连不上：没在跑，或者命令用的 `SBOT_HOME` / `--config` 不是它用的那一份 |
| `wrong_instance` | 连上的是另一个实例（两个实例用了同一个 `[ipc].name`）；守护进程会说明它服务的是哪个目录 |
| `not_ready` | agent 还没建起来（在等 NapCat，或主模型不可用） |
| `ai_stopped` | 有人执行过 `sbot ai stop`；先 `sbot ai start` |
| `protocol_version` | `sbot` 命令与守护进程不是同一个版本：升级了二进制但没重启守护进程 |

Unix 下套接字在 `$XDG_RUNTIME_DIR/<name>.sock`，守护进程没有这个变量时在
`<SBOT_HOME>/run/<name>.sock`。CLI 先试自己环境算出的位置，再试 `<SBOT_HOME>/run/`，所以
“守护进程是 systemd 系统服务（没有这个变量）、CLI 在登录 shell 里（有）”不需要对齐；反过来
——守护进程有这个变量而 CLI 没有或不同——就连不上，要给 CLI 设成同一个值。套接字所在目录
必须只有当前用户能进入，启动时会收紧到 0700，收紧不了就拒绝启动。

## 提交问题前

请附上：

- `sbot --version`、`sbot status` 与 `sbot doctor` 的输出；
- 出错时间段的日志；
- 操作系统与复现步骤（配置片段可脱敏）。

发出去之前检查一遍里面没有 key、token 或聊天原文。
