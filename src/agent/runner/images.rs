use super::*;
use futures_util::StreamExt;

/// 一批消息最多附带的图片数（含回复 / 转发引用里的）。
const MAX_IMAGES_PER_BATCH: usize = 4;
/// 单张入站图片最多下载 / 解码的字节数；超过视觉上限的会先缩小（见 [`vision_attachment`]）。
const MAX_INBOUND_IMAGE_BYTES: usize = 16 * 1024 * 1024;
/// 视觉输入接受的格式；其余格式先转成 JPEG。
const VISION_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

impl Runner {
    /// 把本批入站消息（及其回复 / 转发引用）里的 QQ 图片转成 image prompt。
    ///
    /// 每张图片：选出引用（`url` → `file` → `path`）→ 容器内路径经 `get_image` 解析 →
    /// 取得字节（`base64://` 直接解码，QQ CDN 地址经 HTTPS 下载）→ [`vision_attachment`]
    /// 按格式与大小整理。任何一步失败只跳过这张图并记日志；一批最多
    /// [`MAX_IMAGES_PER_BATCH`] 张。
    pub(super) async fn inbound_image_prompts(
        &self,
        messages: &[crate::qq::event::QqMessage],
    ) -> Vec<AgentMessage> {
        // 取当前槽位：热切换到不接受图片的模型后不再注入。
        if !self.provider_snapshot().supports_images {
            return Vec::new();
        }
        let client = match image_client() {
            Ok(client) => client,
            Err(error) => {
                warn!(%error, "cannot build the image download client");
                return Vec::new();
            }
        };
        // 回复/转发引用里的图片不在当前 segment 里；把引用消息里的 image segment
        // 追加成一条合成消息，后面的解析与下载逻辑保持不变。
        let mut effective: Vec<crate::qq::event::QqMessage> = messages.to_vec();
        let mut quoted_budget = MAX_IMAGES_PER_BATCH;
        for message in messages {
            if quoted_budget == 0 {
                break;
            }
            let quoted = self
                .referenced_image_segments(message, &mut quoted_budget)
                .await;
            if !quoted.is_empty() {
                let mut extra = message.clone();
                extra.segments = quoted;
                effective.push(extra);
            }
        }
        let mut prompts = Vec::new();
        for message in &effective {
            for segment in &message.segments {
                let crate::qq::event::Segment::Image { url, file, path } = segment else {
                    continue;
                };
                if prompts.len() >= MAX_IMAGES_PER_BATCH {
                    warn!("too many images in one batch; the rest were not attached");
                    return prompts;
                }
                let Some(reference) = self.resolve_image_reference(url, file, path).await else {
                    continue;
                };
                let bytes = match fetch_image(&client, &reference).await {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        warn!(%error, "cannot fetch a QQ image");
                        continue;
                    }
                };
                match vision_attachment(bytes).await {
                    Ok(attachment) => prompts.push(AgentMessage::user_with_images(
                        format!("[image from {}]", message.chat),
                        vec![attachment],
                    )),
                    Err(error) => warn!(%error, "a QQ image was not attached"),
                }
            }
        }
        prompts
    }

    /// 选出 image segment 的引用；不是 `base64://` 也不是 http(s) 的（容器内路径、文件名）
    /// 经 NapCat `get_image` 解析成 base64 或 URL。解析不了返回 `None`。
    async fn resolve_image_reference(&self, url: &str, file: &str, path: &str) -> Option<String> {
        let reference = [url, file, path]
            .into_iter()
            .find(|candidate| !candidate.is_empty())?;
        if reference.starts_with("base64://") || reference.starts_with("http") {
            return Some(reference.to_owned());
        }
        let qq = self.deps.qq.as_ref()?;
        let value = match qq
            .call("get_image", serde_json::json!({"file": file}))
            .await
        {
            Ok(value) => value,
            Err(error) => {
                warn!(%error, "cannot resolve a QQ image reference");
                return None;
            }
        };
        let field = |name: &str| {
            value
                .get(name)
                .or_else(|| value.pointer(&format!("/data/{name}")))
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.is_empty())
        };
        if let Some(data) = field("base64") {
            return Some(format!("base64://{data}"));
        }
        field("url").or_else(|| field("file")).map(str::to_owned)
    }
}

/// QQ 图片下载客户端：20 秒超时，重定向只跟到 QQ 的 HTTPS 地址。
fn image_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if is_qq_image_url(attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
}

