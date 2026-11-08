//! 图片处理的共用部分：带上限的解码、缩成 JPEG、把 CPU 密集的工作移出异步线程。
//!
//! 三处会解码不可信的图片：`Read` 看图（过大时缩小）、`API` 的
//! `local.media.image.*`、MCP 工具返回的截图。它们共用这里的上限与实现：
//! - 输入字节上限 [`MAX_INPUT_BYTES`]：读进内存之前先看大小；
//! - 宽高上限 [`MAX_IMAGE_SIDE`]：文件可能很小、声明的尺寸却巨大（解压炸弹）；
//! - 解码、缩放、编码在 [`blocking`] 里跑，不占用 tokio 的异步工作线程。

use image::DynamicImage;

/// 图片单边上限（像素）。8192² 的 RGBA 约 268 MB。
pub const MAX_IMAGE_SIDE: u32 = 8192;

/// 解码前的输入大小上限（字节）。超过它的文件不读进内存，直接报错。
pub const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;

/// 缩图默认的最长边（像素）：多数视觉模型在这个尺寸以内不再降采样。
pub const VISION_MAX_SIDE: u32 = 2000;

/// 交给视觉模型的单张图片字节上限：base64 后约 4.7 MB，低于各家 provider 的上限。
pub const VISION_MAX_BYTES: usize = 3_500_000;

/// 缩图输出的 JPEG 质量。
const JPEG_QUALITY: u8 = 85;

/// 按宽高上限解码。
///
/// `image` 默认只限制 512 MiB 的“分配估算”、不限宽高，1×100000 的细图能通过它，
/// 转个角度却要几十 GB。宽高限制是严格的，超限在分配之前就报错。
pub fn decode_limited(bytes: &[u8]) -> Result<DynamicImage, String> {
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Err(format!(
            "image is {} bytes; the limit is {MAX_INPUT_BYTES}",
            bytes.len()
        ));
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("cannot read image: {error}"))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    reader.limits(limits);
    reader
        .decode()
        .map_err(|error| format!("cannot decode image: {error}"))
}

/// 缩成不超过 `max_bytes` 的 JPEG：从 `max_side` 起每次把最长边缩到 3/4，
/// 直到放得下；缩到 [`MIN_FIT_SIDE`] 仍放不下时报错。只解码一次。
pub fn shrink_to_fit(bytes: &[u8], max_side: u32, max_bytes: usize) -> Result<Vec<u8>, String> {
    let image = decode_limited(bytes)?;
    let mut side = max_side;
    loop {
        let encoded = encode_within(&image, side)?;
        if encoded.len() <= max_bytes {
            return Ok(encoded);
        }
        if side <= MIN_FIT_SIDE {
            return Err(format!(
                "even a {side}px JPEG is {} bytes; the limit is {max_bytes}",
                encoded.len()
            ));
        }
        side = (side / 4 * 3).max(MIN_FIT_SIDE);
    }
}

/// [`shrink_to_fit`] 缩到这个最长边仍放不下就放弃：再小的图对模型已没有意义。
const MIN_FIT_SIDE: u32 = 512;

/// 缩到 `max_side` 以内（不放大）并编码成 JPEG。
fn encode_within(image: &DynamicImage, max_side: u32) -> Result<Vec<u8>, String> {
    // 已经在范围内的图保持原尺寸，只换编码；`thumbnail` 会把小图放大到边界，不能直接用。
    let shrunk = if image.width() <= max_side && image.height() <= max_side {
        image.to_rgb8()
    } else {
        image.thumbnail(max_side, max_side).to_rgb8()
    };
    let mut encoded = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, JPEG_QUALITY)
        .encode_image(&shrunk)
        .map_err(|error| format!("cannot encode JPEG: {error}"))?;
    Ok(encoded)
}

/// 在阻塞线程池里运行 `work`，等它结束。
///
/// 任务 panic 或运行时关闭时返回错误文本，而不是把 panic 传到调用方的异步任务里。
pub async fn blocking<T, F>(work: F) -> Result<T, String>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| format!("image task failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 编码一张 `width`×`height` 的 PNG。
    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = DynamicImage::ImageRgb8(image::RgbImage::new(width, height));
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn oversized_dimensions_are_refused_before_allocation() {
        // 1×(上限+1) 的细图：文件只有几百字节。
        let tall = png(1, MAX_IMAGE_SIDE + 1);
        assert!(tall.len() < 64 * 1024);
        assert!(decode_limited(&tall).is_err());
        assert!(decode_limited(&png(16, 16)).is_ok());
    }

    #[test]
    fn shrinking_keeps_aspect_and_never_enlarges() {
        let big = shrink_to_fit(&png(400, 100), 200, usize::MAX).unwrap();
        let decoded = image::load_from_memory(&big).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (200, 50));
        let small = shrink_to_fit(&png(40, 10), 200, usize::MAX).unwrap();
        let decoded = image::load_from_memory(&small).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (40, 10));
    }

    #[test]
    fn shrinking_steps_down_until_the_jpeg_fits() {
        // 噪声几乎不可压缩：JPEG 大小大致与像素数成正比。
        let mut raw = image::RgbImage::new(1200, 1200);
        let mut seed = 0x9e37_79b9_u32;
        for pixel in raw.pixels_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let [r, g, b, _] = seed.to_le_bytes();
            *pixel = image::Rgb([r, g, b]);
        }
        let mut noisy = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(raw)
            .write_to(&mut noisy, image::ImageFormat::Png)
            .unwrap();
        let noisy = noisy.into_inner();
        let full = shrink_to_fit(&noisy, 1200, usize::MAX).unwrap();
        let fitted = shrink_to_fit(&noisy, 1200, full.len() / 2).unwrap();
        assert!(fitted.len() <= full.len() / 2);
        let decoded = image::load_from_memory(&fitted).unwrap();
        assert!(decoded.width() < 1200 && decoded.width() >= MIN_FIT_SIDE);
        // 最小尺寸都放不下时报错。
        assert!(shrink_to_fit(&noisy, 1200, 1_000).is_err());
    }

    #[tokio::test]
    async fn blocking_work_reports_panics_as_errors() {
        assert_eq!(blocking(|| 2 + 2).await, Ok(4));
        let failed: Result<(), String> = blocking(|| panic!("boom")).await;
        assert!(failed.unwrap_err().contains("image task failed"));
    }
}
