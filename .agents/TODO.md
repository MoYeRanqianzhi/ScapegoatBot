# TODO

按优先级排列；完成后同步 `CHANGELOG.md` 与 `docs/guide/limits.md`。
已完成项直接删除（git 有记录）。

## 安全与设计对齐



## 配置与能力边界


## 运行与数据

- [ ] 长跑/压测：alpha.14 的周期检查与压缩预算需要真实流量验证；记录到
      `docs/development/live-testing.md`。

## 工程

- [ ] 观察 `cargo clippy` 的 MSRV 与依赖升级；`napcat-link` 固定在 alpha.2。

## 文档

- [ ] 真实 API / NapCat 实测记录更新到 alpha.14（当前 live-testing 停在 alpha.12）。
- [ ] 若实现上述任一项，先改 `docs/guide` 对应页，再删 `limits.md` 对应行。
