---
name: storage-invariants
description: 消息归档、transcript 与 turn buffer 的数据不变量；改存储、迁移或崩溃恢复时读
metadata:
  type: project
  scope: src/storage、src/session、vfs blob 与迁移
  status: active
  last_verified: 2026-11-17
---

存储层的硬不变量：

- `MessageStore::append` 全程持有 `append_lock`：行号 = 已提交行数，取号、写文件、
  提交事务必须在同一临界区，否则并发 append 会复用行号并让读取解密出错误消息。
  `SessionStore::append` 同样由自己的 `append_lock` 串行。
- 消息去重键是 `(chat, message_id)`：同一条消息若被归到不同 chat，会被存两次。
- 读取时校验「索引行号指向的行内 id == 请求 id」；不一致按缺失处理并告警，
  AAD 绑定的是行内 id + file_id + line_no，本身发现不了索引错位。
- **一行怎么读由这一行自己和当前配置决定，不由文件头决定**（`storage::envelope::open_line`，
  消息归档、transcript、turn buffer 共用）：密文行在任何 `[storage].encryption` 下都解；明文行
  （`nonce` 为空）只在配置为 `none` 时读，配置为加密时是错误、被跳过。方向不能反过来：加密
  配置下接受明文行，等于让能写数据目录但拿不到密钥的人伪造历史——密文的认证就白做了。
  （2026-11-16 的第一版修法就犯了这个错，当天改掉。）代价：`none` 期间写下的行，改回加密后
  读不出，要读就把配置改回 `none`；这写在 CHANGELOG 与 troubleshooting 里。
- `rebuild_index` 同时修 `messages`、`message_files`、`inbox_state` 三张表；
  只补 messages 会让配额、保留、月视图与未读数全部失准。
- 崩溃恢复：`turn_buffer` 中 open/committing 的回合在启动时补齐缺失的 tool_result，
  按“已写入 transcript 的后缀”去重后追加，避免 committing 中断导致整段重复。
  单会话化后 `turn_buffer.scope` / `transcript_files.scope` 列只为旧数据保留，新写入恒为 `shared`。
- 迁移只追加、不修改已发布条目；当前最新是 v14。v13 与 v14 都是删表（`memory_entries`：只有
  已删除的 `Memory` 工具写过它；`compaction_checkpoints`：只写不读的摘要副本）——没有读者的
  派生表删掉，不留着当“以后也许有用”的副本。

**Evidence:** `src/storage/messages.rs`（`append` / `read_line` / `rebuild_index`）、
`src/storage/mod.rs`（`MIGRATIONS`）、`src/session/{mod,turn_buffer}.rs`；
测试 `storage::messages::tests::{concurrent_appends_keep_distinct_line_numbers,
index_drift_does_not_return_the_wrong_message,
sealed_lines_are_always_readable_and_plain_lines_only_without_encryption,
rebuild_restores_file_counts_and_inbox_state, retention_removes_orphan_inbox_rows}`、
`session::tests::sealed_lines_are_always_readable_and_plain_lines_only_without_encryption`、
`session::turn_buffer::tests::buffered_blocks_follow_the_same_rule_as_stored_lines`、
`session::turn_buffer::tests::{committing_turn_is_recovered,
partially_appended_committing_turn_is_completed_without_duplicates}`、
`agent::runner::tests::automatic_compaction_keeps_the_turn_blocks`（2026-11-17 核对存在）。
加密规则的提交：`5ee49c0`、`cdfeaf4`。

**Recheck when:** 新增 JSONL 字段、调整索引表、改恢复顺序、增加迁移或新的存储后端。

**Why:** 这些不变量曾各自被破坏过一次，表现都是「静默错数据」而不是报错。

**How to apply:** 改写入路径时保持单一临界区；新增派生表必须能从文件重建；
新迁移追加在末尾并更新本条的版本号。
