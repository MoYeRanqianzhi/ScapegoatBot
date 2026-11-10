//! `API` 的 `local.media.image.*` 分支：VFS 内图片的检查、变换、编码与文字。
//!
//! 只读写 VFS，不产生对外影响；字体来自系统字体库（`fontdb`），不内置字体。
//! 解码、变换、编码都是 CPU 密集的同步工作，整个动作在阻塞线程池里跑
//! （[`crate::media::blocking`]），解码上限与 `Read` 看图、MCP 截图一致。

use std::collections::HashSet;

use ab_glyph::{FontVec, PxScale};
use image::{DynamicImage, Rgba, RgbaImage};
use imageproc::drawing::draw_text_mut;
use imageproc::geometric_transformations::{Border, Interpolation, rotate_about_center_no_crop};

use super::*;
use crate::media::{MAX_IMAGE_SIDE, MAX_INPUT_BYTES, decode_limited};
use crate::vfs::Vfs;

impl ApiTool {
    pub(super) async fn run_image(
        &self,
        spec: &ActionSpec,
        params: &Value,
        ctx: &ToolContext,
    ) -> ToolResult {
        // 阻塞任务要求 `'static`：动作名本来就是静态的，参数与上下文（全是 Arc）克隆进去。
        let action = spec.action;
        let params = params.clone();
        let ctx = ctx.clone();
        let outcome = crate::media::blocking(move || match action {
            "local.media.image.info" => image_info(&params, &ctx),
            "local.media.image.transform" => image_transform(&params, &ctx),
            "local.media.image.convert" => image_convert(&params, &ctx),
            "local.media.image.text" => image_text(&params, &ctx),
            "local.media.image.fonts" => image_fonts(&params),
            other => ToolResult::error(format!("unsupported image action {other:?}")),
        })
        .await;
        outcome.unwrap_or_else(ToolResult::error)
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
            let payload = raw
                .strip_prefix("data:image/")
                .and_then(|rest| rest.split_once(";base64,"))
                .map(|(kind, body)| (format!("image/{kind}"), body))
                .unwrap_or_else(|| (mime.to_owned(), raw));
            if payload.1.is_empty() {
                continue;
            }
            out.push(crate::transcript::ImageAttachment {
                mime: payload.0,
                data_base64: payload.1.to_owned(),
            });
        }
        out
    }
}

fn image_info(params: &Value, ctx: &ToolContext) -> ToolResult {
    let (input, image, bytes) = match read_image(params, ctx) {
        Ok(value) => value,
        Err(error) => return ToolResult::error(error),
    };
    ToolResult::ok(
        json!({
            "path": input,
            "width": image.width(),
            "height": image.height(),
            "bytes": bytes.len(),
            "format": image::guess_format(&bytes).ok().map(|format| format!("{format:?}")),
        })
        .to_string(),
    )
}

fn image_transform(params: &Value, ctx: &ToolContext) -> ToolResult {
    let (input, image, _) = match read_image(params, ctx) {
        Ok(value) => value,
        Err(error) => return ToolResult::error(error),
    };
    let (out, out_path, ext) = match output_path(params, ctx) {
        Ok(value) => value,
        Err(error) => return ToolResult::error(error),
    };
    let quality = quality(params);
    let mut rgba = image.to_rgba8();
    let mut changed = false;

    if let Some(crop) = params.get("crop").filter(|value| !value.is_null()) {
        let x = crop.get("x").and_then(Value::as_u64).unwrap_or(0) as u32;
        let y = crop.get("y").and_then(Value::as_u64).unwrap_or(0) as u32;
        let width = crop.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
        let height = crop.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
        if width == 0
            || height == 0
            || width > 8192
            || height > 8192
            || x >= rgba.width()
            || y >= rgba.height()
            || x.checked_add(width).is_none_or(|end| end > rgba.width())
            || y.checked_add(height).is_none_or(|end| end > rgba.height())
        {
            return ToolResult::error("crop rectangle is outside the image or too large");
        }
        rgba = image::imageops::crop_imm(&rgba, x, y, width, height).to_image();
        changed = true;
    }

    if let Some(degrees) = params.get("rotate").and_then(Value::as_f64) {
        if !degrees.is_finite() {
            return ToolResult::error("rotate must be a finite number of degrees");
        }
        rgba = match rotate_rgba(rgba, degrees as f32) {
            Ok(rotated) => rotated,
            Err(error) => return ToolResult::error(error),
        };
        changed = true;
    }

    match (
        params.get("width").and_then(Value::as_u64),
        params.get("height").and_then(Value::as_u64),
    ) {
        (Some(width), Some(height)) => {
            if width == 0 || height == 0 || width > 8192 || height > 8192 {
                return ToolResult::error("width and height must be between 1 and 8192");
            }
            rgba = image::imageops::resize(
                &rgba,
                width as u32,
                height as u32,
                image::imageops::FilterType::Lanczos3,
            );
            changed = true;
        }
        (None, None) => {
            if let (Some(max_width), Some(max_height)) = (
                params.get("max_width").and_then(Value::as_u64),
                params.get("max_height").and_then(Value::as_u64),
            ) {
                if max_width == 0 || max_height == 0 || max_width > 8192 || max_height > 8192 {
                    return ToolResult::error("max_width/max_height must be between 1 and 8192");
                }
                rgba = fit_within(rgba, max_width as u32, max_height as u32);
                changed = true;
            }
        }
        _ => return ToolResult::error("width and height must be provided together"),
    }

    if !changed {
        return ToolResult::error(
            "no transform requested: provide crop, rotate, width/height or max_width/max_height",
        );
    }
    finish(
        input,
        out,
        out_path,
        ext,
        DynamicImage::ImageRgba8(rgba),
        quality,
        ctx,
    )
}

