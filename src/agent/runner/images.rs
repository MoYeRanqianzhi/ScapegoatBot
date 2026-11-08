use super::*;
use futures_util::StreamExt;

impl Runner {
    /// 把本批入站消息里的 QQ 图片转成 image prompt。
    ///
    /// NapCat 的 image segment 有 `url`、`file`（文件路径/URL/base64）和 `path`。
    /// `url` 为空时先用 `file` 直接判断；容器内路径再调用 `get_image` 解析成 URL。
    pub(super) async fn inbound_image_prompts(
        &self,
        messages: &[crate::qq::event::QqMessage],
    ) -> Vec<AgentMessage> {
        // 取当前槽位：热切换到不接受图片的模型后不再注入。
        if !self.provider_snapshot().supports_images {
            return Vec::new();
        }
        use base64::Engine as _;
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if is_qq_image_url(attempt.url()) {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                warn!(%error, "cannot build the image download client");
                return Vec::new();
            }
        };
        // 回复/转发引用里的图片不在当前 segment 里；把引用消息里的 image segment
        // 追加成一条合成消息，后面的解析与下载逻辑保持不变。
        let mut effective: Vec<crate::qq::event::QqMessage> = messages.to_vec();
        let mut quoted_budget = 4usize;
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
        let mut image_count = 0usize;
        for message in &effective {
            for segment in &message.segments {
                if image_count >= 4 {
                    warn!("more than 4 images in one batch; extra images were not attached");
                    break;
                }
                let crate::qq::event::Segment::Image { url, file, path } = segment else {
                    continue;
                };
                let mut reference = if !url.is_empty() {
                    url.clone()
                } else if !file.is_empty() {
                    file.clone()
                } else {
                    path.clone()
                };
                if reference.starts_with("base64://") {
                    let payload = reference.trim_start_matches("base64://");
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(payload)
                        && bytes.len() <= crate::media::VISION_MAX_BYTES
                    {
                        let mime = mime_for_bytes(&bytes);
                        image_count += 1;
                        prompts.push(AgentMessage::user_with_images(
                            format!("[image from {}]", message.chat),
                            vec![crate::agent::message::ImageAttachment {
                                mime,
                                data_base64: payload.to_owned(),
                            }],
                        ));
                    }
                    continue;
                }
                if !reference.starts_with("http") {
                    let Some(qq) = &self.deps.qq else {
                        continue;
                    };
                    match qq
                        .call("get_image", serde_json::json!({"file": file}))
                        .await
                    {
                        Ok(value) => {
                            if let Some(data) = value
                                .get("base64")
                                .or_else(|| value.pointer("/data/base64"))
                                .and_then(serde_json::Value::as_str)
                            {
                                reference = format!("base64://{data}");
                            } else {
                                reference = value
                                    .get("url")
                                    .or_else(|| value.pointer("/data/url"))
                                    .or_else(|| value.get("file"))
                                    .or_else(|| value.pointer("/data/file"))
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned();
                            }
                        }
                        Err(error) => {
                            warn!(%error, "cannot resolve a QQ image reference");
                            continue;
                        }
                    }
                }
                if reference.starts_with("base64://")
                    && let Ok(value) = base64::engine::general_purpose::STANDARD
                        .decode(reference.trim_start_matches("base64://"))
                    && value.len() <= crate::media::VISION_MAX_BYTES
                    && matches!(
                        mime_for_bytes(&value).as_str(),
                        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                    )
                {
                    let mime = mime_for_bytes(&value);
                    image_count += 1;
                    prompts.push(AgentMessage::user_with_images(
                        format!("[image from {}]", message.chat),
                        vec![crate::agent::message::ImageAttachment {
                            mime,
                            data_base64: base64::engine::general_purpose::STANDARD.encode(&value),
                        }],
                    ));
                    continue;
                }
                let Ok(parsed) = reqwest::Url::parse(&reference) else {
                    continue;
                };
                if !matches!(parsed.scheme(), "http" | "https") {
                    continue;
                }
                let Some(host) = parsed.host_str() else {
                    continue;
                };
                if !(host.ends_with(".qq.com.cn")
                    || host.ends_with(".qpic.cn")
                    || host.ends_with(".qq.com"))
                {
                    continue;
                }
                let response = match client.get(parsed).send().await {
                    Ok(response) => response,
                    Err(error) => {
                        warn!(%error, "cannot download a QQ image");
                        continue;
                    }
                };
                let header_mime = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .to_owned();
                let bytes = match read_capped(response, crate::media::VISION_MAX_BYTES).await {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        warn!(%error, "cannot read a QQ image");
                        continue;
                    }
                };
                let mime = if matches!(
                    header_mime.as_str(),
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                ) {
                    header_mime
                } else {
                    mime_for_bytes(&bytes)
                };
                if !matches!(
                    mime.as_str(),
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                ) {
                    continue;
                }
                prompts.push(AgentMessage::user_with_images(
                    format!("[image from {}]", message.chat),
                    vec![crate::agent::message::ImageAttachment {
                        mime,
                        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                    }],
                ));
            }
        }
        prompts
    }
}

fn is_qq_image_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some_and(|host| {
            host.ends_with(".qq.com.cn") || host.ends_with(".qpic.cn") || host.ends_with(".qq.com")
        })
}

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

/// 从图片字节猜 MIME；只用于 QQ 图片附件。
fn mime_for_bytes(bytes: &[u8]) -> String {
    image::guess_format(bytes)
        .map(|format| format.to_mime_type().to_owned())
        .unwrap_or_else(|_| "image/jpeg".to_owned())
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

    #[test]
    fn mime_sniffs_known_formats_and_falls_back() {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(1, 1))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert_eq!(mime_for_bytes(&png), "image/png");

        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(1, 1))
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        assert_eq!(mime_for_bytes(&jpeg), "image/jpeg");
        assert_eq!(mime_for_bytes(b"not an image"), "image/jpeg");
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
