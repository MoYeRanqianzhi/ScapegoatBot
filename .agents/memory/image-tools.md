---
name: image-tools
description: API 的 image.* 图片工具设计、系统字体约束与依赖；使用或修改图片 action 时先读
metadata:
  type: project
  scope: src/actions.rs 的 image specs、src/tools/api/image_ops.rs
  status: active
  last_verified: 2026-11-06
---

API 的图片工具已从 `media.*` 收敛为 `image.*`（Category 仍为 `Media`），共 5 个：

- `image.info`：只读宽高/字节/格式。
- `image.transform`：`crop` -> `rotate`（任意角度；90/180/270 精确，其余 imageproc 双线性扩画布）-> `width/height` 精确缩放或 `max_width/max_height` 等比 fit；至少一个操作。
- `image.convert`：按 out 扩展名编码 PNG/JPEG/WebP；`quality` 只对 JPEG 生效，WebP 为无损。
- `image.text`：系统字体绘制，参数 `font/x/y/size/color/opacity`；`font` 是 family 名，缺省 sans-serif；无字体时明确报错。
- `image.fonts`：`fontdb::Database::load_system_fonts()` 列出 family/style/weight/path，支持 `query/limit`。

关键约束：**不内置字体、不打包字体资产**（用户 2026-11-06 明确否决）。服务器 `hw1` 的 `fc-list` 为 0，因此那里 `image.fonts` 为空、`image.text` 报错；要中文需系统安装字体。

实现注意：模块文件是 `src/tools/api/image_ops.rs`，不能命名为 `image.rs`/`mod image`，否则会在 `tools::api` 内遮蔽 extern crate `image`，导致 `image::imageops`、`ImageFormat`、测试里的 `image::GenericImageView` 全部解析失败。依赖 `imageproc 0.27`、`fontdb 0.24`、`ab_glyph 0.2`；旧 `media.*` action 已删除，文档和 action 注册表同步改成 `image.*`。

**Evidence:** commit `48fe69f`; `src/actions.rs::specs_image`; `src/tools/api/image_ops.rs`; tests `image_info_and_transform_work`、`image_convert_honors_output_extension`、`image_text_and_fonts_work`；2026-11-06 全量测试 377 passed。

**Recheck when:** 增删 image action、改变字体来源/编码参数、或部署环境安装字体后。

**Why:** 旧 media 工具存在“水印只支持 ASCII/CJK 缺字体”的伪需求，用户要求改为系统字体并合并工具；模块名遮蔽 crate 是这次实际踩过的编译坑。

**How to apply:** 修改图片 API 时先看 `specs_image` 与 `image_ops.rs`；不要把字体文件加进仓库；新增 action 同步 `docs/guide/tools-and-vfs.md`、CHANGELOG 和测试。
