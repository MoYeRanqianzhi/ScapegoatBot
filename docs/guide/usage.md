# 日常使用

## 命令一览

所有命令都可以加 `--config <PATH>` 指定另一份 `sbot.toml`。

| 命令 | 作用 |
|------|------|
| `sbot start` | 前台运行守护进程（Ctrl-C 停止） |
| `sbot stop` | 停止守护进程 |
| `sbot restart` | 停止后重新拉起 |
| `sbot status` | 运行状态、模型、hold、ESTOP、用量 |
| `sbot ai start\|stop\|restart` | 只控制 LLM 调用；消息接收与存储不中断 |
| `sbot chat "<text>" [--scope group:<gid>]` | 通过控制通道给 agent 一句话 |
| `sbot pause` / `resume` | 写/删 `ESTOP`，暂停或恢复新回合 |
| `sbot security status` | hold 状态与最近安全事件 |
| `sbot security resume` | 用当前上下文重新过闸，尝试解除 hold |
| `sbot gate pending` | 列出 `approve` 模式待确认的动作 |
| `sbot gate approve <id>` / `reject <id>` | 批准（120 秒内可重试一次）/ 拒绝 |
| `sbot model set <provider/id>` / `list` | 热切换主模型，下一回合生效 |
| `sbot effort set <level>` / `list` | 热切换 effort |
| `sbot backup [--output DIR]` | 在线一致性备份（含主密钥） |
| `sbot doctor` | 只读诊断，不修复数据 |

`start`、`backup`、`doctor` 直接操作本地文件，不需要守护进程在跑；其余命令通过
Windows 命名管道 / Unix domain socket 通信。

## 状态里看什么

```text
model:   anthropic/claude-opus-5-5
hold:    false
estop:   false
usage:   input 12.3k / output 2.1k
```

- `hold: true`：安全闸停机。先看 `sbot security status` 的事件原因。
- `gate`：门禁模式与待确认请求数；`unconfirmed_sends` 是可能已发出的发送数。
- `estop: true`：有人执行了 `sbot pause`，或 `~/.sbot/ESTOP` 存在。
- 没有 agent 运行时，状态会显示 provider 不可用；先看日志里的 provider 报错。

## 审批模式

`[gate].mode = "approve"` 时，发送、群管理、改资料、上传文件等变更动作不会直接执行，
工具会返回：

```text
approval_required: napcat.send_group_msg is pending as g3. ...
```

主人确认（二选一）：

- 机器人会私聊 master 一个 6 位 nonce；主人在该私聊里直接回复 nonce 即批准；
- 或用控制通道：

```bash
sbot gate pending          # 看请求、参数与等待时间
sbot gate approve g3       # 批准；120 秒内让 agent 重试该动作
sbot gate reject g3        # 拒绝
```

请求 60 秒未确认自动失效；批准是一次性的。群聊或他人回复不构成确认。

## 三个开关的区别

| 开关 | 影响 | 恢复 |
|------|------|------|
| `sbot ai stop` | 不发起新的模型请求；消息照收照存 | `sbot ai start` |
| `sbot pause`（ESTOP） | 不开始新回合；消息照收照存 | `sbot resume` |
| hold | 拒绝所有 QQ 动作与对外工具 | 过闸后 `sbot security resume` 或 `agent.resume` |
| `agent_paused` | 安全审查 `critical` 后连新回合也暂停 | 同上；控制通道 `chat` 仍可用 |

`ai stop` 与 ESTOP 都只挡“新回合”，不会中断已经在跑的回合。

## 消息进来之后

1. NapCat 事件到达，`[filter]` 先做准入判断（拒收即丢弃）；
2. 消息写入加密 JSONL，索引进 SQLite，收件箱默认未读；
3. `[queue]` 按 chat 去抖合并；
4. 回合开始：入站消息进 transcript，压缩超限时先压缩，然后请求模型；
5. 输出闸通过后执行工具、发送消息；发送成功记为 `self`；
6. 回合结束提交 transcript，标会话已读。

## 让它主动做事

- `sbot chat "..."`：给 agent 一句 owner 指令，立刻跑一个回合。
- `[[monitor]]` 或 `Monitor` 工具：订阅 QQ 事件/定时器，把事件投递成上下文。
- `Agent` 工具：派子代理做搜索、整理、检查等独立任务。

## 模型热切换

```bash
sbot model list
sbot model set deepseek/deepseek-chat
sbot effort set high
```

- 新模型必须在 `models.json` 里，且在 `[agent].available_models` 白名单内。
- 正在跑的回合继续用旧快照；下一回合生效。
- provider 协议、base_url、key 仍按 `models.json` / `auth.json` 解析。

## 配置热更新

通过 `/config` 写入（agent 自己改）或直接编辑文件后重启：

- 立即生效：`[filter]`、`[queue]`（`max_concurrent` 除外）。
- 下次启动生效：`[compaction]` 的预算、`[skills]`、`[style]`、`[agent]` 白名单键等。
- 只读：凭据、安全策略、存储路径等。

## 备份与恢复

```bash
sbot backup --output /backup/sbot-2026-11-04
```

备份包含 `data.db` 快照、消息/会话/blob 文件与 **主密钥**。请按机密文件保管：
拿到备份就能解密全部历史。

恢复：停进程，把备份内容放回 `~/.sbot`（或新的 `SBOT_HOME`），启动即可。

## 日志

日志走 stderr（由进程管理器收集）。常见关键行：

- `connected to NapCat`：WebSocket 已通；
- `agent runtime ready`：provider 可用；
- `turn completed stop=EndTurn`：回合正常结束；
- `security gate blocked this turn`：闸拦截；
- `cannot store an inbound message`：存储错误，查磁盘与权限。