fn image_convert(params: &Value, ctx: &ToolContext) -> ToolResult {
    let (input, image, _) = match read_image(params, ctx) {
        Ok(value) => value,
        Err(error) => return ToolResult::error(error),
    };
    let (out, out_path, ext) = match output_path(params, ctx) {
        Ok(value) => value,
        Err(error) => return ToolResult::error(error),
    };
    finish(input, out, out_path, ext, image, quality(params), ctx)
}

fn image_text(params: &Value, ctx: &ToolContext) -> ToolResult {
    let (input, image, _) = match read_image(params, ctx) {
        Ok(value) => value,
        Err(error) => return ToolResult::error(error),
    };
    let (out, out_path, ext) = match output_path(params, ctx) {
        Ok(value) => value,
        Err(error) => return ToolResult::error(error),
    };
    let text = params.get("text").and_then(Value::as_str).unwrap_or("");
    if text.is_empty() {
        return ToolResult::error("text must not be empty");
    }
    // 光栅化开销随字数 × 字号² 增长；画布外的字形同样会被光栅化。
    if text.chars().count() > MAX_TEXT_CHARS {
        return ToolResult::error(format!("text must be at most {MAX_TEXT_CHARS} characters"));
    }
    let size = params.get("size").and_then(Value::as_f64).unwrap_or(32.0);
    if !(1.0..=512.0).contains(&size) {
        return ToolResult::error("size must be between 1 and 512");
    }
    let x = params
        .get("x")
        .and_then(Value::as_i64)
        .unwrap_or(8)
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    let y = params
        .get("y")
        .and_then(Value::as_i64)
        .unwrap_or(8)
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    let color = match parse_color(
        params
            .get("color")
            .and_then(Value::as_str)
            .unwrap_or("#ffffff"),
    ) {
        Ok(color) => color,
        Err(error) => return ToolResult::error(error),
    };
    let opacity = params
        .get("opacity")
        .and_then(Value::as_f64)
        .unwrap_or(1.0)
        .clamp(0.0, 1.0);
    let family = params.get("font").and_then(Value::as_str);
    let font = match load_font(family) {
        Ok(font) => font,
        Err(error) => return ToolResult::error(error),
    };
    let mut rgba = image.to_rgba8();
    let color = Rgba([
        color[0],
        color[1],
        color[2],
        (opacity * 255.0).round() as u8,
    ]);
    draw_text_mut(
        &mut rgba,
        color,
        x,
        y,
        PxScale::from(size as f32),
        &font,
        text,
    );
    finish(
        input,
        out,
        out_path,
        ext,
        DynamicImage::ImageRgba8(rgba),
        quality(params),
        ctx,
    )
}

fn image_fonts(params: &Value) -> ToolResult {
    let query = params
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;
    let mut database = fontdb::Database::new();
    database.load_system_fonts();
    let mut seen = HashSet::new();
    let mut fonts = Vec::new();
    for face in database.faces() {
        let Some((family, _)) = face.families.first() else {
            continue;
        };
        if !query.is_empty() && !family.to_lowercase().contains(&query) {
            continue;
        }
        if !seen.insert((family.clone(), format!("{:?}", face.style), face.weight.0)) {
            continue;
        }
        let path = match &face.source {
            fontdb::Source::File(path) => Some(path.display().to_string()),
            _ => None,
        };
        fonts.push(json!({
            "family": family,
            "style": format!("{:?}", face.style).to_lowercase(),
            "weight": face.weight.0,
            "path": path,
        }));
        if fonts.len() >= limit {
            break;
        }
    }
    ToolResult::ok(
        json!({
            "count": fonts.len(),
            "fonts": fonts,
        })
        .to_string(),
    )
}

