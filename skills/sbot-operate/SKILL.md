---
name: sbot-operate
description: Keep a ScapegoatBot (`sbot`) installation healthy and fix it when it is not - read `sbot status` and `sbot doctor`, find out why it does not reply, deal with a security hold or ESTOP, daemon that will not start or keeps restarting, NapCat disconnects, back up and restore (the backup layout differs from the live directory), disk and quota, what the security model does and does not protect, and what this version cannot do. Use whenever a running or previously working sbot misbehaves, before any risky change to `~/.sbot`, or when asked about backups, security or limits of the bot.
---

# 运维 sbot

sbot 是无人值守的：主人只在 QQ 里和它说话，没有人守在主机上。运维的目标是让它在没人管的
时候自己恢复——由服务管理器自动重启（`sbot docs sbot-deploy`）、hold 由 agent 自己解除、
断线自己重连。需要你上机器的情况应该是例外，下面就是这些例外。

参考文件：

- `sbot docs sbot-operate troubleshooting`：按现象查的排查表；
- `sbot docs sbot-operate security`：安全模型、闸、hold、主人保护、威胁边界；
- `sbot docs sbot-operate limits`：这个版本明确做不到的事。

## 第一步永远是这两条

```bash
sbot status          # 需要守护进程在跑；输出一份 JSON
sbot doctor          # 只读，不需要守护进程；有 ERROR 时退出码非 0
```

`doctor` 的输出每行是 `ok` / `warn` / `ERROR` 加一句说明，最后一行是三者的计数。它不修任何
东西；`ERROR` 的那几行就是要修的地方。

`sbot status` 连不上（`Is the daemon running?`）：守护进程没在跑，或者你的 `SBOT_HOME` /
`--config` 不是它用的那一份（由 systemd 托管时记得带上同一个用户和 `SBOT_HOME`）。

## 读 `sbot status`

| 字段 | 含义 | 要注意的值 |
|------|------|-----------|
| `version`、`pid`、`uptime_secs` | 版本、进程、已运行秒数 | `uptime_secs` 总是很小：进程在反复崩溃重启，看日志 |
| `home`、`config_path` | 这个守护进程服务的目录与配置 | 不是你以为的那个：连到别的实例上了 |
| `agent.state` | `running` / `starting` / `waiting_for_napcat` / `no_provider` | 后两个见下面 |
| `napcat.connected` | WebSocket 此刻是否连着 | `false`：没连上或断了，它一直在重试 |
| `agent.self_id` | 登录的 QQ 号 | `null`：agent 还没建起来 |
| `ai_enabled` | 是否允许模型调用 | `false`：有人执行过 `sbot ai stop`（只在内存里，重启就恢复） |
| `estop` | 是否暂停新回合 | `true`：有人执行过 `sbot pause`，或 `ESTOP` 文件存在 |
| `hold` | 安全闸是否熔断 | `true`：所有 QQ 动作被拒绝，见“hold” |
| `agent.filtered_messages` | 启动以来被 `[filter]` 丢弃的消息数 | 一直在涨而它不回复：过滤规则太宽 |
| `unconfirmed_sends` | 不知道结果的发送数：发出去了，但崩溃或超时发生在记下结果之前 | 不为 0：这些消息可能已经送达也可能没有，sbot 不会自动重发，由 agent 查历史后决定 |
| `gate.pending_approvals` | approve 模式下等人确认的动作数 | 见 `sbot docs sbot-use` 的“审批模式” |
| `gate.mode`、`guard.*` | 门禁与安全闸的当前配置 | |
| `model`、`thinking_level`、`available_models` | 当前模型、effort 与白名单 | |
| `vfs`、`messages`、`sessions` | 各部分占用的字节数与条数 | 见“磁盘与配额” |
| `schema_version` | 数据库 schema 版本 | |

`agent.state` 不是 `running` 时：

- `waiting_for_napcat`：NapCat 没开、QQ 没登录、`[napcat].url` 或 token 不对；日志里的
  `waiting for NapCat` 一行带着具体原因。这段时间 agent 还没建起来，`sbot chat` 返回
  `not_ready`。守护进程一直重试，NapCat 恢复后自己接上，不需要重启 sbot。
