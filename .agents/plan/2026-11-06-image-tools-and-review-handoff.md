# Handoff: image tools + review remediation

Status: active. Written 2026-11-06 for a fresh session continuing this repository.
Worktree: `G:/ClaudeProjects/ScapegoatBot/ScapegoatBot` (main checkout; do not build in subagents or on C:).

## 1. Current state (verified)

- 图片功能代码：`48fe69f`；交接文档在后续提交（以 `git log -1` 为准）。
- Tests: `cargo test --locked --all-targets` = **377 passed** (365 lib + 12 integration), 2 ignored, 0 failed.
- `cargo clippy --locked --all-targets -- -D warnings` and `cargo fmt --all` pass at this commit.
- Tag `v0.2.0-alpha.35` exists but does **not point at HEAD** (`git describe` shows `-N-g...`). Do not publish it as-is; re-point after the next accepted change.
- Release build was last verified **before** the image tool rewrite and before the new `imageproc/fontdb/ab_glyph` dependencies; the current HEAD has not had `cargo build --release`.
- Server `hw1` `sbot.service` is stopped. Nothing has been pushed, tagged remotely, released, or deployed.
- NapCat is **logged out** (QR login loop since 2026-11-05 05:22, `Login Error ErrType:1 ErrCode:3`). `get_image`, QQ CDN download, `get_msg`/`get_forward_msg` live paths remain unverified.

## 2. Image tool rewrite (done, awaiting user approval of the API surface)

Old `media.*` actions were deleted and replaced by 5 `image.*` actions. Category remains `Category::Media`; namespace is `image` (not `media`). Implementation: `src/tools/api/image_ops.rs` (renamed from `media.rs`; module declared as `image_ops` because a local module named `image` shadows the `image` crate inside `tools::api`). Registry: `src/actions.rs::specs_image`. Dispatch: `src/tools/api.rs` match arm `"image" => self.run_image(...)`.

| action | params | behavior |
|---|---|---|
| `image.info` | `path` | read-only; width/height/bytes/format |
| `image.transform` | `path, out, crop?, rotate?, width?, height?, max_width?, max_height?, quality?` | order crop -> rotate -> resize; `rotate` any degrees (90/180/270 exact; arbitrary uses imageproc bilinear with expanded canvas); exact `width+height` or fit `max_width+max_height`; at least one op required |
| `image.convert` | `path, out, quality?` | PNG/JPEG/WebP by output extension; quality 1-100 only for JPEG; WebP is lossless in the current image crate |
| `image.text` | `path, out, text, font?, x?, y?, size?, color?, opacity?` | text from **system fonts** via fontdb; `font` = family name, default sans-serif; x/y top-left; size 1-512; color `#RRGGBB`; opacity 0-1; no fonts -> explicit error |
| `image.fonts` | `query?, limit?` | list installed families with `family/style/weight/path`; query filter; limit default 50, max 200 |

Design constraints from the user (do not regress):

- **No bundled font asset.** The user rejected built-in fonts. `image.text` uses system font discovery; on the server (`fc-list` count 0) `image.fonts` returns empty and `image.text` must fail clearly. Do not add a font file or a font fallback asset.
- Position, color, opacity, font family, and size are caller-selected.
- `image.text` replaces the old watermark tool; do not reintroduce `media.image_watermark`.
- The user asked for the new list to be reviewed before further work; if they change names/params, update this plan and `docs/guide/tools-and-vfs.md` together.

Dependencies added (Cargo.toml/Cargo.lock): `imageproc = "0.27.0"`, `fontdb = "0.24.0"`, `ab_glyph = "0.2"` (direct for `FontVec`). `imageproc` pulls a large tree (rayon, rustfft, tiff, etc.); build time increased.

Tests added: `image_info_and_transform_work`, `image_convert_honors_output_extension`, `image_text_and_fonts_work` in `src/tools/api.rs`; they skip the text assertion when the machine has no fonts. On this Windows host fonts exist and the text path rendered.

## 3. Review findings still to fix

Two read-only subagent reviews ran on baseline `f2cf79d` (before the image rewrite). They did not modify files or run cargo. Use the line numbers as leads; re-check against current files because the image module moved.

### 3.1 Structure (P1)