fn read_image(
    params: &Value,
    ctx: &ToolContext,
) -> Result<(String, DynamicImage, Vec<u8>), String> {
    let input = arg_str(params, "path").map_err(|error| error.content)?;
    let path = crate::vfs::path::VfsPath::parse(input)
        .map_err(|error| format!("invalid VFS path: {error}"))?;
    // 先看大小再读：超过解码上限的文件不读进内存。
    let size = ctx.vfs.stat(&path).map_err(|error| error.to_string())?.size;
    if size > MAX_INPUT_BYTES {
        return Err(format!(
            "{input} is {size} bytes; image actions accept at most {MAX_INPUT_BYTES}"
        ));
    }
    let bytes = ctx
        .vfs
        .read_bytes(&path)
        .map_err(|error| error.to_string())?;
    let image = decode_limited(&bytes)?;
    Ok((input.to_owned(), image, bytes))
}

/// `local.media.image.text` 一次最多绘制的字符数。
const MAX_TEXT_CHARS: usize = 1000;

/// 解析并检查输出路径：`out` 必须是合法 VFS 路径、扩展名是支持的图片格式。
///
/// 开了 `allow_send` 的自定义子代理拿到完整的 `API`，也能调用这里（`docs` 的 `API` 只读，
/// 到不了这一步），所以与 `Write` 一样经过
/// [`crate::tools::subagent_write_denied`]：不能把图片写进 `/memory`、`/config` 等受保护路径。
/// `/trash` 由 VFS 写入层拒绝，这里不重复。
fn output_path(
    params: &Value,
    ctx: &ToolContext,
) -> Result<(String, crate::vfs::VfsPath, String), String> {
    let out = arg_str(params, "out").map_err(|error| error.content)?;
    let path = crate::vfs::path::VfsPath::parse(out)
        .map_err(|error| format!("invalid output path: {error}"))?;
    // 受保护路径对子代理只读；主代理返回 None，照常写。
    if let Some(denied) = crate::tools::subagent_write_denied(ctx, &path) {
        return Err(denied.content);
    }
    let ext = out.to_ascii_lowercase();
    if !(ext.ends_with(".png")
        || ext.ends_with(".jpg")
        || ext.ends_with(".jpeg")
        || ext.ends_with(".webp"))
    {
        return Err("image output must end with .png/.jpg/.jpeg/.webp".to_owned());
    }
    Ok((out.to_owned(), path, ext))
}

fn quality(params: &Value) -> u8 {
    params
        .get("quality")
        .and_then(Value::as_u64)
        .unwrap_or(80)
        .clamp(1, 100) as u8
}

#[allow(clippy::too_many_arguments)]
fn finish(
    input: String,
    out: String,
    out_path: crate::vfs::VfsPath,
    ext: String,
    image: DynamicImage,
    quality: u8,
    ctx: &ToolContext,
) -> ToolResult {
    let bytes = match encode_image(&image, &ext, quality) {
        Ok(bytes) => bytes,
        Err(error) => return ToolResult::error(error),
    };
    if let Err(error) = ctx
        .vfs
        .write_binary(&out_path, &bytes, crate::vfs::NodeKind::Image)
    {
        return ToolResult::error(format!("cannot write {out}: {error}"));
    }
    ToolResult::ok(
        json!({
            "path": out,
            "source": input,
            "width": image.width(),
            "height": image.height(),
            "bytes": bytes.len(),
            "quality": quality,
        })
        .to_string(),
    )
}

fn encode_image(image: &DynamicImage, ext: &str, quality: u8) -> Result<Vec<u8>, String> {
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
    result
        .map(|()| encoded)
        .map_err(|error| format!("cannot encode {format:?}: {error}"))
}

