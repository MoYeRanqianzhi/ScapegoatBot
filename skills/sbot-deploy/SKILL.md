---
name: sbot-deploy
description: Install and bring up ScapegoatBot (`sbot`) from nothing to a QQ agent that answers - pick the right release file for the platform and CPU, connect NapCat (OneBot 11 WebSocket), fill in credentials, first start, verify, hand it to a service manager with automatic restart, upgrade, run a second instance, move to another machine. Use whenever sbot is not installed yet, has never started successfully, or needs to be upgraded, duplicated or relocated - including when the request only says "set up the QQ bot" or "deploy ScapegoatBot".
---

# 部署 sbot

目标状态：`sbot status` 里 `agent.state` 是 `running`、`napcat.connected` 是 `true`，守护进程由
服务管理器看着、挂了会被自动拉起。下面按顺序做；每一步后面都写了怎么确认它成了。

配置项的完整说明不在这里：`sbot docs sbot-deploy configuration`；带注释的完整样例：
`sbot docs sbot-deploy sbot.example.toml`。

## 需要什么

| 依赖 | 说明 |
|------|------|
| 一个 `sbot` 可执行文件 | 下载预编译版本，或从源码构建（Rust 1.89+） |
| NapCatQQ | 已登录要用的 QQ 号，开着 OneBot 11 的 **WebSocket 服务器** |
| 一个大模型 API | Anthropic Messages、OpenAI Chat Completions 或 OpenAI Responses 兼容的端点与 key |

不需要 Node、Python 或数据库服务：全部状态在 `~/.sbot`（环境变量 `SBOT_HOME` 可以换位置），
SQLite 编在程序里。

## 1. 拿到二进制

发布页 <https://github.com/MoYeRanqianzhi/ScapegoatBot/releases> 每个版本有 20 个包，名字是
`sbot-<版本>-<平台>.tar.gz`（Windows 是 `.zip`），解开是一个同名目录，里面有 `sbot` 本体。

| 系统 | 平台 | 说明 |
|------|------|------|
| Linux，glibc ≥ 2.28（Debian 10+、Ubuntu 20.04+、RHEL / Rocky / Alma 8+、Amazon Linux 2023、Fedora、Arch、openSUSE 等） | `x86_64-unknown-linux-gnu`、`aarch64-unknown-linux-gnu` | 首选 |
| 任何 Linux（Alpine、glibc 更旧的系统、不确定的环境） | `x86_64-unknown-linux-musl`、`aarch64-unknown-linux-musl` | 全静态，不依赖任何系统库 |
| Windows 10 / 11 | `x86_64-pc-windows-msvc`、`aarch64-pc-windows-msvc` | 不需要装 VC++ 运行库 |
| macOS 11+ | `aarch64-apple-darwin`（Apple 芯片）、`x86_64-apple-darwin`（Intel） | |

x86_64 的每种平台按 CPU 等级各出一份，等级写在架构后面（例如
`x86_64_v3-unknown-linux-gnu`）。等级越高，编译器能用的指令越新、程序越快：

- 不带后缀（v1）：任何 x86-64 CPU；
- `_v2`：2009 年以后的 CPU（SSE4.2、POPCNT）；
- `_v3`：Intel Haswell（2013）及以后、AMD Zen 及以后（AVX2、BMI2、FMA）；
- `_v4`：支持 AVX-512 的 CPU。

**不确定就先下不带后缀的**，运行 `sbot --version`：`cpu:` 一行写着这台机器能用的最高等级，
再换成对应的包。选高了的后果只有一个：程序一启动就以“非法指令”（Illegal instruction）
退出，不会损坏任何东西。

校验下载的文件（第二条确认它确实是这个仓库的发布流程从对应 tag 构建出来的）：

```bash
sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify sbot-<版本>-<平台>.tar.gz --repo MoYeRanqianzhi/ScapegoatBot
```

从源码构建：`git clone` 仓库后 `cargo build --release`，产物是 `target/release/sbot`。

把 `sbot` 放到 `PATH` 里（下文都直接写 `sbot`）。确认：`sbot --version` 打印版本、平台与
`cpu:` 三行。

## 2. NapCat