- `no_provider`：主模型用不了——不在模型清单里，或 `auth.json` 里没有它的 key。`doctor`
  的 `auth:` / `models:` 行会指出缺哪个。改好后**重启**。
- `starting`：刚连上，正在建 agent，几秒后再看；一直停在这里就看日志。

## 不回复

按这个顺序排除，每一步都有明确的判据：

1. `agent.state` 是 `running` 吗？不是，按上一节处理。
2. `estop`、`ai_enabled`、`hold`：三个里有一个不对就是原因。
3. 这条消息本来就不该唤醒它吗？默认只有 @ 本账号（含 @全体）的消息和主人发来的消息触发
   回合，其余只进收件箱。这是设计，不是故障。
4. 被 `[filter]` 丢了吗？看 `agent.filtered_messages` 有没有涨。
5. 预算用完了吗？日志里找 `skipping this turn`：每日 token / USD 用完（到本地零点恢复），
   或这个会话一小时内的回合数到了 `[budget].chat_turns_per_hour`。
6. 回合跑了吗？日志里找 `turn completed`。没有而前面几条都正常：看日志里的 provider
   报错（key 失效、限流、端点不通）。
7. 回合跑了但没发出来：`send_scopes` 不包含这个会话、`[gate]` 在等确认
   （`sbot gate pending`）、发送规则拦了（重复、频率），或者这个会话一小时内的发送数到了
   `[budget].chat_sends_per_hour`。
8. 都不是：它读了，决定不回。被唤醒不等于必须回复；要它多说话是人设问题
   （`sbot docs sbot-use persona-memory`）。

更多现象：`sbot docs sbot-operate troubleshooting`。

## hold

hold 是安全闸判定输入或输出危险之后的熔断：所有 QQ 动作（`napcat.*`）被拒绝，回合照常跑，
消息照收照存。它没有超时，也不会自己消失。

```bash
sbot security status      # hold、estop 与最近 20 条安全事件（判定原因在事件里）
```

正常的恢复路径**不需要人**：agent 下一次被唤醒时会看到自己处于 hold，隔离可疑内容
（`agent.quarantine`），再调用 `agent.resume` 让安全闸复核当前上下文，干净就解除。你在
主机上时可以手动触发同一次复核：

```bash
sbot security resume
```

hold 的原因也写在 `~/.sbot/SECURITY_HOLD` 文件里（只有原因，没有被判危险的原文）。

复核不通过时 hold 保持，返回的说明里写着原因，分两类：上下文里还有被判危险的内容——用
`sbot chat` 让 agent 隔离那段内容或清空上下文（`agent.clear_context`），再复核；或者
`classifier unavailable: …`——分类器本身用不了或超时，隔离解决不了，要修
`[security.guard]` 的配置或网络。

不要靠删 `SECURITY_HOLD` 文件来“解除”：进程在跑时删它没有用（状态还在内存里）；停机时
删掉等于跳过了唯一的一次复核，而被判危险的那段内容还留在上下文里。

反复进入 hold 的两个常见原因：`[security.guard]` 开着 enforce 但分类器没配好（启动日志与
`doctor` 会提示）；某个群里确实有人在持续注入。前者改配置，后者用 `[filter]` 或
`receive_scopes` 把来源挡在外面。完整模型：`sbot docs sbot-operate security`。

## 起不来，或者反复重启

前台跑一次 `sbot start`，错误直接打在终端上：

| 现象 | 原因与处理 |
|------|-----------|
| `another sbot instance is already running` | 同一个 `SBOT_HOME` 已经有进程；`sbot stop`，或者这本来就是该用另一个 `SBOT_HOME` 的实例 |
| `another instance is already listening on …` / `cannot create pipe …` | 另一个实例用了同一个 `[ipc].name`；给其中一个改名（`sbot docs sbot-deploy` 的“再开一个实例”） |
| 配置解析错误，带字段路径 | 严格 schema：不认识的键、类型不对、`version` 不是 1 都拒绝启动；对照 `sbot docs sbot-deploy configuration` |
| `key file … has an unknown format` / 读不了 | 主密钥文件坏了，或运行用户读不了它；**不要删了重新生成**，从备份取回 |
| `encryption canary does not match; refusing to open the database` | 数据库不是用这把 `key` 加密的。典型原因：搬家或恢复时没带 `key`，启动时自动生成了一把新的。用备份里的 `key` 覆盖这个新文件 |
| 进程启动后过一阵退出，退出码非 0 | 内部错误（panic）：sbot 的策略是立刻整个退出，由服务管理器拉起；日志最后几行是原因。反复出现请带日志报告问题 |

