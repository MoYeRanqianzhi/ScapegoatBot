---
name: sbot
description: Entry point for working with ScapegoatBot (the `sbot` binary), a QQ agent that replies on its owner's behalf through NapCat. Use this first whenever a task mentions sbot, ScapegoatBot, a QQ bot or agent on NapCat / OneBot 11, `~/.sbot`, `SBOT_HOME` or `sbot.toml` — it shows how to find out what state an installation is in and which sbot skill (deploy, use, operate) covers the task at hand.
---

# sbot：从哪里开始

ScapegoatBot 是一个跑在本机的 QQ agent：它经 NapCat（OneBot 11 WebSocket）连上一个 QQ
账号，用你配置的大模型，代替主人（或作为独立账号）在群里和私聊里回复。可执行文件叫
`sbot`，全部状态在 `~/.sbot`（环境变量 `SBOT_HOME` 可以换位置）。

它是无人值守的：启动之后主人只通过 QQ 与它交互，不会有人守在主机上。你做的每一步部署和
运维都要按这个前提来——凡是“出了问题得有人上机器敲命令才能恢复”的状态，都等于停机。

## 这些文档跟着二进制走

你现在读的内容内嵌在 `sbot` 里，和这个二进制是同一个版本：

```bash
sbot docs                             # 列出全部 skill 与参考文件
sbot docs sbot-deploy                 # 打印一个 skill
sbot docs sbot-deploy configuration   # 打印它的一份参考文件
sbot docs --export <目录>             # 把整套 skill 写进目录（装进你自己的 skills 目录）
```

`sbot docs` 不需要守护进程，也不需要 `~/.sbot`。行为与文档对不上时，先确认你读的是
`sbot --version` 这个版本的文档，而不是别处抄来的旧说明；升级二进制之后重新导出一次。

## 先看现状

动手之前花十秒弄清楚你面对的是什么：

```bash
sbot --version       # 二进制在不在、哪个版本
sbot status          # 守护进程在不在；在的话输出一份 JSON
sbot doctor          # 只读自检：配置、凭据、密钥、数据库；守护进程没跑也能用
```

- `sbot` 找不到 → 还没安装，读 `sbot-deploy`。
- `sbot status` 连不上（`Is the daemon running?`）→ 守护进程没在跑，或者你用的
  `SBOT_HOME` / `--config` 不是它用的那一份。没配过就读 `sbot-deploy`；配过但起不来读
  `sbot-operate`。
- `sbot status` 有输出 → 已经在跑。先看 `agent.state`：

  | `agent.state` | 含义 |
  |---------------|------|
  | `running` | agent 在工作 |
  | `waiting_for_napcat` | 守护进程在，但还没连上 NapCat，或 NapCat 里没有登录 QQ；它会一直重试 |
  | `starting` | 刚连上，正在建 agent；过几秒再看 |
  | `no_provider` | 主模型用不了（清单里没有，或缺凭据）；改好配置后重启 |

  再看 `napcat.connected`（此刻是否连着）、`agent.self_id`（登录的 QQ）、`hold`、`estop`、
  `ai_enabled`。每个字段的含义在 `sbot-operate`。

## 该读哪一个

| 任务 | skill |
|------|-------|
| 安装、接 NapCat、填凭据、第一次启动、交给服务管理器、升级、再开一个实例、换机器 | `sbot-deploy` |
| 日常命令、给它下指令、审批、换模型、调人设与记忆、让它订阅事件 | `sbot-use` |
| 不回复、hold、起不来、备份与恢复、磁盘与配额、它做不到什么 | `sbot-operate` |

配置项的完整说明在 `sbot docs sbot-deploy configuration`；安全模型在
`sbot docs sbot-operate security`。

## 三个概念，贯穿所有文档

- **回合（turn）**：一次模型对话。QQ 消息默认只进收件箱，不逐条触发回合；只有被 monitor
  命中的（默认是 @ 本账号的消息、以及主人发来的消息）才唤醒 agent。
- **VFS**：agent 眼里的文件系统，存在加密的数据库里，不是宿主机的目录。`/AGENTS.md`、
  `/USER.md`、`/MEMORY.md` 是它自己的文件；`/messages`、`/inbox`、`/config`、`/system`
  是运行时生成的视图。你在宿主机上看不到这些路径——要读写它们，用 `sbot chat` 让
  agent 自己做。
- **hold**：安全闸判定危险后的熔断：所有 QQ 动作被拒绝，回合照常跑。它没有超时，由 agent
  自己复核上下文后解除。

## 两条不要做的事

- 不要为了“修好”去删 `~/.sbot/key`、`data/` 下的文件或 `SECURITY_HOLD`。`key` 没了，
  全部历史就再也解不开；其余的先 `sbot doctor`、读 `sbot-operate`。动之前
  `sbot backup --output <目录>`。
- 不要把 `auth.json` 的内容、`key`、备份目录贴进任何对话或提交。它们是明文凭据和能解开
  全部历史的主密钥。
