# 内嵌文档与使用者 skill（2026-11-11）

> 状态：进行中。来源：用户 2026-11-10 的两条要求（见 `memory/docs-layout.md`）。
> 完成后把落地的路径与命令并入 `memory/docs-layout.md`，删除本文件。

## 要解决的问题

- 发布包里只有二进制（`.github/workflows/release.yml` 只打包 `sbot` 与 sha256），没有任何
  文档。部署这个二进制的 agent 拿不到使用说明；仓库里的 `docs/guide/` 又可能与它手里的
  二进制版本不一致。
- `docs/development/` 是给“开发者”的，而开发者就是 agent：这类内容属于 `.agents/`。

## 方案

**一份源文件，三个出口。** 源文件是仓库里的 `skills/`，按标准 skill 目录组织：

```
skills/
  sbot/                      入口：是什么、怎么判断现状、该读哪个 skill
    SKILL.md
  sbot-deploy/               从零到跑起来：安装、NapCat、凭据、首次启动、验证、升级、多实例
    SKILL.md
    references/configuration.md      sbot.toml / models.json / auth.json 全量
  sbot-use/                  日常使用：命令、审批、三个开关、人设与记忆、让它主动做事
    SKILL.md
    references/persona-memory.md
    references/tools-and-vfs.md
  sbot-operate/              运维：状态怎么读、hold、备份恢复、排障、限制
    SKILL.md
    references/security.md
    references/troubleshooting.md
    references/limits.md
```

出口：

1. **二进制内嵌**（`include_str!`）：`sbot docs` 列出全部 skill；`sbot docs <name>` 打印
   `SKILL.md`；`sbot docs <name> <reference>` 打印参考文件；`sbot docs --export <dir>` 把
   整套 skill 写到目录里（agent 把它装进自己的 skills 目录）。不需要守护进程，也不需要
   `~/.sbot`。
2. **仓库里的 `skills/`**：给克隆仓库的人或 agent 直接读、直接装。
3. **README**：只留简介与指路（`sbot docs`），不再抄一遍内容。

`docs/guide/` 的内容搬进 `skills/*/references/`（它们已经是使用文档，改写成“给 agent 的
操作说明”的口吻），`docs/guide/` 删除。`docs/development/` 删除：

| 文件 | 去向 |
|------|------|
| `README.md`（模块地图、结构约定、维护约定） | 模块地图不留（代码与模块注释就是地图，抄一份会漂移）；仍成立的维护约定并入 `.agents/memory/maintenance-rules.md` |
| `prompt-maintenance.md` | 并入 `.agents/memory/prompt-style.md`（或拆一条 `prompt-structure.md`）：三层结构、两套身份、新增文本的归类规则、预算、相关测试 |
| `live-testing.md` | 可复现的命令并入 `.agents/memory/live-testing.md`；alpha.12 的一次性结果不留 |
| `refactor-report.md` | 删除（alpha.12 的历史快照；数据来源 `plan/2026-10-30-refactor-baseline.md` 一并删） |

## 约束

- 内嵌文档必须与 `skills/` 逐字相同：测试枚举 `skills/` 下的每个文件，断言都被内嵌且内容
  一致；反过来内嵌清单里的每一项在磁盘上都存在。新增文件忘了登记会让测试失败。
- skill 里写到的命令、配置键、默认值要能被测试锁住的尽量锁住（现有
  `tests/config_example.rs` 读 README 与入门指南里的最小配置，改读 skill）。
- skill 的文字是给 agent 的：讲清为什么、给可照抄的命令；不写“见仓库某文件”这种离开
  二进制就失效的指路（参考文件用 `sbot docs <name> <reference>` 指）。
- 模型面文本（system prompt、工具描述）仍是英文；skill 是使用文档，中文（与原指南一致）。
  frontmatter 的 `description` 用英文：它是别的 agent 判断要不要加载的依据。

## 步骤

- [ ] 写 `skills/`（四个 skill + references，内容来自 `docs/guide/`，按现状逐条核对代码）。
- [ ] `src/docs.rs`：内嵌清单、查找、导出；`sbot docs` 子命令。
- [ ] 测试：内嵌与磁盘一致；`sbot docs` 的列出 / 打印 / 导出；最小配置仍然有效。
- [ ] 删除 `docs/guide/`、`docs/development/`；README、`config.example.toml`、代码注释里的
      指路改掉；`.agents` 里引用这两个目录的记忆改掉。
- [ ] 开发侧内容并入 `.agents/memory/`。
- [ ] 上级目录 `../Claude.md` 写着“开发文档位于 docs/development/”，不在本仓库里：告诉用户。
- [ ] 用 skill-creator 的方式验证：给没有上下文的 agent 一个部署 / 排障任务，看只凭
      `sbot docs` 能不能做对。
