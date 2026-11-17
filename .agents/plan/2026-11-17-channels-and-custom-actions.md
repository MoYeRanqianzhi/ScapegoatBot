# 命名、自定义 action 与任意渠道：设计稿

> 状态：**用户 2026-11-17 确认，A – E 全做**（AskUserQuestion 的回答是“按设计稿全做”：采纳
> 文末六条建议，但第 1 条的范围改为连 E 一起做，不等第二个渠道）。进度记在
> [`../TODO.md`](../TODO.md) 的“进行中”。写于 2026-11-17。
>
> 用户同一天追加的要求：**自定义 action / 渠道的开发与接入都要有 skill**。设想中的使用方式是
> 用户让一个 agent 登录服务器替他部署 sbot、替他写适配器，几乎没有人手工做；所以每一步落地
> 时都要同步写进 `skills/`（步骤 C 另加一个讲适配器协议、怎么写、怎么配、怎么排错的 skill，
> 带一个能直接改的示例），写完用不带上下文的子代理按 skill 实际做一遍来验证。
> 需求原话与方向记在 [`memory/tool-surface.md`](../memory/tool-surface.md)；TODO 里的条目是
> “命名与可扩展性”。文末“要用户定的事”逐条给了建议，用户可以只回编号。

## 用户要什么

2026-11-13 的三句话：

1. 本机动作的前缀 `local.*` 改成 `native.*`。
2. 前缀**只是命名空间**，方便区分，不承载任何强制约定。
3. `API` 要可扩展：用户能自己加 action；能接任意即时通讯渠道并映射成 `API` 的 action；
   NapCat 可以禁用。

## 现状（2026-11-17 对照代码）

**策略挂在前缀上的地方**——第 2 句话要求它们全部挪走：

| 位置 | 现在怎么判断 | 实际想表达的 |
|------|--------------|--------------|
| `src/actions.rs` `ActionSpec::hold_allowed` | `namespace() != "napcat"` | hold 期间拒绝对外渠道的动作 |
| `src/actions.rs` 注册表生成处 | `starts_with("napcat.get_")` → 只读查询；`starts_with("napcat.upload_")` → 要 owner intent | 这个动作是读还是写、要不要 owner intent |
| `src/security/action_gate.rs`、`src/tools/api/outbound.rs` | `namespace() != "napcat"` 就跳过 | 门禁与发送前改写只管对外渠道的动作 |
| `src/tools/api/policy.rs` | `strip_prefix("napcat.")` 得到发给 NapCat 的动作名 | 这个动作在渠道那一侧叫什么 |
| `src/tools/api.rs` 的分派 | `match spec.namespace()`、`in_namespace("local.media")` / `("local.net")` | 谁来执行这个动作 |
| `src/tools/api.rs` 子代理限制 | `"napcat" if self.local_only`、`"agent" if ctx.subagent.is_some()` | 子代理不能碰对外渠道，也不能动主代理的状态 |

`ActionSpec` 已经有一半是属性：`category`、`requires_owner_intent`、`scope_field`、
`target_field`、`destructive`。缺的是“谁执行”和“碰不碰外部”。

`src/tools/api.rs` 分派处的注释还写着“本地工具只读写 VFS”——这正是用户纠正过的误读
（`local.net.download` 就要联网），这次一起改掉。

**写死 QQ / NapCat 的地方**——第 3 句话涉及的范围：

- 会话标识 `ChatId::{Group(u64), Private(u64)}`（24 个源文件在用）；文本形式 `group:<群号>` /
  `private:<QQ 号>` 出现在 `receive_scopes` / `send_scopes`、monitor 的 `event.chat`、
  `/messages/groups/<群号>/…` 这样的 VFS 路径、消息索引的主键 `(chat, message_id)` 里。
- 身份：`owner_qq` / `master_qq` 是 `u64`，启动时用登录账号推出来（`resolve_principals`）。
- 入站：`QqEvent`、`qq::normalize`、发送账本（认领 `message_sent` 回声）、消息段格式。
- 出站：`NapcatGateway` 只有一个方法 `call(action, params)`，本身已经是一层很薄的抽象。
- 启动：要等 NapCat 连上并登录才建 agent（`agent.state = waiting_for_napcat`，这期间
  `sbot chat` 返回 `not_ready`）。
- 配置：`[napcat].url`；monitor 的 `source = { qq = true }`。

## 设计

### 1. 属性代替前缀（不改变任何行为）

给 `ActionSpec` 补两个字段，把上表的判断全部换成读属性：

