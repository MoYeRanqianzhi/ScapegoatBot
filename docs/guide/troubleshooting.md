# 故障排查

先跑这两个命令，大多数问题能直接定位：

```bash
sbot status          # 进程在不在、模型、hold、ESTOP
sbot doctor          # 只读检查配置、密钥、索引、记忆三方对账
```

`doctor` 不修数据；它报错的地方按提示修。

## 守护进程起不来

| 现象 | 检查 |
|------|------|
| `another sbot instance is already running` | 同一 `SBOT_HOME` 已有进程；先 `sbot stop`，或换 `SBOT_HOME` |
| 配置解析错误 | 严格 schema：未知字段会报 `unknown field`；错误里带字段路径 |
| `version` 错误 | `sbot.toml` 必须 `version = 1` |
| 权限错误 | 检查 `~/.sbot` 与 `~/.sbot/key` 的属主/权限（key 应为 0600） |

配置校验失败时进程不会启动；把报错里的字段路径对照
[configuration.md](configuration.md) 修。

## NapCat 连不上

日志关键字：

```text
cannot connect to NapCat (ws://...)
NapCat connection timed out
```

- NapCat 是否启动、QQ 是否登录；
- OneBot 11 WebSocket 服务是否开启，地址是否与 `[napcat].url` 一致；
- 有 access token 时是否写进 `~/.sbot/auth.json`；
- 防火墙/端口占用。

没连上时守护进程照常运行，但 QQ 工具会返回 `NapCat is not connected`。

## 不回复

按顺序看：

1. `sbot status`：`estop` 或 `hold` 是否 true？
2. 日志有没有 `no usable provider` / `missing API keys`：模型 key 与
   `models.json` 的 provider 名是否对上；
3. 模型是否在 `[agent].available_models` 白名单里；
4. 消息是否被 `[filter]` 拒收（日志 `message rejected by the admission filter`）；
5. 群是否在 `send_scopes` 里（发送范围只对发送生效，不影响读）；
6. `sbot ai stop` 是否被误执行。

## hold

```bash
sbot security status
```

事件里会写判定原因（规则命中还是分类器）。确认内容无害后：

```bash
sbot security resume
```

如果提示 `The gate still blocks this context`，说明复核仍判定危险。处理方式：
清理上下文里被标记的内容（`agent.quarantine`、修正文件），或确认分类器配置正确。

**enforce 且没配分类器**时每个回合都会被 hold：启动日志与 `doctor` 会提示。
要么配 `base_url` + key，要么显式 `backend = "off"`。

## ESTOP

`~/.sbot/ESTOP` 存在即暂停新回合：

```bash
sbot resume          # 删除 ESTOP
```

`ai stop` 与 ESTOP 都不影响消息接收与存储。

## 历史读不出来

- 换过机器但没带 `~/.sbot/key`：内容无法解密，只能从备份恢复。
- 曾经把 `[storage].encryption` 从 `none` 改成加密（或反向）：新实现按文件头解密，
  旧文件不受影响；如果文件是更早版本写的、没有 `encryption` 头，才会退回当前配置。
- 索引行号错位：启动时会比较 JSONL 文件数与索引数，不一致自动重建；`doctor`
  会报告数量差异。

## 回合卡住 / 很慢

- 单回合步数上限 `[agent].max_steps`；
- provider 重试与回退链会拉长耗时：看日志里的重试与 fallback；
- 上下文过大触发压缩：压缩本身要一次模型请求；
- 主 agent 回合是串行的：前一个回合未结束时，新的唤醒会等待它完成。

## 磁盘与配额

日志出现 `QuotaExceeded` / `DiskFull`：

- 看 `/system` 的 VFS 用量；
- 清 `/trash` 或调大 `[storage].max_vfs_bytes`；
- 磁盘余量低于 1 GiB 时拒绝写入，先腾空间。

## MCP

- `MCP is enabled but no server exposed any tool`：检查 `command` / `args` 能否在
  当前环境直接运行；
- 服务器启动失败只告警，不影响主进程；用 `sbot doctor` 与日志确认；
- 结果被截断会显式标注。

## IPC 命令失败

`status` / `chat` 等通过命名管道 / UDS 通信：

- 守护进程没跑：先 `sbot start`；
- 自定义了 `[ipc].name`：CLI 会读同一份配置，确保命令与进程用的是同一份；
- Windows 下同名管道被旧进程占用：`sbot stop` 后重试。

## 提交问题前

请附上：

- `sbot status` 与 `sbot doctor` 输出（删掉 key）；
- 出错时间段的日志；
- 操作系统、Rust 版本、`sbot --version`；
- 复现步骤（配置片段可脱敏）。
