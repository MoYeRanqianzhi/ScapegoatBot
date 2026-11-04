# TODO

按优先级排列；完成后同步 `CHANGELOG.md` 与 `docs/guide/limits.md`。
已完成项直接删除（git 有记录）。

## 安全与设计对齐

- [ ] `auto` 门禁的内容级规则：跨 chat 5-gram 重叠、媒体 `origin_chat` 校验、
      `context_sources` 追踪（设计 §5.6 / §5.8）。当前只有频率/重复/跨群/@all 启发式。
- [ ] 周期检查复用主请求的逐字节前缀缓存（设计 §5.9）：当前是独立窗口。

## 配置与能力边界

- [ ] `[hooks.events]` 的 rewrite 只在 BeforeSend；SessionEnd/Stop/ConfigChange
      的 inject_context 没有落点。

## 运行与数据

- [ ] WebFetch 改为本地抓取（或对后端做逐跳重定向协议），消除“先取内容后复核”。
- [ ] `Agent message` 发给运行中的子代理只入历史、当前回合不读取：明确语义或实现中断。
- [ ] 长跑/压测：alpha.14 的周期检查与压缩预算需要真实流量验证；记录到
      `docs/development/live-testing.md`。

## 工程

- [ ] 观察 `cargo clippy` 的 MSRV 与依赖升级；`napcat-link` 固定在 alpha.2。

## 文档

- [ ] 真实 API / NapCat 实测记录更新到 alpha.14（当前 live-testing 停在 alpha.12）。
- [ ] 若实现上述任一项，先改 `docs/guide` 对应页，再删 `limits.md` 对应行。