/// 取得一张图片的字节：`base64://` 直接解码；QQ CDN 地址（`http` 升级为 `https`）下载，
/// 最多 [`MAX_INBOUND_IMAGE_BYTES`]。其余地址拒绝：入站图片只从 QQ 自己的 CDN 取。
async fn fetch_image(client: &reqwest::Client, reference: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine as _;
    if let Some(payload) = reference.strip_prefix("base64://") {
        // base64 长度约为字节数的 4/3：先按长度拒绝，不为超大的内联数据分配内存。
        if payload.len() / 4 * 3 > MAX_INBOUND_IMAGE_BYTES {
            anyhow::bail!("inline image exceeds {MAX_INBOUND_IMAGE_BYTES} bytes");
        }
        return Ok(base64::engine::general_purpose::STANDARD.decode(payload)?);
    }
    let mut url = reqwest::Url::parse(reference)?;
    if url.scheme() == "http" {
        // QQ 的图片 CDN 都提供 HTTPS；明文地址升级后再走同一条校验。
        let _ = url.set_scheme("https");
    }
    if !is_qq_image_url(&url) {
        anyhow::bail!("not a QQ image URL: {}", url.host_str().unwrap_or_default());
    }
    let response = client.get(url).send().await?.error_for_status()?;
    read_capped(response, MAX_INBOUND_IMAGE_BYTES).await
}