1. `src/app/assembly.rs` is 1017 production lines: `build_agent` 383, `assemble_tools` 241, plus `open_stores`, wake presets, QQ connect, principal resolution, security reviewer, model loading. Only 1 test (17 lines). Split `open_stores` into `stores.rs`; split `assemble_tools` registration into 3 helpers; split `build_agent` into provider/ops/budget/tools/review/hot-reload groups. Highest risk; add assembly-level tests first.
2. `src/agent/runner.rs`: 15-field Runner with 7 responsibilities; `run_subagent` re-lists 15 LoopConfig fields (870-898) duplicating `loop_config()` (465-492); budget-skip image persistence duplicated (613-616, 635-638); skip checks duplicated with `injected.rs:7-37`; duplicate `debug!("turn completed")` at 802-813. `commit_turn` has 9 args; package into `TurnCommit`.
3. `src/agent/loop.rs`: `run_with_steering` 244 lines with 4 terminal branches; extract budget/input-gate/request/output-gate/terminal handlers. `request_once` stream match and `BlockAccumulator` belong in `agent/stream.rs`; render helpers in `agent/render.rs`.
4. `src/monitor/mod.rs`: 898 production lines mixing predicate DSL, runtime, CRUD, delivery, scheduler, persistence, missed occurrences, cron, config loading; `Monitor` has 23 fields. Split `predicate.rs`, `cron.rs`, `delivery.rs`, `config.rs`; group `Monitor` fields into lifecycle/runtime.
5. `src/vfs/sqlite.rs`: `edit_text_count` vs `Vfs::edit_text`, `read_lines` vs `Vfs::read`, usage SQL repeated, `write_binary` vs `copy_node` binary branch, `is_dynamic` three copies. `edit_text`/`read`/`purge_trash` are test-only trait surface. Delegating `edit_text` to `edit_text_count` and `read` to `read_lines` would remove ~60 lines; `write_blob_tx` removes another ~20-25 and removes the copy-path staging drift.
6. `src/tools/api/policy.rs`: `run_napcat` is a 223-line ordered security chain (target guard, hold, receive scope, owner intent, ActionGate, send scope, cleaning/BeforeSend, ledger, call). Split into ordered private helpers without reordering; add a test that asserts stage order.

### 3.2 Redundancy (P2)

- Monitor spec parsing is duplicated across `tools/agent.rs:51-169`, `tools/monitor.rs:115-269`, `monitor/mod.rs:938-1046`, plus 4 `MonitorSpec` literals. Consolidate into `MonitorSpec::from_json/from_config`; estimated 110-130 lines.
- Image mime/compression helpers: mime whitelist in `agent/runner/images.rs`, `tools/fs.rs`, `mcp/mod.rs`; JPEG shrink logic in `mcp/mod.rs:485-507` and `tools/fs.rs:24-32`; `ImageAttachment` construction in 5 places; two inconsistent base64 branches in `images.rs`. Create one `image_support` helper; 45-55 lines.
- Provider stream finish/error strings duplicated in `anthropic.rs`, `openai.rs`, `chat.rs`; add default `finish()` and error constructors; 25-30 lines.
- Session/messages encryption line encoding, header read/parse, append mode, `EncryptionMode` string mapping, `write_private` unix/windows bodies duplicated; use `EncryptionMode::as_str/parse`, `jsonl::append_line`, `read_header`; 50-60 lines.
- `complete_text` call sites repeat the same 4-branch error mapping (5 places); add `display_with(label)`; 15-20 lines.
- `self_message_from_params` (`tools/api.rs:422-470`) duplicates `qq/event.rs::parse_segment`; expose `parse_segment` and reuse; 40-45 lines.
- At/timer parsing triplicated (`monitor/mod.rs:1040-1046`, `tools/monitor.rs:253-260`, `tools/agent.rs:75-88`); single parse helper; 10-12 lines.
- Assembly/hooks notify block duplicated (`runner.rs:343-360`, `assembly.rs:864-885`); `notify_owner` helper; 12 lines.
- Config JSON read/parse boilerplate (`config/auth.rs:56-66`, `config/models.rs:134-144`, `app/bootstrap.rs:126-130`); `config::read_json<T>`; 10-12 lines.
- Broken whitespace strings (model-visible errors) at `policy.rs:92/112/125/231`, `action_gate.rs:663/680/706`, `web.rs:345/351`, `assembly.rs:416`; old `media.rs:159` is gone. Fix and add a grep check.
- Zero-reference `pub fn`: `actions.rs::is_outbound`, `qq/event.rs::as_text`, `qq/format.rs::render_batch/segment_to_napcat/scope_name`, `security/events.rs::count_blocked`, `session/turn_buffer.rs::load_context`, `tools/fs.rs::read_static`. Delete or document as library API.
- `Box::leak` remains in `prompt/mod.rs:37` (static prefix); replace with `OnceLock` if touched. Module cycles: none (verified). `too_many_arguments` allows: 4.

### 3.3 Documentation/source comments (P1 - fix before publishing)

