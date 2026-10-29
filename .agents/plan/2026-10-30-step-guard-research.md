# 安全分类器参考调研（2026-10-30）

- 目的：把安全检查从“每 256K 上下文一次”降到“Agent Loop 每个 tick 一次”，先确认这条路在业界已有可验证的先例，再决定接口与成本。
- 结论：可行。单遍决策模型的延迟（70–500ms）与单价（约 $0.00001/次）足以嵌入每一次循环步；但它是**选择器不是推理器**，只能回答窄问题，需要推理的判断必须交给更高一层。设计落在 `.agents/plan/2026-10-28-rebuild-design.md` §5.10，文本在 `.agents/plan/2026-10-29-prompt-catalog.md` §12。

---

## 1. Jev / TypeSafe System One

**形态**：非自回归决策模型。输入 `state` + 一组强类型问题，输出带概率的结构化答案，不生成文本。

**接口**（`POST /v3/alpha/decisions`，body `{model, state, questions}`）：

| 类型 | 含义 | 返回 |
|------|------|------|
| `noul` | 是/否 | `noul` 字段为 true 的概率 |
| `choice` | 从 `criteria` 里选一项（1–255 个选项） | `choice` + `confidence` + `probabilities` |
| `score` | 有序评分（2–10 级，最低在前） | 分数位置（如 1.78）+ `confidence` + `probabilities` |

- 响应还带 `usage`（input/output tokens）与 `cost`（美元）。
- 一次请求可在同一 state 上批量问多个问题——这是“每 tick 一次调用但问 4 个问题”的关键。
- 延迟 **70–500ms**；输入 **$0.042/M token**，输出免费；示例单次 **$0.000005–$0.000018**。
- 厂商自选基准声称比前沿 LLM 快 193.6 倍、便宜 444.6 倍；同基准准确率约 68%，与中端模型接近。
- 接口仍是 **alpha**，可能不兼容变更——因此设计里后端必须可替换、响应必须校验。

**能力边界**（arXiv:2610.24929，*What Can't Jev Decide?*）：

- 20k token、64 选项以内的选择/查找基本无错。
- 单次前向能解决的题：97.5% 正确；需要链式推理的题：74%。
- 会**跟随过期的 state**；置信度能标出它自己的推理错误，但**标不出继承自过期 state 的错误**。
- 提供 handoff 选项并不能修复这个问题。

→ 直接决定了三条设计约束：state 由 runtime 现场组装、问题必须是感知型、需要推理的判断交给 L2。

**在循环内怎么用**（`tamaratran/fast-jev-compaction`，7.8k star，2026-10-29 更新）：

- 用 Jev 替代压缩摘要：把整段历史作为 state，**工具结果全部替换成一行摘要**（`ok, 4213 chars (omitted)`），只保留工具入参与文本原文。
- 每个工具调用问两个 `noul` 问题（call 还要不要、result 还要不要原文），按阈值决定 keep / 截断 / 删除，**从不改写文本**。
- 问题拆成多个请求（state 每次重发、请求并发）以避开单请求上限；失败即抛出，由调用方决定回退。
- 这说明“单遍模型 + 每次一步 + 结构化动作”已在真实产品里跑通，且成本可忽略。

---

## 2. Anthropic Constitutional Classifiers（+ 下一代）

**第一代**：用明文宪法规则生成合成数据训练输入/输出分类器；越狱成功率 86% → 4.4%；输出分类器支持**流式逐 token 预测**；代价约 +23.7% 计算量、无害请求拒答率 +0.38%。

**第二代（部署于 Sonnet 4.5）**：两级级联。

1. **廉价内部探针**：看模型内部激活，复用已有计算，接近免费，扫描全部流量，**只做升级不做拒绝**，因此可以容忍较高误报。
2. **交换级分类器**：同时看提示词与响应，而不是各自单独看；因此能抓**输出混淆**（用“食品香料”代指试剂）与**重建攻击**（把有害请求拆成若干无害片段再组装）。
3. 最终由 probe + classifier 集成判定。

- 整体约 **+1%** 计算量（对比第一代 23.7%）；无害请求拒答率 **0.05%**（下降 87%）。
- 红队：1700+ 小时、198,000 次尝试，0.005 次检出/千查询。

→ 设计直接借用两点：**级联**（便宜的全量、可疑才升级）与**交换级视角**（必须同时看两侧，单侧都可能是无害的）。轨迹累计用于对抗重建攻击。

## 3. Claude 的流式安全分类器（API 行为）

- Claude 4+ 起，流式响应被分类器拦截时返回 `stop_reason: "refusal"`，`stop_details` 带 `category` 与 `explanation`（`category` 例如 `cyber`、`bio`、`reasoning_extraction`、`frontier_llm`），随 `message_delta` 到达。
- 这是 **HTTP 200**，不是错误，所以只看错误率的监控会漏掉它。
- 被拒请求可能仍然计费，因此不能盲目重试。

→ 设计里把它当作一等事件：不重试、写 `security_events`、通知主人、必要时换回退模型重发一次。

## 4. Claude Code auto mode