/// 按内容（不看文件名或响应头）识别格式，整理成视觉输入：受支持的格式且不超过
/// [`crate::media::VISION_MAX_BYTES`] 时原样附带，否则在阻塞线程池里缩成 JPEG
/// （与 `Read` 看图同一套规则）。认不出的数据报错，不会被当成 JPEG 发给模型。
async fn vision_attachment(
    bytes: Vec<u8>,
) -> Result<crate::agent::message::ImageAttachment, String> {
    use base64::Engine as _;
    let mime = image::guess_format(&bytes)
        .map(|format| format.to_mime_type().to_owned())
        .map_err(|_| "the data is not a recognised image".to_owned())?;
    let (mime, bytes) =
        if VISION_MIMES.contains(&mime.as_str()) && bytes.len() <= crate::media::VISION_MAX_BYTES {
            (mime, bytes)
        } else {
            let shrunk = crate::media::blocking(move || {
                crate::media::shrink_to_fit(
                    &bytes,
                    crate::media::VISION_MAX_SIDE,
                    crate::media::VISION_MAX_BYTES,
                )
            })
            .await
            .and_then(|result| result)?;
            ("image/jpeg".to_owned(), shrunk)
        };
    Ok(crate::agent::message::ImageAttachment {
        mime,
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

/// QQ 图片 CDN 的 HTTPS 地址（`*.qq.com`、`*.qq.com.cn`、`*.qpic.cn`）。
fn is_qq_image_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some_and(|host| {
            host.ends_with(".qq.com.cn") || host.ends_with(".qpic.cn") || host.ends_with(".qq.com")
        })
}

/// 读取响应体，超过 `limit` 字节立即报错（不先整块缓冲）。
async fn read_capped(response: reqwest::Response, limit: usize) -> anyhow::Result<Vec<u8>> {
    let mut stream = response.bytes_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if out.len() + chunk.len() > limit {
            anyhow::bail!("response exceeds the image size limit");
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

impl Runner {
    /// 取回复/转发引用消息里的图片 segment。
    ///
    /// 最多解析 3 条回复与 2 条转发，避免一条消息触发无限次 NapCat 查询。
    async fn referenced_image_segments(
        &self,
        message: &crate::qq::event::QqMessage,
        budget: &mut usize,
    ) -> Vec<crate::qq::event::Segment> {
        let Some(qq) = &self.deps.qq else {
            return Vec::new();
        };
        let mut replies = Vec::new();
        let mut forwards = Vec::new();
        for segment in &message.segments {
            match segment {
                crate::qq::event::Segment::Reply { id } => replies.push(id.clone()),
                crate::qq::event::Segment::Other { kind, data }
                    if matches!(kind.as_str(), "forward" | "json") =>
                {
                    if let Some(id) = data
                        .get("id")
                        .or_else(|| data.get("data").and_then(|value| value.get("id")))
                        .and_then(serde_json::Value::as_str)
                    {
                        forwards.push(id.to_owned());
                    }
                }
                _ => {}
            }
        }
        if let Some(reply) = message.reply_to {
            replies.push(reply.to_string());
        }
        replies.sort();
        replies.dedup();
        forwards.sort();
        forwards.dedup();

        let mut segments = Vec::new();
        for id in replies.into_iter().take(3) {
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            let params = match id.parse::<i64>() {
                Ok(message_id) => serde_json::json!({"message_id": message_id}),
                Err(_) => serde_json::json!({"message_id": id}),
            };
            match qq.call("get_msg", params).await {
                Ok(value) => segments.extend(image_segments_from_value(&value)),
                Err(error) => warn!(%error, "cannot resolve a replied message"),
            }
        }
        for id in forwards.into_iter().take(2) {
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            match qq
                .call("get_forward_msg", serde_json::json!({"id": id}))
                .await
            {
                Ok(value) => segments.extend(image_segments_from_value(&value)),
                Err(error) => warn!(%error, "cannot resolve a forwarded message"),
            }
        }
        segments
    }
}

/// 从 OneBot 消息/转发 JSON 里递归提取 image segment。
fn image_segments_from_value(value: &serde_json::Value) -> Vec<crate::qq::event::Segment> {
    fn walk(value: &serde_json::Value, out: &mut Vec<crate::qq::event::Segment>, depth: usize) {
        if depth > 16 {
            return;
        }
        match value {
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(item, out, depth + 1);
                }
            }
            serde_json::Value::Object(map) => {
                if map.get("type").and_then(serde_json::Value::as_str) == Some("image") {
                    let data = map.get("data").unwrap_or(&serde_json::Value::Null);
                    let text = |key: &str| {
                        data.get(key)
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_owned()
                    };
                    let (url, file, path) = (text("url"), text("file"), text("path"));
                    if !url.is_empty() || !file.is_empty() || !path.is_empty() {
                        out.push(crate::qq::event::Segment::Image { url, file, path });
                    }
                }
                for key in ["message", "content", "messages", "data"] {
                    if let Some(child) = map.get(key) {
                        walk(child, out, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(value, &mut out, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qq_image_urls_require_https_and_qq_domains() {
        let parse = |raw: &str| reqwest::Url::parse(raw).unwrap();
        assert!(is_qq_image_url(&parse(
            "https://multimedia.nt.qq.com.cn/a.png"
        )));
        assert!(is_qq_image_url(&parse("https://gchat.qpic.cn/a.png")));
        assert!(is_qq_image_url(&parse("https://example.qq.com/a.png")));
        assert!(!is_qq_image_url(&parse(
            "http://multimedia.nt.qq.com.cn/a.png"
        )));
        assert!(!is_qq_image_url(&parse("https://evilqq.com/a.png")));
        assert!(!is_qq_image_url(&parse("https://example.com/a.png")));
    }

    #[test]
    fn quoted_message_images_are_extracted_recursively() {
        let get_msg = serde_json::json!({
            "status": "ok",
            "data": {"message": [
                {"type": "text", "data": {"text": "look"}},
                {"type": "image", "data": {"url": "https://gchat.qpic.cn/a.png", "file": "a.png"}}
            ]}
        });
        let segments = image_segments_from_value(&get_msg);
        assert_eq!(segments.len(), 1);

        let forward = serde_json::json!({
            "data": {"messages": [
                {"message": [{"type": "image", "data": {"file": "b.png"}}]},
                {"message": [{"type": "text", "data": {"text": "hi"}}]}
            ]}
        });
        let segments = image_segments_from_value(&forward);
        assert_eq!(segments.len(), 1);
        assert!(
            matches!(&segments[0], crate::qq::event::Segment::Image { file, .. } if file == "b.png")
        );
    }

    fn encoded(format: image::ImageFormat, side: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(side, side))
            .write_to(&mut std::io::Cursor::new(&mut out), format)
            .unwrap();
        out
    }

    /// 格式按内容识别：认不出的数据不附带（以前回退成 image/jpeg，会被 API 以 400 拒绝）；
    /// 视觉不支持的格式转成 JPEG。
    #[tokio::test]
    async fn vision_attachments_sniff_the_format_and_convert_unsupported_ones() {
        let png = vision_attachment(encoded(image::ImageFormat::Png, 1))
            .await
            .unwrap();
        assert_eq!(png.mime, "image/png");
        let bmp = vision_attachment(encoded(image::ImageFormat::Bmp, 4))
            .await
            .unwrap();
        assert_eq!(bmp.mime, "image/jpeg");
        assert!(vision_attachment(b"not an image".to_vec()).await.is_err());
    }

    #[tokio::test]
    async fn only_qq_image_urls_are_fetched_and_http_is_upgraded() {
        let client = image_client().unwrap();
        for reference in [
            "https://example.com/a.png",
            "http://evilqq.com/a.png",
            "ftp://gchat.qpic.cn/a",
        ] {
            let error = fetch_image(&client, reference).await.unwrap_err();
            assert!(
                error.to_string().contains("not a QQ image URL"),
                "{reference}: {error}"
            );
        }
        // 超长的内联数据在解码之前就被拒绝。
        let huge = format!(
            "base64://{}",
            "A".repeat(MAX_INBOUND_IMAGE_BYTES / 3 * 4 + 8)
        );
        assert!(fetch_image(&client, &huge).await.is_err());
        let inline = format!(
            "base64://{}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"abc")
        );
        assert_eq!(fetch_image(&client, &inline).await.unwrap(), b"abc");
    }

    #[test]
    fn quoted_image_parser_has_a_depth_limit() {
        let mut value = serde_json::json!({"type": "image", "data": {"file": "deep.png"}});
        for _ in 0..20 {
            value = serde_json::json!({"data": value});
        }
        assert!(image_segments_from_value(&value).is_empty());
    }
}