- `docs/development/prompt-maintenance.md:59` claims only owner/agent-origin memories are injected and untrusted never reaches memory_context; `docs/guide/tools-and-vfs.md:82` claims Memory is the only writer for `/USER.md`, `/memory/*`, `/MEMORY.md`. Code: `prompt/mod.rs:118-135` injects `/MEMORY.md` verbatim without source filtering; `storage/mod.rs:235-242` has `source` only, no origin; `tools/memory.rs:10-12` says source is audit-only and VFS file tools can write; `tools/fs.rs` has no `/MEMORY.md` write guard. Either delete the promises or implement them. (Not changed yet.)
- `README.md:109`, `docs/guide/usage.md:12/30-34`: `sbot status` is documented as text with `usage/用量`; actual CLI prints pretty JSON and `status_payload` has no usage/input/output fields. Rewrite against `src/cli/mod.rs:313-316` and `src/app/mod.rs:269-321`.
- `docs/guide/configuration.md:44` `max_steps` default 48 vs code 64 (`agent.rs:84`); `:48` `retry_max_ms` default 8000 vs 30000 (`agent.rs:79`).
- `docs/guide/usage.md:106` says only `[filter]` hot-reloads; `configuration.md:82` and README:152 say filter+queue; code `assembly.rs:827-832` reloads those two. Unify.
- `docs/guide/security.md:211` says periodic review does not share the main prefix; `docs/guide/limits.md:21` says it reuses system prefix; code `security/review.rs:72-124` reuses it. Use limits.md.
- `docs/development/README.md:30` migrations v1-v11 vs v12 (`storage/mod.rs:32`, v12 at 305-312); `:47` still cites deleted `ScopeCache`; `:41` monitor sources omit `stream` (`config/sections/monitor.rs:96-100`); README:183 directory tree still says `app.rs`.
- `config.example.toml:45` says `compaction_model` unused; code `assembly.rs:682-695` uses it.
- `docs/guide/README.md:18` says 19 built-in tools; registry has 20 (11 fs + 9), default config enables 17; `:64-65` broken sentence ("同一 chat 的连发会在 / 写入 inbox").
- Source comments: `tools/mod.rs:8-9` Permission audit claim has no consumer; `swarm/mod.rs:9` says nested spawn unsupported but it is supported (`runner.rs:845-846`, `swarm/mod.rs:381`); `hooks.rs:3-5` says only BeforeSend wired but fire() is wired; `prompt/mod.rs:31` says static prefix selects identity but `:43` ignores mode; `config/mod.rs:112` leftover "上下文作用域。"; `tools/api.rs:5/220` "three namespaces" vs four (now `napcat/agent/image/api`); `api.rs:227`/`actions.rs` `agent.compact chat` is ignored by single-session `scope.rs:15-22` (document as compat or remove); `runner.rs:158` stale "每个 transcript scope 一个回合锁"; `turn_buffer.rs:65-66` says scope can be `group-<id>` but writes `"shared"` always.
- `.agents/plan/refactor-2.md` is stale: says alpha.35 not tagged (tag exists), per_chat still supported, references deleted `qq/resync.rs`/`control_slots`/`ScopeCache`. Mark it closed/superseded. `.agents/plan/2026-11-03-code-review.md` is explicitly a pre-fix snapshot; keep.
- CHANGELOG mentions alpha.23/26/28/34 but no tags exist for them; note as historical if needed.

Already fixed this session: README hold semantics, `[skills]` keys, `tools/mod.rs` hold comments, `docs/development/README.md` actions path/count (approx), image docs/CHANGELOG. Do not redo.

## 4. Next steps (recommended order)

1. Wait for user approval/edits of the 5-action image surface. If approved, update TODO/memory and proceed.
2. Fix documentation P1: memory source-gating claims, status docs, defaults/hot-reload, v12/monitor map. Commit separately.
3. Low-risk structure cleanup: broken whitespace/error strings, provider `finish` default, image mime/attachment helper. Run tests after each.
4. Bigger refactors with test nets: monitor spec parsing -> VFS duplication -> runner/loop -> policy chain -> assembly split (add assembly tests before splitting).
5. Live verification after user scans NapCat QR: `get_image`, QQ CDN inbound image, `get_msg`/`get_forward_msg`; record evidence in TODO/CHANGELOG.
6. Release: re-point tag to final HEAD, rebuild release locally, update CHANGELOG test count, then ask for explicit push/release/deploy authorization.

## 5. Environment / build notes

- Subagents must not run cargo; builds only in this main checkout default `target/` on G:, never C:. Use `cargo test --locked --all-targets`.
- This Windows host has a cargo TLS/schannel failure against crates.io (`SSL connect error ... schannel`), while `curl` to crates.io/rsproxy works. Add dependencies or fetch with:
  `cargo --config 'source.crates-io.replace-with="rsproxy"' --config 'source.rsproxy.registry="sparse+https://rsproxy.cn/index/"' <command>`
  Do not commit this mirror config; CI uses normal crates.io. `Cargo.lock` already contains the new crates.
- `git status` was clean after `48fe69f` before writing these docs.
- Tag is local-only; do not push without confirmation.

## 6. Verification baseline commands

```
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all --check
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
```

Expected: 377 passed (365+12), 2 ignored; clippy/fmt/doc clean at `48fe69f`.