```rust
/// 谁来执行。名字不参与判断：名字只是名字。
enum Executor {
    /// 交给一个渠道；`wire` 是它在渠道那一侧的动作名。
    Channel { channel: &'static str, wire: &'static str },
    /// 本进程内置的实现（图片处理、下载、api.help、agent.resume …）。
    Builtin(Builtin),
    /// 用户接进来的外部进程（见第 3 节）。
    Adapter { adapter: String, wire: String },
}

/// 这个动作会不会离开本机、让别人看见。
enum Reach {
    /// 只读本机或 agent 自己的状态。
    Internal,
    /// 从外部读（查消息、查群成员、下载）。
    ExternalRead,
    /// 对外产生后果（发消息、群管理、上传、改资料）。
    ExternalWrite,
}
```

由属性推出策略，规则写在一处（`policy.rs`）：

- hold 拒绝：`Executor::Channel` 的全部动作（与现在“只拒绝 `napcat.*`”完全等价）。
- 门禁、发送规则、发送前改写：`Executor::Channel`。
- 子代理：`Reach::Internal` 与本机的 `ExternalRead` 可用；`Executor::Channel` 按
  `allow_send` 决定；动主代理状态的内置动作单独一个标记，子代理恒不可用。
- `api.list` / `api.help` 把 `reach`、`destructive`、`requires_owner_intent` 直接给 agent 看
  （顺带解决 TODO 里“`api.help` 不暴露 destructive”那一条）。

这一步是纯重构：现有测试一个都不该改，另加一条测试断言“注册表里每个动作的属性与改之前按
前缀推出来的结果一致”，证明等价之后再删掉旧的前缀判断。

### 2. 改名：`local.*` → `native.*`

属性化之后改名只是改字符串：注册表、system prompt、工具描述、skill、测试、CHANGELOG 的
破坏性变更。agent 自己写在 VFS 里的技能与笔记如果提到 `local.*`，改名后会调用失败——未知
动作的报错里列出 `native.*` 下的名字即可，不保留别名。

### 3. 用户自己加 action：适配器进程

一种机制同时满足“自定义 action”与“自定义渠道”：**适配器**——用户配置的一个外部进程，
用 stdio 上的 JSON 行与 sbot 说话（和 MCP 服务器同一种部署方式，所以用户已经会配）。

```toml
[[adapters]]
name = "home"                 # 也是它的命名空间：动作叫 home.<动作名>
command = "python"
args = ["/opt/sbot-adapters/home.py"]
env = { }
```

协议只有三种消息：

- `describe` → 适配器返回它提供的动作：名字、摘要、参数 schema，以及**它声明的属性**
  （`reach`、`destructive`、`requires_owner_intent`、作用域字段）。
- `call { action, params }` → 结果或错误。
- `event { … }`（可选，适配器主动发）→ 入站事件。只提供动作的适配器不发它。

安全上的两条：

- 适配器声明的属性**只能被配置收紧，不能被放宽**；没声明的按最严处理
  （`ExternalWrite`、要 owner intent）。写得马虎的适配器不会因此多出权限。
- 适配器由主人在宿主机上配置，和 MCP 服务器是同一个信任级别。**agent 自己不能加适配器**：
  `[[adapters]]` 不进 `/config` 的可写白名单。理由：没有 Bash 是这个 agent 的安全前提之一，
  让它自己定义“执行某个命令 / 请求某个地址”的动作，等于绕过 WebFetch 的网络策略开一条
  外传通道。

不做的：声明式的“HTTP 动作”（URL 模板 + 参数映射）。它看起来省事，但模板、鉴权、重试、
返回值裁剪会长成另一套小语言；一个二十行的适配器脚本能做同样的事。

### 4. 渠道 = 会发事件、有身份的适配器

```rust
trait Channel: Send + Sync {
    /// 命名空间，也是会话标识里的渠道名。
    fn name(&self) -> &str;
    /// 连接并取得登录身份；失败由调用方重试。
    async fn identity(&self) -> Result<Identity>;
    /// 它提供的动作（带属性）。
    fn actions(&self) -> Vec<ActionSpec>;
    async fn call(&self, wire: &str, params: Value) -> Result<Value>;
    /// 归一化之后的入站事件。
    fn events(&self) -> EventStream;
}
```

NapCat 是内置的一个 `Channel` 实现（现在的 `src/qq/` 整体搬到它后面：归一化、消息段格式、
发送账本都是它的内部细节）。外部渠道是“声明了 `channel = true` 的适配器”，由一个通用的
`AdapterChannel` 包起来。

