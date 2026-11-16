---
name: image-tools
description: API 的 local.media.image.* 图片工具设计、系统字体约束与依赖；使用或修改图片 action 时先读
metadata:
  type: project
  scope: src/actions.rs 的 specs_local_media_image、src/tools/api/image_ops.rs
  status: active
  last_verified: 2026-11-14
---

图片工具归属“本地工具 → 媒体 → 图片”，action 名 `local.media.image.*`（Category `Media`），
共 5 个（2026-11-07 从 `image.*` 改名，见 [[design-decisions]] 的命名分层）：

- `info`：只读宽高/字节/格式。
- `transform`：`crop` -> `rotate`（任意角度；90/180/270 精确，其余 imageproc 双线性扩画布，
  先算画布、单边超过 8192 拒绝）-> `width/height` 精确缩放或 `max_width/max_height` 等比 fit。
- `convert`：按 out 扩展名编码 PNG/JPEG/WebP；`quality` 只对 JPEG 生效，WebP 为无损。
- `text`：系统字体绘制，参数 `font/x/y/size/color/opacity`；最多 1000 字、单行（控制字符拒绝）；
  无字体时明确报错。渲染（imageproc `draw_text_mut`）没有字体回退，缺的字符会画成 `.notdef`
  方块或不画而动作照样成功，所以 `pick_font` 先核对字形：指定 `font` 时缺字报错并列出字符；
  不指定时先试 fontdb 的默认无衬线族，不行就换第一个画得全的字体（正体、字重近 400 优先，
  跳过 LastResort 这类给所有码位配示意字形的占位字体）。
- `fonts`：`fontdb::Database::load_system_fonts()` 列出 family/style/weight/path，支持 `query/limit`。

解码统一走 `decode_limited`（宽高 ≤ 8192，防解压炸弹）。

关键约束：**不内置字体、不打包字体资产**（用户 2026-11-06 明确否决，见 [[feature-scope-correction]]）。
服务器 `hw1` 的 `fc-list` 为 0，那里 `fonts` 为空、`text` 报错；要中文需系统安装字体。

实现注意：模块文件是 `src/tools/api/image_ops.rs`，不能命名为 `image.rs`/`mod image`，否则会在
`tools::api` 内遮蔽 extern crate `image`。`ApiTool::execute` 按顶层命名空间 `local` 分派到
`run_image`；以后加别的本地工具时要在那里按第二段再分。依赖 `imageproc 0.27`、`fontdb 0.24`、`ab_glyph 0.2`。

**Evidence:** `src/actions.rs::specs_local_media_image`；`src/tools/api/image_ops.rs`；
测试 `image_info_and_transform_work`、`image_convert_honors_output_extension`、`image_text_and_fonts_work`、
`api_list_namespace_is_a_hierarchical_prefix`、`rotation_refuses_canvases_beyond_the_side_limit`、
`an_automatically_picked_font_covers_the_whole_text`、`a_character_no_font_has_is_refused`。
缺字静默成功是 2026-11-12 GitHub Actions 的 ubuntu 机器上暴露的（run 43277381531：系统字体库
里排第一的族没有拉丁字形，旧测试拿它画 `Hi 42`，一个像素都没变）。

**Recheck when:** 增删图片 action、加入其他 `local.*` 工具、改变字体来源或部署环境安装字体后。

**Why:** 命名层级是用户明确要求；模块名遮蔽 crate 是实际踩过的编译坑。

**How to apply:** 改图片 API 时同步 `skills/sbot-use/references/tools-and-vfs.md`、CHANGELOG、system prompt 与测试；
不要把字体文件加进仓库。
