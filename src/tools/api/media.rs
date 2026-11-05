//! `API` 工具的媒体分支：图片解码、变换与编码，只读写 VFS。
//!
//! NapCat 的发送策略不在这里；`media.*` 不会产生对外影响。

use super::*;
use crate::vfs::Vfs;

impl ApiTool {
    pub(super) async fn run_media(
        &self,
        spec: &ActionSpec,
        params: &Value,
        ctx: &ToolContext,
    ) -> ToolResult {
        let input = match arg_str(params, "path") {
            Ok(path) => path,
            Err(error) => return error,
        };
        let input_path = match crate::vfs::path::VfsPath::parse(input) {
            Ok(path) => path,
            Err(error) => return ToolResult::error(format!("invalid VFS path: {error}")),
        };
        let bytes = match ctx.vfs.read_bytes(&input_path) {
            Ok(bytes) => bytes,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        let image = match image::load_from_memory(&bytes) {
            Ok(image) => image,
            Err(error) => return ToolResult::error(format!("cannot decode image: {error}")),
        };
        let (width, height) = (image.width(), image.height());
        if spec.action == "media.image_info" {
            return ToolResult::ok(
                json!({
                    "path": input,
                    "width": width,
                    "height": height,
                    "bytes": bytes.len(),
                    "format": image::guess_format(&bytes).ok().map(|f| format!("{f:?}")),
                })
                .to_string(),
            );
        }
        let out = match arg_str(params, "out") {
            Ok(path) => path,
            Err(error) => return error,
        };
        let out_path = match crate::vfs::path::VfsPath::parse(out) {
            Ok(path) => path,
            Err(error) => return ToolResult::error(format!("invalid output path: {error}")),
        };
        let ext = out.to_ascii_lowercase();
        if !(ext.ends_with(".png")
            || ext.ends_with(".jpg")
            || ext.ends_with(".jpeg")
            || ext.ends_with(".webp"))
        {
            return ToolResult::error("media output must end with .png/.jpg/.jpeg/.webp");
        }
        let output = match spec.action {
            "media.image_resize" => {
                let w = params.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
                let h = params.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
                if w == 0 || h == 0 || w > 8192 || h > 8192 {
                    return ToolResult::error("width and height must be between 1 and 8192");
                }
                image.resize(w, h, image::imageops::FilterType::Lanczos3)
            }
            "media.image_crop" => {
                let x = params.get("x").and_then(Value::as_u64).unwrap_or(0) as u32;
                let y = params.get("y").and_then(Value::as_u64).unwrap_or(0) as u32;
                let w = params.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
                let h = params.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
                if w == 0
                    || h == 0
                    || w > 8192
                    || h > 8192
                    || x >= width
                    || y >= height
                    || x.checked_add(w).is_none_or(|end| end > width)
                    || y.checked_add(h).is_none_or(|end| end > height)
                {
                    return ToolResult::error("crop rectangle is outside the image or too large");
                }
                image.crop_imm(x, y, w, h)
            }
            "media.image_rotate" => {
                let degrees = params.get("degrees").and_then(Value::as_u64).unwrap_or(0);
                match degrees {
                    90 => image::DynamicImage::ImageRgba8(image::imageops::rotate90(&image)),
                    180 => image::DynamicImage::ImageRgba8(image::imageops::rotate180(&image)),
                    270 => image::DynamicImage::ImageRgba8(image::imageops::rotate270(&image)),
                    _ => return ToolResult::error("degrees must be 90, 180 or 270"),
                }
            }
            "media.image_thumbnail" => {
                let w = params.get("max_width").and_then(Value::as_u64).unwrap_or(0) as u32;
                let h = params
                    .get("max_height")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32;
                if w == 0 || h == 0 || w > 8192 || h > 8192 {
                    return ToolResult::error("max_width/max_height must be 1..8192");
                }
                image.thumbnail(w, h)
            }
            "media.image_compress" => {
                let quality = params
                    .get("quality")
                    .and_then(Value::as_u64)
                    .unwrap_or(80)
                    .clamp(1, 100) as u8;
                // 扩展名已在上面校验过，只剩 png/jpg/jpeg/webp 四种。
                let format = if ext.ends_with(".png") {
                    image::ImageFormat::Png
                } else if ext.ends_with(".webp") {
                    image::ImageFormat::WebP
                } else {
                    image::ImageFormat::Jpeg
                };
                let mut encoded = Vec::new();
                let result = if format == image::ImageFormat::Jpeg {
                    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, quality)
                        .encode_image(&image.to_rgb8())
                } else {
                    let mut cursor = std::io::Cursor::new(&mut encoded);
                    image.write_to(&mut cursor, format).map(|_| ())
                };
                if let Err(error) = result {
                    return ToolResult::error(format!("cannot encode {format:?}: {error}"));
                }
                if let Err(error) =
                    ctx.vfs
                        .write_binary(&out_path, &encoded, crate::vfs::NodeKind::Image)
                {
                    return ToolResult::error(format!("cannot write {out}: {error}"));
                }
                return ToolResult::ok(
                    json!({
                        "path": out,
                        "source": input,
                        "width": width,
                        "height": height,
                        "bytes": encoded.len(),
                        "quality": quality,
                    })
                    .to_string(),
                );
            }
            "media.image_watermark" => {
                let text = params.get("text").and_then(Value::as_str).unwrap_or("sbot");
                let chars: Vec<char> = text.chars().collect();
                let mut glyphs = Vec::with_capacity(chars.len());
                for &ch in &chars {
                    match watermark_glyph(ch) {
                        Some(glyph) => glyphs.push(glyph),
                        None => {
                            return ToolResult::error(format!(
                                "media.image_watermark cannot render U+{:04X};                                  font8x8 has no CJK glyphs",
                                ch as u32
                            ));
                        }
                    }
                }
                let mut rgba = image.to_rgba8();
                let (w, h) = rgba.dimensions();
                let scale = 2u32;
                let text_w = chars.len() as u32 * 8 * scale;
                let x0 = w.saturating_sub(text_w + 8);
                let y0 = h.saturating_sub(8 * scale + 8);
                for (i, glyph) in glyphs.iter().enumerate() {
                    for (row, bits) in glyph.iter().enumerate() {
                        for col in 0..8 {
                            if bits & (1 << col) == 0 {
                                continue;
                            }
                            for dy in 0..scale {
                                for dx in 0..scale {
                                    let x = x0 + i as u32 * 8 * scale + col as u32 * scale + dx;
                                    let y = y0 + row as u32 * scale + dy;
                                    if x < w && y < h {
                                        rgba.put_pixel(x, y, image::Rgba([255, 255, 255, 180]));
                                    }
                                }
                            }
                        }
                    }
                }
                image::DynamicImage::ImageRgba8(rgba)
            }
            "media.image_convert" => image,
            other => return ToolResult::error(format!("unsupported media action {other:?}")),
        };
        let format = image::ImageFormat::from_path(out).unwrap_or(image::ImageFormat::Png);
        let mut encoded = std::io::Cursor::new(Vec::new());
        if let Err(error) = output.write_to(&mut encoded, format) {
            return ToolResult::error(format!("cannot encode image: {error}"));
        }
        let encoded = encoded.into_inner();
        if let Err(error) = ctx
            .vfs
            .write_binary(&out_path, &encoded, crate::vfs::NodeKind::Image)
        {
            return ToolResult::error(format!("cannot write {out}: {error}"));
        }
        ToolResult::ok(
            json!({
                "path": out,
                "source": input,
                "width": output.width(),
                "height": output.height(),
                "bytes": encoded.len(),
            })
            .to_string(),
        )
    }