1. 按 NapCat 自己的文档登录 QQ。
2. 在 NapCat 的 OneBot 11 网络配置里加一个 **WebSocket 服务器**（正向 WS，配置项
   `network.websocketServers`）。sbot 只会作为客户端去连它；NapCat 的“WebSocket 客户端”
   （反向 WS）连不上 sbot。
   - 地址例如 `ws://127.0.0.1:3001`，之后填进 `sbot.toml` 的 `[napcat].url`；
   - `messagePostFormat` 保持默认的 `array`；
   - 打开 `reportSelfMessage`。替身模式（`stand_in`，agent 用的就是主人的号）下，主人在
     手机上手打的消息只有这样才能被 agent 看到；agent 自己发的消息会被发送账本认出来，
     不会被当成主人的话；
   - 设了 `token` 的话，之后填进 `auth.json` 的 `napcat.access_token`。

sbot 不修改 NapCat 的配置。

## 3. 第一次启动与配置

```bash
sbot start
```

`start` 是前台进程，`Ctrl-C` 停止。第一次启动会建好 `~/.sbot/`：写一份默认的 `sbot.toml`
（已经有就不动）、生成主密钥 `key`（权限 0600）、建数据库并跑迁移、往 agent 的虚拟文件系统
里写入初始文件。然后它会一直等 NapCat。

另开一个终端，写两份文件。`~/.sbot/sbot.toml` 最小可用的内容：

```toml
version = 1
[agent]
model = "anthropic/claude-opus-5-5"
available_models = ["anthropic/claude-opus-5-5"]

[napcat]
url = "ws://127.0.0.1:3001"
```

`~/.sbot/auth.json`（凭据只放在这里，`sbot.toml` 里没有任何 key）：

```json
{
  "providers": { "anthropic": { "api_key": "sk-ant-..." } }
}
```

- 模型名是 `provider/id`，必须同时出现在 `available_models` 里。内置的模型清单编在
  二进制里；要加 provider 或模型时再建 `~/.sbot/models.json`，它与内置清单合并。写法见
  `sbot docs sbot-deploy configuration`。
- 配置是严格 schema：写了不认识的键，启动直接报错并指出是哪个键。
- 两种模式：`stand_in`（默认，agent 用主人自己的 QQ 号）与 `companion`（agent 用独立的
  号，这时必须填 `[agent].owner_qq`）。模式启动时固定。

**改了 `sbot.toml` 或 `auth.json` 要重启才生效**（`Ctrl-C` 后再 `sbot start`）。

## 4. 确认它跑起来了

```bash
sbot status
```

输出是一份 JSON。看这几个字段：

| 字段 | 期望 | 不对的时候 |
|------|------|-----------|
| `agent.state` | `running` | `waiting_for_napcat`：NapCat 没开、QQ 没登录或地址 / token 不对，它会一直重试，日志每次写一条 `waiting for NapCat`；`no_provider`：模型不在清单里或缺 key，改好后重启 |
| `napcat.connected` | `true` | 同上 |
| `agent.self_id` | 登录的 QQ 号 | `null` 说明 agent 还没建起来 |
| `hold`、`estop` | `false` | 见 `sbot docs sbot-operate` |

然后让模型真的跑一个回合，先走控制通道（不需要 QQ 消息）：

```bash
sbot chat "Reply with exactly: live-ok"
```

命令立刻返回 `{"accepted": true, ...}`——这只表示指令被接受，回合在后台跑，模型的输出
不会打印在终端上。看守护进程的日志：出现 `control turn finished stop=EndTurn` 说明模型
调用、凭据与落盘都通了；出现 `control turn failed` 时后面跟着原因（多半是 key 或地址不对）。

再走一次真实的 QQ 路径：在一个它在的群里，用别的号 @ 它。默认只有两种消息会唤醒它——
@ 本账号（含 @全体）的消息，以及主人发来的消息；**别人发来的私聊只进收件箱，不会触发
回复**，拿它来验证会以为没部署好。日志里出现 `turn completed` 就是整条链路通了。

`sbot doctor` 可以随时跑：只读地检查配置、凭据、密钥与数据库，守护进程没在跑也能用。

## 5. 交给服务管理器，并且要配自动重启

sbot 不自带后台运行或服务安装：`start` 就是一个前台进程，日志写到 stderr。长期运行要交给
systemd、launchd、Windows 服务包装器之类的服务管理器，而且**必须配自动重启**。

原因：程序内部出错（panic）时，sbot 的做法是整个进程立刻退出，而不是带着一个坏掉的后台
任务继续跑。它无人值守，没有人会注意到“进程还在但已经不干活了”；退出再被拉起来反而是
能自己恢复的状态——被打断的回合已经产生的内容在启动时补回对话记录，上下文不丢。没有自动
重启，一次内部错误就是停机。