进程被杀或崩溃时正在跑的回合不会重新执行；它已经产生的内容在下次启动时补进对话记录
（日志 `recovered interrupted turns from the turn buffer`），上下文不丢。结果不明的发送记在
`unconfirmed_sends` 里，不会自动重发。

## 备份与恢复

```bash
sbot backup --output /backup/sbot-2026-11-16
```

可以在守护进程运行时执行。`--output` 必须是空目录或还不存在的目录，并且不能在数据目录
（`[storage].path`）或子代理定义目录（`[swarm].agents_dir`）里面——不满足就报错，什么都
不写。每次用一个新目录；不带 `--output` 时写到 `~/.sbot/backups/<时间戳>/`。

备份里有**主密钥**和**明文的 `auth.json`**：拿到这个目录的人能解开全部历史、能用你的
API key。按机密保管，不要放进同步盘、代码仓库或任何对话里。

备份目录是扁平的，和 `~/.sbot` 的布局**不一样**，恢复时要按下表放回去：

| 备份里 | 放回到 |
|--------|--------|
| `sbot.toml`、`models.json`、`auth.json`、`key` | `~/.sbot/` 下同名 |
| `data.db` | `~/.sbot/data/data.db` |
| `messages/`、`sessions/`、`blobs/` | `~/.sbot/data/` 下同名 |
| `agents/` | `~/.sbot/agents/` |

（上面是默认位置；改过 `[storage].path` 或 `[swarm].agents_dir` 的按配置里的位置。）

恢复步骤：停掉守护进程 → 把现有的 `~/.sbot` 整个改名留着（先别删）→ 建新的 `~/.sbot`
并按表放回 → `chmod 600 ~/.sbot/key ~/.sbot/auth.json` → `sbot doctor` 应当没有 `ERROR`
（其中 `database: canary matches the master key` 说明密钥和数据库是一对）→ 启动。

数据库迁移只往前走：旧版本的备份可以用新版本的 sbot 启动（启动时自动迁移），反过来不保证。

## 磁盘与配额

- `status` 的 `vfs.active_bytes` 是 agent 文件的活跃用量，上限是 `[storage].max_vfs_bytes`；
  写满时 agent 的写入报 `VFS quota exceeded (… of … bytes)`。`vfs.trash_bytes` 是回收站，
  不计入配额。让 agent 清理（`sbot chat`），或调大上限后重启。
- 宿主机磁盘余量低于 1 GiB 时 VFS 拒绝写入（`not enough free disk space …`），先腾空间。
- `messages` 与 `sessions` 在 `~/.sbot/data/` 下，增长速度取决于群的活跃度，备份的体积主要
  也是它们。超过 `[storage].max_message_bytes` / `max_session_bytes` 时自动删最旧的：消息
  按整月删、不删当月，对话记录删最旧的已关闭分段。

## 改动 `~/.sbot` 之前

- 先 `sbot backup --output <新目录>`。
- `key` 没有第二份：丢了或换了，全部历史解不开，没有任何恢复手段。
- `data/` 下的文件不要手工编辑或删除；它们和 `data.db` 里的索引是一对。
- `ESTOP` 可以放心用（`sbot pause` / `sbot resume`）：要做有风险的操作又不想停进程时，
  先 `sbot pause` 让它不再开始新回合。

## 报告问题时带上

`sbot --version`、`sbot status` 与 `sbot doctor` 的输出、出错时间段的日志、复现步骤。
发出去之前检查一遍里面没有 key、token 或 QQ 聊天原文。
