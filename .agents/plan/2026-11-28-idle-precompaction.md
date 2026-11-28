# 空闲预压缩（设计与进度）

状态：设计已定（主代理 2026-11-28），交给 Codex 实现；进度与实现中的决定记在文末。
来源：用户 2026-11-17 的决定（见 `TODO.md`“进行中”）：保留尾巴不变；到软阈值时在回合之外
提前算好摘要，回合边界采用；硬阈值保留同步压缩兜底；采用时“记快照长度、把之后追加的消息
原样接回”。

## 现状（2026-11-28 的代码）

- `maybe_auto_compact`（`src/agent/runner/compaction.rs`）在回合开头、回合锁之内调用：估算
  超过 `compaction_budget()`（硬阈值）就调 `compact_scope("auto")`，同步做完摘要再继续回合。
- `compact_scope` 拿 `compaction_lock`，取历史快照，`split_for_compaction` 切成 old / keep，
  安全复核（`check_on_compaction`）、PreCompact hook、记忆提取、摘要，然后
  `[摘要] + keep + post_compact_context()` 经 `sessions.rotate_with` 写进新分段，再整体替换内存历史。
  它靠回合锁保证快照到替换之间没人追加。
- 手动 `agent.compact`（`compact_now("manual")`）是回合里的一次工具调用，也走 `compact_scope`。
  `API` 工具是顺序执行的（`src/tools/api.rs` 的 `execution_mode`；一批调用里有顺序执行的就整批
  顺序执行，`src/agent/loop.rs` 约 713 行），所以回合内没有别的东西与它并发改历史。

## 设计

### 配置

`[compaction].precompact_percent`：整数，默认 `80`，`0` = 关闭，允许 `0..=99`（`>= 100` 时软阈值
不低于硬阈值，没有意义，校验报错）。软阈值 = `max(硬阈值 × percent / 100, keep_recent_tokens + 1)`，
硬阈值即现在的 `compaction_budget()`（随热切换的模型窗口变化，所以软阈值每次现算）。
`RunnerConfig` 增加同名字段。

### 一次压缩拆成“准备”和“安装”两步

- 准备：给定历史快照，切分、安全复核、PreCompact hook、记忆提取、摘要，得到
  `PreparedCompaction { snapshot, cut, session_file, summary, reason }`（`keep = snapshot[cut..]`，
  `session_file` 是快照时的当前分段）。`old` 为空时没有可准备的。只读共享状态，不改历史。
- 安装：持有历史锁，先校验历史仍以 `snapshot` 开头（逐条相等）且当前分段仍是 `session_file`；
  不成立就作废（返回“未安装”，历史不动）。成立则
  `新历史 = [摘要] + keep + 后缀 + post_compact_context()`，后缀 = 历史里 `snapshot.len()` 之后
  追加的消息，原样接回；keep 与后缀里所有 assistant 消息的 `usage` 清零（它们描述的是压缩前的
  提示长度，留着会让估算继续超限）。`rotate_with` 写新分段，替换内存历史，然后照旧清零失败数、
  重建 system、触发 PostCompact。
- 摘要消息的格式不变：`[checkpoint summary | previous_session=/sessions/<file> | reason=<r>]`，
  后台准备的 `reason` 是 `idle`。

校验用逐条相等而不是计数器：清空上下文、别的压缩、`agent.quarantine` 改写了快照范围内的消息，
都会让前缀不等，一种判断覆盖所有情况。

### 后台准备（回合结束时启动）

普通回合与 injected 回合提交之后（仍在回合锁内）检查，全部满足才启动：

1. `precompact_percent > 0`；
2. 模型请求没有被停（`sbot ai stop` / ESTOP，与 `new_turns_paused` 用同一个判断）；
3. 没有待执行的 `discard_context` 标记（被判 critical 的上下文不拿去摘要）；
4. 自动压缩没有因连续失败停掉；
5. 没有已经准备好、还没采用的结果；
6. 历史估算超过软阈值；
7. 当日预算没有耗尽；
8. `compaction_lock` 能立刻拿到（拿到后把锁交给后台任务，整个准备期间持有：
   “有后台准备在跑” ⇔ 这把锁被占着）。

后台任务对启动时的快照做“准备”，成功则存起来等待采用；失败计入 `compaction_failures`
（与同步压缩同一个计数：摘要请求连续失败说明同步那条路也会失败），历史不动。
后台任务运行时不持有回合锁，新回合照常进行。

**后台准备不做安全复核**（`check_on_compaction` 只作用于同步压缩）。复核要放在“采用”这一步，
在回合锁之内：

- 后台复核的结论到得晚：判为 critical 时置的 `discard_context`，可能已经被下一回合开头的检查
  错过；那一回合只追加消息，快照前缀与分段都还对得上，硬阈值的等待会把这份摘要装进去，
  正是“被判 critical 的上下文不拿去摘要”要防的事（Codex 2026-11-28 核对源码时指出）。
- `run_security_review` 结束时把 `unchecked_tokens` 清零；后台复核期间回合提交累加上去的、
  快照里没有的那部分会被一起清掉，周期复核就少看了这些新内容（同上）。

所以采用时（回合锁之内、安装之前），`check_on_compaction` 打开就先对**安装时的完整历史**
（快照 + 后缀）复核一次，与同步压缩复核的范围相同；复核之后 `discard_context` 被置位，就
丢弃准备好的结果、不安装，交给下一回合开头的丢弃逻辑。复核和记账因此都只在回合锁里发生，
与现在相同。代价：打开 `check_on_compaction` 时，采用那一刻仍要等一次复核请求（复核比摘要
快得多，且默认关闭）。

