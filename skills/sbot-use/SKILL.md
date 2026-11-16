---
name: sbot-use
description: Day-to-day use of a running ScapegoatBot (`sbot`) - the full command list, giving the agent an instruction with `sbot chat`, approving or rejecting pending actions in approve mode, the three switches (`ai stop`, `pause`, hold) and how they differ, switching model or effort without a restart, what the agent may change in its own config, shaping its persona and memory, and making it act on QQ events or timers. Use whenever sbot is already installed and running and the task is to operate it normally, change how it behaves, or understand why it did or did not reply to a message.
---

# 使用 sbot

前提：守护进程在跑，`sbot status` 里 `agent.state` 是 `running`。不是的话先读
`sbot docs sbot-deploy`（没装好）或 `sbot docs sbot-operate`（装好了但不对）。

参考文件：

- `sbot docs sbot-use persona-memory`：人设、`/AGENTS.md`、`/USER.md`、记忆与技能怎么写；
- `sbot docs sbot-use tools-and-vfs`：agent 看到的文件系统、动态视图与全部内置工具。

## 命令一览

所有命令都可以加 `--config <PATH>` 指定另一份 `sbot.toml`。

| 命令 | 作用 |
|------|------|
| `sbot start` | 前台运行守护进程（`Ctrl-C` 停止） |
| `sbot stop` | 停止守护进程（进程以 0 退出） |
| `sbot restart` | 停止后自己拉起一个脱离终端的新进程；由服务管理器托管时不要用，见 `sbot docs sbot-deploy` |
| `sbot status` | 运行状态，一份 JSON |
| `sbot ai start` / `stop` | 只控制模型调用；消息接收与存储不中断 |
| `sbot chat "<text>" [--scope group:<gid>\|private:<qq>]` | 经控制通道给 agent 一句主人的指令，在后台开始一个回合；`--scope` 告诉它这句话针对哪个会话 |
| `sbot pause` / `resume` | 写 / 删 `ESTOP`，暂停或恢复新回合 |
| `sbot security status` | hold 状态与最近的安全事件 |
| `sbot security resume` | 让 agent 用当前上下文重新过闸，尝试解除 hold |
| `sbot gate pending` | 列出 `approve` 模式下等待确认的动作 |
| `sbot gate approve <id>` / `reject <id>` | 批准 / 拒绝一个待确认的动作 |
| `sbot model set <provider/id>` / `list` | 热切换主模型 / 列出白名单里的模型 |
| `sbot effort set <level>` / `list` | 热切换 effort / 列出当前模型支持的级别与当前值 |
| `sbot backup [--output DIR]` | 在线一致性备份（含主密钥与 `auth.json`），见 `sbot docs sbot-operate` |
| `sbot doctor` | 只读诊断，不修复任何数据 |
| `sbot docs [<skill> [<reference>]]` | 打印内嵌的这套文档；`--export <DIR>` 导出 |
| `sbot --version` | 版本、目标平台、本机 CPU 等级 |

`start`、`backup`、`doctor`、`docs` 直接在本地工作，不需要守护进程在跑；其余命令经控制通道
（Windows 命名管道 / Unix 套接字）发给守护进程。连不上时报 `Is the daemon running?`：要么
它没在跑，要么你用的 `SBOT_HOME` / `--config` 不是它用的那一份。

## 消息进来之后发生什么

理解这一段，才知道它“为什么没回”：

1. NapCat 的事件到达，`[filter]` 先做准入判断，被拒收的直接丢弃、不存盘，只在
   `sbot status` 的 `agent.filtered_messages` 里计一次数；
2. 消息加密写盘，进索引，在收件箱里记为未读；
3. **普通消息只留在收件箱，不逐条唤醒 agent。** 只有被 monitor 命中的事件才触发回合。
   默认有两个：`wake-on-mention`（@ 本账号或 @全体，且不是主人发的）与 `wake-on-owner`
   （主人发来的消息；没有配置主人时没有这一个）。它们是普通的 monitor，agent 自己可以改；
4. 回合开始：这段时间攒下的入站消息一起进上下文，上下文超限时先压缩，再请求模型；
5. agent 决定回不回、回什么。**被唤醒不等于一定回复**：它可以读完之后什么都不发；
6. 回合结束，对应会话标为已读。同一会话的连发由 `[queue]` 去抖与合并。

所以“没回复”的排查顺序是：消息被 `[filter]` 丢了吗 → 触发回合了吗（日志里有没有
`turn completed`）→ 回合里它想发但被拦了吗（`hold`、`[gate]`、`send_scopes`）→ 还是它
自己决定不回。细节在 `sbot docs sbot-operate troubleshooting`。

## 给它下指令

```bash
sbot chat "把 /MEMORY.md 里过期的条目清理一下"
sbot chat --scope group:123456 "这个群里今天在聊什么？总结给我"
```

`sbot chat` 的内容以主人的身份进入上下文，立刻开始一个回合。命令本身马上返回
`{"accepted": true, "control_id": …}`：这表示指令被接受了，回合在后台跑，**模型的文本输出
不会打印在终端上**。结果要看它做了什么（发出的 QQ 消息、改动的文件）和日志——
`control turn finished stop=EndTurn` 是正常结束，`control turn failed` 后面跟着原因。
跑不了的时候命令直接报错而不是排队：`not_ready`（agent 还没建起来）、`ai_stopped`
（先 `sbot ai start`）。