会话标识要带上渠道：

- 内部表示改成 `ChatId { channel, kind, id: String }`。
- 文本形式：NapCat 的保持 `group:<群号>` / `private:<QQ 号>` 不变（现有配置、monitor、
  VFS 路径、消息索引都不用迁移）；其它渠道写 `<渠道名>/group:<id>`、`<渠道名>/private:<id>`。
- 主人身份按渠道配置：每个渠道各有自己的 owner / master 账号；owner intent、主人保护、
  `wake-on-owner` 都按“这条消息所在渠道的 owner”判断。

这一步改动面最大（`ChatId` 从 `Copy` 的两个整数变成带字符串的结构，24 个文件），也是唯一
需要认真做数据兼容的一步。

### 5. NapCat 可以禁用

`[napcat].enabled = false`（默认 `true`）。没有任何渠道时：

- 不等登录，直接建 agent；`agent.state` 不再有 `waiting_for_napcat`。
- 可用的入口是控制通道（`sbot chat`）与 monitor 的 `timer` / `stream` / `internal` 源。
- `stand_in` / `companion` 两种身份都以“有一个账号”为前提：没有渠道时用一段中性的身份
  （“你是主人的助手，目前没有接入任何聊天渠道”），`owner_qq` / `master` 不参与校验。
- 渠道动作不出现在 `api.list` 里；system prompt 里讲 QQ 发送的那几段要变成渠道提供的片段，
  而不是写死在静态前缀里——否则禁用 NapCat 之后提示词还在教它发 QQ 消息。

有多个渠道时，`agent.state` 只看“必需的渠道”是否就绪；每个渠道各自的连接状态放进
`sbot status`。

## 分步与每步的边界

| 步 | 内容 | 破坏性 | 依赖 |
|----|------|--------|------|
| A | 属性代替前缀（第 1 节） | 无 | — |
| B | `local.*` → `native.*`（第 2 节） | 有：动作名 | A |
| C | 适配器提供动作（第 3 节，不含事件） | 无（新增配置） | A |
| D | NapCat 可禁用（第 5 节） | 无（默认不变） | A |
| E | 渠道抽象、带渠道的会话标识、适配器渠道（第 4 节） | 有：内部数据模型；对外保持 QQ 的写法 | A、C、D |

A – D 各自都能独立发版、独立验证。E 值得单独再写一份设计（事件字段、消息段在不同渠道间
怎么表示、媒体、撤回这类不是每个渠道都有的能力怎么声明），等 A – D 落地、并且有一个真的要接
的第二渠道时再做——没有具体的第二个渠道，抽象多半会抽错。

## 要用户定的事

1. **范围与顺序**。建议按上表 A → B → C → D 做，E 等有具体要接的渠道时再设计。
   另一种选择是现在就把 E 一起做完（一次性的大改动，约等于重写 `src/qq/` 与存储的会话键）。
2. **自定义 action 的机制**。建议：适配器进程（第 3 节）。备选：声明式 HTTP 动作；或者把
   MCP 服务器的工具映射进 `API` 的命名空间（现在它们在单独的 `MCP` 工具里）。
3. **agent 能不能自己加 action**。建议：不能，只有主人在宿主机上能配（理由见第 3 节）。
4. **改名要不要保留旧名**。建议：不保留，`local.*` 直接失效，报错里给出新名字。项目还在
   alpha，别名会一直留在提示词与测试里。
5. **hold 对自定义动作的范围**。现在的决定是“hold 只拒绝 QQ 动作”。建议推广为：渠道的动作
   （内置的与适配器渠道的）一律拒绝；只提供动作、不是渠道的适配器不受 hold 影响（与 MCP、
   Web 现在的待遇一致），除非它在配置里标了 `blocked_in_hold = true`。
6. **非 QQ 渠道的会话写法**。建议：`<渠道名>/group:<id>`，QQ 保持现在的短写法。只有做 E 时
   才需要定。

## 验证方式（做的时候）

- A：新增“属性与旧前缀规则等价”的测试；全部现有测试不改而通过。
- B：`tests/source_hygiene.rs` 加一条——源码、提示词、skill 里不再出现 `local.` 开头的动作名。
- C：仓库里放一个二十行的示例适配器（`examples/`），集成测试起它、`describe`、`call`，并断言
  没声明属性的动作按最严处理、配置放宽属性会被拒绝。
- D：集成测试在 `[napcat].enabled = false` 下启动，`sbot chat` 能跑完一个回合，`api.list`
  里没有渠道动作，静态提示词里没有 QQ 发送的段落。