### 回合边界采用（`maybe_auto_compact`）

1. 有准备好的结果、且此刻没有压缩在跑：安装它（不论是否到了硬阈值；安装前的复核见上一节）。
   作废或复核后要丢弃上下文时，丢弃结果。
2. 估算不超过硬阈值：结束。
3. 超过硬阈值：等正在跑的后台准备结束（拿一次 `compaction_lock` 再放掉），有结果就安装；
   安装后不超过硬阈值就结束。等它比重新同步摘要快：它开始得更早。
4. 仍然超过：照旧检查失败次数、同步压缩。

### 同步压缩与手动压缩

`compact_scope` 改为“拿锁 → 丢弃已准备的结果（马上要替换历史，它必然作废）→ 准备 → 安装”。
同步路径在回合锁与压缩锁之内，校验按现在的并发模型不会失败；万一失败就报错、不替换历史
（不静默覆盖）。
`clear_all_context` 同时丢弃已准备的结果。

## 不变量（测试要守住的）

- 采用后，快照之后追加的消息一条不少、顺序不变地出现在新历史与新分段里。
- 快照范围内的历史被改过（清空、隔离、别的压缩）时，准备好的结果不会被安装。
- 后台失败不改历史、不改分段，计入失败数；连续失败停掉后不再启动后台准备。
- 超过硬阈值且后台正在准备时，只发一次摘要请求。
- `precompact_percent = 0` 时行为与以前完全相同。
- 后台任务里不调用 `run_security_review`；`check_on_compaction` 打开时，采用前对完整历史复核，
  复核判 critical 时不安装。

## 进度与决定

- 2026-11-28 主代理：设计如上，交给 Codex 实现（分支 `codex/idle-precompaction`）。
- 2026-11-28 Codex：从 `c45814d` 创建分支。核对源码后在修改实现前停止：后台准备的
  第 3 项只在启动前看 `discard_context`。`run_security_review` 遇到 `Critical` 会置位后
  正常返回（`src/agent/runner/review.rs:39`、`:87`）；现有压缩路径随后不再看标记就请求摘要
  （`src/agent/runner/compaction.rs:155`–`:168`），与不摘要已判 critical 的上下文冲突。
  若下一回合已经在 `run_turn` 开头检查过标记（`src/agent/runner.rs:567`），置位后未必有
  清理发生；只追加新消息时，快照前缀和当前分段仍相同，硬阈值等待后仍可能安装该结果。
- 2026-11-28 Codex：同一次源码核对还发现，`review_lock` 只串行复核，不串行记账。
  回合提交后可在后台复核等待期间累加快照之后的 token（`src/agent/runner.rs:729`–`:735`）；
  复核返回便把 `unchecked_tokens` 清零（`src/agent/runner/review.rs:46`–`:51`），会抹掉未审
  新消息的累计量。两处均为源码时序推演，未运行测试。需先明确后台复核的丢弃与记账语义。
- 2026-11-28 主代理：采纳上面两条发现，改设计——后台不复核，复核挪到采用时（回合锁内、对完整
  历史）；见“后台准备”一节的说明。
- 2026-11-28 Codex：按修订复核同步路径，critical 时不调用摘要；准备阶段不复核。
  分段名随快照取得，避免异步准备期间轮转后记录了错误的来源；准备阶段在 hook 和记忆提取后
  才处理分段读取错误，保留原副作用顺序。安装在同一历史锁内校验前缀和分段，并把后缀接回。
- 2026-11-28 Codex：拆分后的 Windows 验收通过：格式与 clippy 退出 0，测试 609 通过、
  2 忽略；`fn_len.py 100` 只列出原有 `build_agent`。源码核对：安全节的默认
  `check_on_compaction` 实为 `true`（`src/config/sections/security.rs:106`），空的
  `review_model` 走主模型（`src/app/assembly/providers.rs:166`）；不改这两个默认值，
  所以默认仍会在采用时等安全复核，但不在正常采用路径上等待摘要。
- 2026-11-28 Codex：拆分提交 `8e1a889`。后台接管压缩锁，先发布结果或记下失败再释放；
  前一次准备可能在估算或异步日预算检查期间结束，所以取得锁后复查结果槽和失败上限，
  不覆盖刚发布的结果。采用时若复核置位丢弃标记，在任一阈值路径上都结束，不再同步
  复核或摘要同一上下文。清空会丢弃已就绪的结果；尚在准备的结果即使稍后发布，安装
  校验也会拒绝其旧分段。
- 2026-11-28 Codex：`test --locked --lib compaction` 18 个通过。用 Notify 和首个轮询信号
  固定顺序：低于硬阈值立即返回；超过后先进入等待，再放出摘要响应。临时移除后缀接回、
  前缀校验、清空时丢弃已准备摘要、硬阈值等待，分别命中对应的四条断言（14 通过、4 失败，
  Cargo 退出 101）；随后恢复实现。
- 2026-11-28 Codex：恢复实现后的 Windows 验收：fmt、fmt check、clippy 均为 0；全量
  618 通过、0 失败、2 个在线 provider 测试忽略；`fn_len.py 100` 仍只列出 `build_agent`。
  复核测试覆盖后台不复核、不清计数，采用窗口含后缀，critical 时采用与同步都不安装。