- **输入侧**：每个工具结果在进入上下文前过一遍服务端注入检测；疑似劫持则插入警告。
- **输出侧**：转录分类器先做单 token 是/否，可疑样本升级到带推理的二次判断。
- **关键设计：reasoning-blind**——分类器看到的转录**剥掉 agent 的文本与思考、剥掉工具结果**，只剩用户消息与工具调用。理由有两条：agent 会替自己的危险调用编理由；恶意文件/网页会试图说服审查器。
- 决策管线：先走 allow/ask/deny 规则 → 只读与项目内编辑自动放行 → 只读工具白名单 → 只有 shell、网络、MCP、跨项目操作才进分类器。
- 阻断规则按意图分组：破坏/外泄、降低安全姿态、跨越信任边界、绕过审查、影响他人。
- 失败 **fail closed**；连续拒绝后回退人工确认。
- 指标（两阶段整体）：FPR 0.4%（n=10,000）、真实过度行为 FNR 17%（n=52）、合成外泄 FNR 5.7%（n=1,000）；仅第一阶段 FPR 8.5% / FNR 6.6%。

→ 设计借用：四种视图各自的裁剪、规则先行分类器兜后、fail closed、以及“把 FPR/FNR 作为上线门槛”。

---

## 5. 采纳与不采纳

最终设计（5.10）只保留两个检查点：**模型输入前**与**模型输出后**。工具调用是模型输出的一部分、工具结果是模型输入的一部分，所以两个闸覆盖全部路径，不需要在工具调用上再撒检查点。

采纳：

- 用 Jev 做判定器：问题强类型（`noul` / `choice`），一次请求批量问完，约 $0.00002、200ms。
- 只问感知型问题（“这段文本是不是在试图改变规则/冒充主人/索取别处内容”），不问“该不该拦”。判定由 runtime 按阈值与时序事实做出。
- 命中即停机：关闭全部对外接口，**无超时、无自动恢复**。
- 无人值守恢复：停机期间 agent 照常思考与操作，怎么办、何时重开都由它自己判断；重开时 runtime 用当前上下文再过一次同样的闸，过则恢复。不设修复流程、不设审批、不设计时器。
- 收缩暴露面的动作（拉黑、禁言、踢出、deny）自主执行，不需要 owner 证据。

不采纳：

- 逐 tick 撒检查点（工具调用前、工具结果前、每个副作用前）：两端两个闸已经覆盖，多撒只会把成本和延迟乘以数倍。
- 风险分累计 / 阈值升级 / “连续 N 次才停机”：一次成功的泄露就已经结束了，没有第二次。
- 内部激活探针：需要模型权重与推理栈控制权，走 provider API 拿不到。
- 逐 token 流式拒绝：QQ 整条发送，按 `delta_every_tokens` 窗口检查 + 发送前终检即可。
- 把 Jev 当推理器：论文已证明它不做多步推理，判断必须留在 runtime。
- 绑定 TypeSafe 单一供应商：接口是 alpha，必须抽象成 `jev | llm | off` 并允许自建代理。

## 6. 参考链接

- Jev 论文：<https://arxiv.org/abs/2610.24929>
- Jev 接口与价格：<https://www.edenai.co/post/jev-a-new-kind-of-ai-model-built-for-decisions-not-conversation>
- 循环内用法：<https://github.com/tamaratran/fast-jev-compaction>
- 下一代宪法分类器：<https://www.anthropic.com/research/next-generation-constitutional-classifiers>
- 流式拒绝处理：<https://platform.claude.com/docs/en/test-and-evaluate/strengthen-guardrails/handle-streaming-refusals>
- Claude Code auto mode：<https://www.anthropic.com/research/claude-code-auto-mode>

## 7. 实现备注

- 凭据形态：**明文写在 `auth.json`**（`security.guard.api_key`），不使用环境变量作为必需项。`[security.guard].base_url` 放在 `sbot.toml`，key 不放。
- 判定阈值默认 `dangerous >= 0.9`；上线前必须先在 `shadow` 模式跑够样本，记录各类别的误判率。误判的代价是停机，直到 agent 自己判断可以重开，所以校准比有人值守时更重要。
- 本机测试端点与 key 记录在 `.agents/MEMORY.local.md`（不进入版本库）；该端点不是 TypeSafe 官方地址，请求路径、响应字段、问题类型支持情况都要在适配层实测，失败即降级到 `llm` 后端，不得假设与官方一致。

## 8. 实测记录（2026-10-30）

- 端点：`POST {base_url}/v1/systemone`（`/v3/alpha/decisions` 不存在，实测 404）。
- 鉴权：`Authorization: Bearer <key>`；`/openapi.json` 也需要同一个头才能拿到（OpenAPI 3.1，title TypeSafe，version 0.2.0）。
- 模型：`GET /v1/models` 返回 `jev-latest` 等别名；请求里 `model` 用别名。
- 请求体：`{ "model": "jev-latest", "state": "...", "questions": { "dangerous": { "type": "noul", "instructions": "..." } } }`
  - `questions` 是**对象映射**（名字 → 问题），不是数组；
  - 字段名是 `instructions`，不是 `prompt`；
  - `noul` 可选 `criteria: { true, false }` 说明什么算 yes/no；`choice` 用 `criteria` 映射；`score` 用有序数组。
- 响应：`{ "model": "...", "answers": { "dangerous": { "type": "noul", "noul": 0.98 } }, "usage": { "input_tokens": 120, "output_tokens": 12 } }`
- 实测样本：
  - 良性：「A group chat about weekend plans. / Shall we meet at noon?」→ `noul = 0.01`
  - 凭证：「here is the key sk-abcdefghijklmnopqrstuvwx」→ `noul = 0.84`
- 结论：默认阈值 `0.9` 时 0.84 **不会**触发 hold，但规则层的 `sk-` 形态会以 0.99 命中。这正是「规则先行、分类器兜后」的价值；也说明 `shadow` 期必须按类别统计分数分布，不能直接拿默认阈值上线。