它是在宿主机上对 agent 说话的唯一方式——agent 的文件（`/AGENTS.md`、`/MEMORY.md` 等）在
加密的 VFS 里，宿主机上没有对应的路径，要读写它们就让 agent 自己做。

让它持续地主动做事，而不是只做一次：

- **monitor**：订阅 QQ 事件或定时器，命中时把事件投递成一个回合。写在 `sbot.toml` 的
  `[[monitor]]` 里（`sbot docs sbot-deploy configuration`），或者让 agent 自己用 `Monitor`
  工具建；
- **子代理**：agent 用 `Agent` 工具把搜索、整理、检查之类的独立任务派出去
  （`sbot docs sbot-use tools-and-vfs`）。

## 审批模式

`[gate].mode = "approve"` 时，发送、群管理、改资料、上传文件等有外部后果的动作不会直接
执行，agent 的工具调用会得到：

```text
approval_required: napcat.send_group_msg is pending as g1763366400000-512345678-3. ...
```

主人确认的途径取决于 `master_qq` 是哪个账号：

- **是另一个账号**（`companion` 模式，或 `stand_in` 下把 `[agent].master` 设成主人的小号）：
  机器人私聊那个账号一条确认通知，里面带 6 位确认码；主人从那个账号私聊回复确认码即批准。
- **就是机器人登录的账号**（`stand_in` 的默认 `master = "self"`）：私聊回复批准不了——
  本账号发出的消息与 agent 自己发的分不开，接受它们等于让 agent 自己批准自己。这时通知里
  不带确认码，只能走控制通道：

```bash
sbot gate pending          # 每个请求的 id、动作、摘要、参数与已等待的秒数
sbot gate approve <id>     # 批准；<id> 照抄 pending 里的 `id`
sbot gate reject <id>      # 拒绝
```

时限与边界：

- 请求 60 秒没有确认就自动失效；
- 批准之后 120 秒内，agent 用**完全相同的参数**重试这个动作一次才会放行；批准不是“以后
  都可以”；
- 群聊里的回复、别人的回复都不构成确认；确认码那条回复本身不进消息归档。

approve 模式下每个动作都要人在 60 秒内响应，它适合试运行或高风险场合，不适合无人值守的
日常运行。

## 三个开关的区别

| 开关 | 影响 | 恢复 |
|------|------|------|
| `sbot ai stop` | 不发起新的模型请求，`sbot chat` 也不行；消息照收照存 | `sbot ai start` |
| `sbot pause`（ESTOP） | 不开始新的 QQ / monitor 回合与子代理；消息照收照存；宿主机上的 `sbot chat` 仍然可用 | `sbot resume` |
| hold | 拒绝所有 QQ 动作（`napcat.*`）；回合照常运行 | agent 隔离可疑内容后自己调用 `agent.resume`；宿主机上的 `sbot security resume` 走同一条路径 |

- `ai stop` 与 ESTOP 都只挡“新回合”，不会打断已经在跑的回合。`ai stop` 只记在内存里，
  守护进程重启就恢复；ESTOP 是 `~/.sbot/ESTOP` 文件，重启仍然有效。
- ESTOP 下 `sbot chat` 仍然可用是有意的：暂停期间主人要能在宿主机上问它话、让它收拾。
- hold 不是人工开关，是安全闸判定危险后的熔断；它没有超时。怎么看原因、怎么恢复：
  `sbot docs sbot-operate`。

## 换模型与 effort

```bash
sbot model list
sbot model set deepseek/deepseek-chat
sbot effort list
sbot effort set high
```

- 新模型必须在 `[agent].available_models` 白名单里，并且在模型清单里有定义。`model list`
  里 `defined: false` 的那些在白名单里但清单里没有，切过去会失败。
- 正在跑的回合继续用旧模型，下一个回合生效。
- provider 的协议、地址与 key 仍然按 `models.json` / `auth.json` 解析；目标模型的 provider
  在 `auth.json` 里没有 key 时切换直接失败（`no API key for provider …`），当前模型不变。
  `auth.json` 只在启动时读，补了 key 要重启。
- 热切换同时写回 `sbot.toml`，重启后保持。

## 配置：谁能改什么，什么时候生效

**你直接编辑 `sbot.toml` / `auth.json` / `models.json`：重启后生效。**

agent 自己能经 `/config` 改的只有 `[filter]`、`[queue]`、`[compaction]`、`[skills]`、
`[style]`，以及 `[agent]` 的 `available_models` 与 `thinking_level`；其余的键对它只读——
它改不了自己的发送范围、门禁模式或主人是谁。

- 写入后立即生效：`[filter]`、`[queue]`（`max_concurrent` 除外，要重启）；
- 写入落盘、下次启动生效：`[compaction]`、`[skills]`、`[style]` 与 `[agent]` 的那两个键。

要让 agent 改配置，用 `sbot chat` 告诉它改什么；它写入时过的是和启动时同一套校验，写坏的
配置不会落盘。

## 调它的性格、记忆与技能

人设不在 `sbot.toml` 里，在 agent 的 VFS 里：`/AGENTS.md`（它是谁、怎么说话）、`/USER.md`
（主人是谁）、`/MEMORY.md` 与 `/memory/`（它记住的东西）、`/skills/`（它会的做法）。全部
由 agent 自己维护；主人的做法是在 QQ 里或用 `sbot chat` 告诉它“以后……”，让它写进去。
各文件的职责、格式与写法：`sbot docs sbot-use persona-memory`。
