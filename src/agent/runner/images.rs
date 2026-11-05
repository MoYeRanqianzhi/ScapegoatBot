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
        if !self.config.supports_images {
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
        let mut prompts = Vec::new();
        let mut image_count = 0usize;
        for message in messages {
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
                        && bytes.len() <= 3_500_000
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
                    && value.len() <= 3_500_000
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
                let bytes = match read_capped(response, 3_500_000).await {
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
