//! `API` 的 `local.media.image.*` 分支：VFS 内图片的检查、变换、编码与文字。
//!
//! 只读写 VFS，不产生对外影响；字体来自系统字体库（`fontdb`），不内置字体。
//! 解码、变换、编码都是 CPU 密集的同步工作，整个动作在阻塞线程池里跑
//! （[`crate::media::blocking`]），解码上限与 `Read` 看图、MCP 截图一致。

use std::collections::HashSet;

use ab_glyph::{Font, FontRef, PxScale};
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
    // 渲染器一个字符画一个字形、不排版：换行符和制表符不会换行或对齐，只会被画成缺字形的
    // 方块。直接拒绝，让调用方一行一次地画。
    if text.chars().any(char::is_control) {
        return ToolResult::error(
            "text must be a single line without control characters; draw each line with its own call",
        );
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
    let mut database = fontdb::Database::new();
    database.load_system_fonts();
    let font = match pick_font(&database, family, text) {
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
    let scale = PxScale::from(size as f32);
    let drawn = with_font(&database, font, |font| {
        draw_text_mut(&mut rgba, color, x, y, scale, font, text);
    });
    if drawn.is_none() {
        return ToolResult::error("cannot read the selected font");
    }
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

/// 选出画 `text` 用的字体。
///
/// 渲染没有字体回退：整段文字只用一个字体画，字体里没有的字符会被画成“豆腐块”
/// （`.notdef`）或者什么都不画，动作却照样成功。所以先核对字形，画不全就报错：
///
/// - 指定了 `family`：只用它。没安装、或有字符画不出都报错，错误里列出画不出的字符；
/// - 没指定：先试系统默认的无衬线字体；它没安装（Linux 上 fontconfig 配的首选字体经常没装）
///   或画不全（拉丁字体画不了中文）时，换成第一个画得全的已安装字体，正体、常规字重优先。
fn pick_font(
    database: &fontdb::Database,
    family: Option<&str>,
    text: &str,
) -> Result<fontdb::ID, String> {
    // 每个字符只核对一次；保持出现顺序，报错时按原文的先后列出。
    let mut chars: Vec<char> = Vec::new();
    for ch in text.chars() {
        if !chars.contains(&ch) {
            chars.push(ch);
        }
    }
    let wanted = [family.map_or(fontdb::Family::SansSerif, fontdb::Family::Name)];
    let preferred = database.query(&fontdb::Query {
        families: &wanted,
        ..Default::default()
    });

    if let Some(name) = family {
        let id = preferred.ok_or_else(|| {
            format!("font {name:?} is not installed; call local.media.image.fonts to list families")
        })?;
        let missing: Vec<char> = with_font(database, id, |font| {
            chars
                .iter()
                .copied()
                .filter(|&ch| !can_draw(font, ch))
                .collect()
        })
        .ok_or_else(|| format!("cannot read font {name:?}"))?;
        if !missing.is_empty() {
            return Err(format!(
                "font {name:?} cannot draw {}; choose another family \
                 (local.media.image.fonts) or omit font to pick one automatically",
                list_chars(&missing)
            ));
        }
        return Ok(id);
    }

    // 默认字体排最前；其余按“正体、字重接近 400”排，同分保持系统字体库里的顺序（稳定排序）。
    let mut rest: Vec<&fontdb::FaceInfo> = database.faces().collect();
    rest.sort_by_key(|face| {
        (
            face.style != fontdb::Style::Normal,
            face.weight.0.abs_diff(fontdb::Weight::NORMAL.0),
        )
    });
    for id in preferred.into_iter().chain(rest.iter().map(|face| face.id)) {
        // 读不出、解析不了的字体文件当作画不了，跳过：一个坏文件不该挡住其余字体。
        let covers = with_font(database, id, |font| {
            !is_placeholder(font) && chars.iter().all(|&ch| can_draw(font, ch))
        });
        if covers == Some(true) {
            return Ok(id);
        }
    }
    Err(if database.is_empty() {
        "no fonts are installed on this system, so text cannot be drawn".to_owned()
    } else {
        "no installed font can draw every character of this text; split it and draw each part \
         with a font that covers it (local.media.image.fonts lists families)"
            .to_owned()
    })
}

/// 用一个已安装的字体执行 `work`；字体文件读不出或解析不了时为 `None`。
///
/// 字体数据只借用（文件由 `fontdb` 内存映射），不复制：中文字体一个就有几十 MB，没指定字体
/// 时还要挨个核对系统里的字体。
fn with_font<T>(
    database: &fontdb::Database,
    id: fontdb::ID,
    work: impl FnOnce(&FontRef<'_>) -> T,
) -> Option<T> {
    database
        .with_face_data(id, |data, index| {
            FontRef::try_from_slice_and_index(data, index)
                .ok()
                .map(|font| work(&font))
        })
        .flatten()
}

/// 这个字体能不能画出 `ch`。
///
/// 要有这个字符的字形（没有时渲染器画 0 号字形 `.notdef`），并且字形有轮廓：有字形却没有
/// 轮廓的是位图 / 彩色字体（例如 Noto Color Emoji），渲染器只会画轮廓。空白字符只要求有
/// 字形，它本来就不画，只占宽度。
fn can_draw(font: &FontRef<'_>, ch: char) -> bool {
    let glyph = font.glyph_id(ch);
    glyph.0 != 0 && (ch.is_whitespace() || font.outline(glyph).is_some())
}

/// 占位字体（Unicode Last Resort 一类，macOS 自带 `LastResort.otf`）：给每个码位都配一个
/// “这是哪个区段的字符”的示意字形，连永远不会分配字符的 U+10FFFF 都有。用它画出来的不是
/// 文字，自动选字体时跳过；点名要用它的照常给。
fn is_placeholder(font: &FontRef<'_>) -> bool {
    font.glyph_id('\u{10FFFF}').0 != 0
}

/// 报错时列出画不出的字符，例如 `'你', '\u{fe0f}'`（看不见的字符按转义写）；太多时只列前几个。
fn list_chars(chars: &[char]) -> String {
    const SHOWN: usize = 12;
    let mut listed = chars
        .iter()
        .take(SHOWN)
        .map(|ch| format!("{ch:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    if chars.len() > SHOWN {
        listed.push_str(&format!(" and {} more", chars.len() - SHOWN));
    }
    listed
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

    fn system_fonts() -> fontdb::Database {
        let mut database = fontdb::Database::new();
        database.load_system_fonts();
        database
    }

    /// 没有任何字体（最小化安装的服务器）：两种选法都报错，并且说得出原因。
    #[test]
    fn without_fonts_text_is_an_error() {
        let empty = fontdb::Database::new();
        let error = pick_font(&empty, None, "Hi").unwrap_err();
        assert!(error.contains("no fonts are installed"), "{error}");
        let error = pick_font(&empty, Some("Arial"), "Hi").unwrap_err();
        assert!(error.contains("is not installed"), "{error}");
    }

    /// 自动选出的字体一定画得出整段文字。装了哪些字体因机器而异，所以只断言这条不变量：
    /// 要么报错，要么选中的字体每个字符都有可画的字形（CI 的 Linux 机器上排在最前的字体
    /// 没有拉丁字形，以前照样被选中，画出来一个像素都没变）。
    #[test]
    fn an_automatically_picked_font_covers_the_whole_text() {
        let database = system_fonts();
        for text in ["Hi 42", "你好，世界", "mixed 中文 text"] {
            let Ok(id) = pick_font(&database, None, text) else {
                continue;
            };
            let covered = with_font(&database, id, |font| {
                text.chars().all(|ch| can_draw(font, ch))
            });
            assert_eq!(covered, Some(true), "{text:?}");
        }
    }

    /// U+10FFFF 是非字符，正常字体都没有它的字形（有的是占位字体，自动选择时跳过）：自动
    /// 选不出字体；指定字体时报错并点出这个字符，而不是画一个豆腐块了事。
    #[test]
    fn a_character_no_font_has_is_refused() {
        let database = system_fonts();
        let text = "a\u{10FFFF}";
        assert!(pick_font(&database, None, text).is_err());

        // 取一个确实装着、画得出拉丁字母的字体来指定；机器上没有这样的字体时没有可测的。
        let Ok(id) = pick_font(&database, None, "a") else {
            return;
        };
        let family = database.face(id).unwrap().families[0].0.clone();
        let error = pick_font(&database, Some(&family), text).unwrap_err();
        assert!(error.contains("cannot draw '\\u{10ffff}'"), "{error}");
        assert!(pick_font(&database, Some(&family), "a").is_ok());
    }

    #[test]
    fn long_lists_of_missing_characters_are_cut_short() {
        assert_eq!(list_chars(&['你', '\u{fe0f}']), "'你', '\\u{fe0f}'");
        let many: Vec<char> = ('a'..='z').collect();
        let listed = list_chars(&many);
        assert!(listed.starts_with("'a', 'b', "), "{listed}");
        assert!(listed.ends_with("'l' and 14 more"), "{listed}");
    }
}