fn rotate_rgba(image: RgbaImage, degrees: f32) -> Result<RgbaImage, String> {
    let normalized = degrees.rem_euclid(360.0);
    if normalized.abs() < f32::EPSILON {
        return Ok(image);
    }
    if (normalized - 90.0).abs() < 0.01 {
        return Ok(image::imageops::rotate90(&image));
    }
    if (normalized - 180.0).abs() < 0.01 {
        return Ok(image::imageops::rotate180(&image));
    }
    if (normalized - 270.0).abs() < 0.01 {
        return Ok(image::imageops::rotate270(&image));
    }
    // 任意角度会扩画布，宽高都可能超过输入（8192×8192 转 45° 约 11586²）。
    // 先按 imageproc `rotate_about_center_no_crop` 的同一公式算出画布，超限就拒绝。
    let (width, height) = image.dimensions();
    let (sin, cos) = normalized.to_radians().sin_cos();
    let (sin, cos) = (f64::from(sin.abs()), f64::from(cos.abs()));
    let out_width = (f64::from(height) * sin + f64::from(width) * cos).ceil();
    let out_height = (f64::from(height) * cos + f64::from(width) * sin).ceil();
    if out_width > f64::from(MAX_IMAGE_SIDE) || out_height > f64::from(MAX_IMAGE_SIDE) {
        return Err(format!(
            "rotating a {width}x{height} image by {degrees} degrees needs a \
             {out_width}x{out_height} canvas; the limit is {MAX_IMAGE_SIDE} per side. \
             Crop or shrink it first."
        ));
    }
    Ok(rotate_about_center_no_crop(
        &image,
        normalized.to_radians(),
        Interpolation::Bilinear,
        Border::Constant(Rgba([0, 0, 0, 0])),
    ))
}

fn fit_within(image: RgbaImage, max_width: u32, max_height: u32) -> RgbaImage {
    let (width, height) = image.dimensions();
    if width <= max_width && height <= max_height {
        return image;
    }
    let scale = (max_width as f64 / width as f64).min(max_height as f64 / height as f64);
    let target_width = ((width as f64 * scale).round() as u32).max(1);
    let target_height = ((height as f64 * scale).round() as u32).max(1);
    image::imageops::resize(
        &image,
        target_width,
        target_height,
        image::imageops::FilterType::Lanczos3,
    )
}

fn parse_color(raw: &str) -> Result<Rgba<u8>, String> {
    let hex = raw.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err("color must be a #RRGGBB hex string".to_owned());
    }
    let value = u32::from_str_radix(hex, 16).map_err(|error| error.to_string())?;
    Ok(Rgba([
        ((value >> 16) & 0xff) as u8,
        ((value >> 8) & 0xff) as u8,
        (value & 0xff) as u8,
        255,
    ]))
}

fn load_font(family: Option<&str>) -> Result<FontVec, String> {
    let mut database = fontdb::Database::new();
    database.load_system_fonts();
    let family = family
        .map(fontdb::Family::Name)
        .unwrap_or(fontdb::Family::SansSerif);
    let query = fontdb::Query {
        families: &[family],
        ..Default::default()
    };
    let id = database.query(&query).ok_or_else(|| match family {
        fontdb::Family::Name(name) => {
            format!("font {name:?} is not installed; call local.media.image.fonts to list families")
        }
        _ => "no sans-serif font is installed; call local.media.image.fonts to list families"
            .to_owned(),
    })?;
    let (data, index) = database
        .with_face_data(id, |data, index| (data.to_vec(), index))
        .ok_or_else(|| "cannot read the selected font data".to_owned())?;
    FontVec::try_from_vec_and_index(data, index)
        .map_err(|error| format!("cannot parse the selected font: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 任意角度旋转先算画布再分配：超过单边上限时报错，不尝试分配。
    #[test]
    fn rotation_refuses_canvases_beyond_the_side_limit() {
        // 8192×1024 本身合法；转 10° 时宽为
        // ceil(1024·sin10° + 8192·cos10°) = ceil(177.8 + 8067.5) = 8246 > 8192。
        let wide = RgbaImage::new(MAX_IMAGE_SIDE, 1024);
        let error = rotate_rgba(wide, 10.0).unwrap_err();
        assert!(error.contains("8246x"), "{error}");
        // 90° 的倍数只交换宽高，不扩画布。
        let wide = RgbaImage::new(MAX_IMAGE_SIDE, 1024);
        let rotated = rotate_rgba(wide, 90.0).unwrap();
        assert_eq!(rotated.dimensions(), (1024, MAX_IMAGE_SIDE));
        // 小图任意角度照常可用，画布按公式扩大。
        let small = RgbaImage::new(100, 50);
        let rotated = rotate_rgba(small, 30.0).unwrap();
        assert_eq!(rotated.dimensions(), (112, 94));
    }
}