systemd 的例子（`/etc/systemd/system/sbot.service`）：

```ini
[Unit]
Description=ScapegoatBot
After=network-online.target
Wants=network-online.target

[Service]
User=sbot
Environment=SBOT_HOME=/var/lib/sbot
ExecStart=/usr/local/bin/sbot start
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

- `Restart=on-failure` 正好：`sbot stop` 让进程以 0 退出，不会被重新拉起；崩溃是非 0，会。
  不要用 `Restart=always`：它在 `sbot stop` 之后也把进程拉起来，停不下来。
- 日志用 `journalctl -u sbot` 看。
- 由服务管理器运行时，**重启用服务管理器的命令**（`systemctl restart sbot`），不要用
  `sbot restart`：后者自己拉起一个脱离终端的新进程，不归服务管理器管，日志也没有地方去。
  `sbot restart` 只适合手工在终端里跑的场合。
- 从登录 shell 里用 `sbot status` 等命令时，要让它找到同一个实例：同一个用户、同一个
  `SBOT_HOME`（上例里是 `sudo -u sbot SBOT_HOME=/var/lib/sbot sbot status`）。

Windows 上用任务计划程序（“失败时重新启动”）或 NSSM / WinSW 之类的包装器，原则相同：
前台运行 `sbot start`，退出码非 0 时重启。

## 接到真实的群之前

默认配置下 agent 能读、能发所有会话。先在私聊或小号群里试，再收紧：

- `[agent].send_scopes`：允许发消息的会话列表，空列表表示一条都不许发；
- `[agent].receive_scopes`：允许读的会话列表；
- `[filter]`：直接丢弃某些群、某些人或匹配某些模式的消息；
- `[gate].mode = "approve"`：发送、群管理等动作先等主人确认（见 `sbot docs sbot-use`）。

安全模型与各项默认值的含义：`sbot docs sbot-operate security`。

## 目录布局

```text
~/.sbot/
  sbot.toml           主配置（不含任何凭据）
  models.json         自定义的模型清单（可以没有）
  auth.json           凭据，明文，只在本机
  key                 主密钥（0600）；丢了，全部历史再也解不开
  ESTOP               存在即暂停新回合（sbot pause 写、sbot resume 删）
  SECURITY_HOLD       存在即处于 hold
  data/
    data.db           SQLite 索引
    messages/         加密的消息文件
    sessions/         加密的对话记录分段
    blobs/            加密的二进制分块
  run/                单实例锁、PID、控制通道的套接字（Unix）
  backups/            sbot backup 的默认输出位置
```

## 升级

```bash
sbot backup --output <一个新目录>     # 先备份：包含主密钥与 auth.json，按机密保管
# 换掉 sbot 可执行文件
systemctl restart sbot                # 或者 sbot stop 之后再 sbot start
sbot status                           # version 是新版本，agent.state 回到 running
sbot docs --export <你的 skills 目录>  # 文档跟着二进制走：导出过的话重新导出一次
```

新版本启动时自动跑数据库迁移。迁移只往前走：升级之后不保证还能换回旧版本，所以升级前的
那份备份要留着。每个版本的破坏性变更写在发布页的说明里，升级前看一眼。

## 再开一个实例

每个实例要有自己的目录和自己的控制通道名字：

```bash
SBOT_HOME=/srv/sbot-b sbot start      # 第一次启动生成 /srv/sbot-b/sbot.toml
```

然后在 `/srv/sbot-b/sbot.toml` 里加：

```toml
[ipc]
name = "sbot-b"
```

控制通道的名字默认是 `scapegoat-bot`，不随 `SBOT_HOME` 变：Windows 上它是全局的命名管道，
Unix 上设了 `XDG_RUNTIME_DIR` 时套接字也放在同一个目录里。两个实例用同一个名字，后启动的
那个起不来（Unix 报 `another instance is already listening on …`，Windows 报
`cannot create pipe …. Another sbot instance may already be running`）。之后对第二个实例的每条命令都带上
同一个 `SBOT_HOME`；带错了，守护进程会拒绝并说明它服务的是哪个目录。同一个 `SBOT_HOME`
同时只能跑一个进程。

## 换机器

停掉进程，把整个 `~/.sbot` 复制到新机器的同一位置（或新的 `SBOT_HOME`），启动。必须连
`key` 一起带走：只有 `data.db` 或 `messages/` 而没有 `key`，内容解不开。NapCat 的登录态
不在 `~/.sbot` 里，新机器上要重新登录。