    /// NapCat/扩展动作若直接返回 base64 图片，把它作为 tool result 的图片附件；
    /// 调用方（provider）会转成 image block，和 Playwright 截图的行为一致。
    pub(super) fn images_from_response(
        response: &Value,
    ) -> Vec<crate::transcript::ImageAttachment> {
        let data = response.get("data").unwrap_or(response);
        let mut out = Vec::new();
        for (key, mime) in [
            ("base64", "image/png"),
            ("image_base64", "image/png"),
            ("image", "image/png"),
        ] {
            let Some(raw) = data.get(key).and_then(Value::as_str) else {
                continue;
            };
            let (mime, payload) = raw
                .strip_prefix("data:")
                .and_then(|rest| rest.split_once(','))
                .map(|(header, payload)| {
                    (
                        header.split(';').next().unwrap_or(mime).to_owned(),
                        payload.to_owned(),
                    )
                })
                .unwrap_or_else(|| (mime.to_owned(), raw.to_owned()));
            if matches!(
                mime.as_str(),
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            ) && payload.len() <= 4_700_000
            {
                out.push(crate::transcript::ImageAttachment {
                    mime,
                    data_base64: payload,
                });
            }
            break;
        }
        out
    }
}

/// 在 font8x8 的各个 Unicode 字库中查找字形；CJK 不在其中（需要字体文件）。
fn watermark_glyph(ch: char) -> Option<[u8; 8]> {
    use font8x8::UnicodeFonts;
    [
        font8x8::BASIC_FONTS.get(ch),
        font8x8::LATIN_FONTS.get(ch),
        font8x8::GREEK_FONTS.get(ch),
        font8x8::BLOCK_FONTS.get(ch),
        font8x8::BOX_FONTS.get(ch),
        font8x8::HIRAGANA_FONTS.get(ch),
        font8x8::MISC_FONTS.get(ch),
        font8x8::SGA_FONTS.get(ch),
    ]
    .into_iter()
    .flatten()
    .next()
}
