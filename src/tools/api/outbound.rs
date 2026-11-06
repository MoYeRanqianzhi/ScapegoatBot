//! 发往 NapCat 的文件引用：NapCat 只会收到 `base64://`。
//!
//! NapCat 解析 `file` / `url` 一类字段时（NapCatQQ
//! `packages/napcat-common/src/file.ts` 的 `checkUriType`）：
//! - 字符串只要是**宿主机**上存在的路径，就直接读取那个文件；
//! - `file://` 同样指宿主机路径；
//! - `http(s)://` 由 NapCat 在它自己的网络位置下载。
//!
//! 模型手里只有 VFS 路径，而宿主机上有 `~/.sbot/auth.json`、`~/.sbot/key`。
//! 如果把模型写的字符串原样交给 NapCat，一次提示注入就能让 bot 把宿主机文件
//! 发进群里，或让 NapCat 去请求内网地址。所以在调用 NapCat 之前统一改写：
//!
//! - `base64://`：原样保留（内容由调用方给出，不碰宿主机）；
//! - VFS 绝对路径（如 `/tmp/a.png`）：本进程从 VFS 读出，换成 `base64://`；
//! - `http(s)://`：本进程按网络策略下载（只连公网、逐跳检查重定向、限制大小），
//!   换成 `base64://`；
//! - 其他一律拒绝：`file://`、相对路径、Windows 路径、`data:` 等。
//!
//! 改写只作用于发给 NapCat 的副本；发送账本、归档与门禁看到的仍是模型写的引用。

use base64::Engine;
use serde_json::{Value, json};

use crate::tools::net::{NetPolicy, read_limited};
use crate::vfs::Vfs;
use crate::vfs::path::VfsPath;
use crate::vfs::sqlite::VfsStore;

/// 一次调用里所有内联内容（解码后）的总字节上限。
///
/// 内联内容会以 base64 形式出现在同一条 WebSocket 帧里（约放大 4/3），
/// 并在本进程里整体驻留内存；20 MiB 足够图片、语音与小文件，又不会撑大帧。
pub(super) const MAX_INLINE_BYTES: u64 = 20 * 1024 * 1024;

/// 需要内联的媒体消息段类型。
const MEDIA_SEGMENTS: &[&str] = &["image", "record", "video", "file"];

/// 媒体段的内容来源字段，顺序与 NapCat 一致：它取第一个非空的
/// （`packages/napcat-onebot/api/msg.ts` 的 `handleOb11FileLikeMessage`：
/// `[url, file, path].find(...)`）。
const SOURCE_KEYS: &[&str] = &["url", "file", "path"];

/// 视频封面；NapCat 单独对它调用 `uriToLocalFile`。
const THUMB_KEY: &str = "thumb";

/// 非媒体段里一旦出现就说明在引用文件的字段；这类段不支持，直接拒绝。
const FOREIGN_FILE_KEYS: &[&str] = &["file", "path"];

/// 不在 `message` 里、而是直接作为参数引用文件的 action 与字段。
///
/// 测试 `every_file_param_in_the_registry_is_inlined` 保证它与注册表一致。
pub(super) const FILE_PARAMS: &[(&str, &str)] = &[
    ("napcat.upload_group_file", "file"),
    ("napcat.upload_private_file", "file"),
    ("napcat.set_qq_avatar", "file"),
];

/// 合并转发（`node`）允许嵌套的层数。
const MAX_NODE_DEPTH: usize = 4;

/// 返回一份可以交给 NapCat 的参数副本：所有文件引用都已换成 `base64://`。
///
/// `params` 不被修改；调用方继续用它记账与归档。
pub(super) async fn inline_references(
    action: &str,
    params: &Value,
    vfs: &VfsStore,
    net: &NetPolicy,
) -> Result<Value, String> {
    // 改写对象是副本，原参数留给账本与归档。
    let mut wire = params.clone();
    // 需要内联的字段，以 JSON Pointer 记录，第二步统一改写。
    let mut pointers = Vec::new();
    // 消息段：可能是数组，也可能已被 `sanitize_send_params` 规范成数组。
    if let Some(message) = wire.get_mut("message") {
        collect_segment_references(message, "/message", 0, &mut pointers)?;
    }
    // 直接引用文件的参数。
    for (name, field) in FILE_PARAMS {
        if *name == action && wire.get(*field).is_some() {
            pointers.push(format!("/{field}"));
        }
    }

    // 剩余可内联字节数；所有引用共享同一个上限。
    let mut remaining = MAX_INLINE_BYTES;
    for pointer in pointers {
        // 收集阶段只记录了字符串字段，这里一定能取到。
        let Some(slot) = wire.pointer_mut(&pointer) else {
            continue;
        };
        let Some(raw) = slot.as_str() else {
            return Err(format!("{pointer} must be a string reference"));
        };
        let inlined = inline_one(raw, vfs, net, &mut remaining)
            .await
            .map_err(|error| format!("{pointer}: {error}"))?;
        *slot = Value::String(inlined);
    }
    Ok(wire)
}

/// 遍历消息段，记录要内联的字段，并把 `node` 里的字符串正文规范成文本段。
fn collect_segment_references(
    message: &mut Value,
    base: &str,
    depth: usize,
    out: &mut Vec<String>,
) -> Result<(), String> {
    // 字符串消息已在发送前规范成文本段；这里仍是字符串说明没有可引用的文件。
    let Some(segments) = message.as_array_mut() else {
        return Ok(());
    };
    for (index, segment) in segments.iter_mut().enumerate() {
        // 段类型；缺失时按未知类型处理。
        let kind = segment
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        // 段的 JSON Pointer 前缀，例如 `/message/0`。
        let prefix = format!("{base}/{index}");
        let Some(data) = segment.get_mut("data").and_then(Value::as_object_mut) else {
            continue;
        };
        if MEDIA_SEGMENTS.contains(&kind.as_str()) {
            // 按 NapCat 的顺序选出唯一来源，删掉其余来源字段，只留 `file`：
            // NapCat 拿不到任何未经改写的引用。
            let mut source = None;
            for key in SOURCE_KEYS {
                match data.get(*key) {
                    Some(Value::String(text)) if !text.trim().is_empty() => {
                        source = Some(text.clone());
                        break;
                    }
                    // 空串、null 与缺失：NapCat 同样跳过。
                    Some(Value::String(_)) | Some(Value::Null) | None => {}
                    Some(_) => {
                        return Err(format!("{prefix}/data/{key} must be a string reference"));
                    }
                }
            }
            let Some(source) = source else {
                return Err(format!(
                    "{prefix}: a {kind} segment needs `file` or `url` (a VFS path, a public \
                     http(s) URL, or base64:// data)"
                ));
            };
            for key in SOURCE_KEYS {
                data.remove(*key);
            }
            data.insert("file".to_owned(), Value::String(source));
            out.push(format!("{prefix}/data/file"));
            // 封面同样会被 NapCat 读取；空值 NapCat 视为没有封面。
            match data.get(THUMB_KEY) {
                Some(Value::String(text)) if !text.trim().is_empty() => {
                    out.push(format!("{prefix}/data/{THUMB_KEY}"));
                }
                Some(Value::String(_)) | Some(Value::Null) | None => {}
                Some(_) => {
                    return Err(format!(
                        "{prefix}/data/{THUMB_KEY} must be a string reference"
                    ));
                }
            }
        } else if kind == "node" {
            if depth >= MAX_NODE_DEPTH {
                return Err(format!(
                    "{prefix}: forward nodes nest deeper than {MAX_NODE_DEPTH} levels"
                ));
            }
            // NapCat 会把字符串正文当作 CQ 码解析；先变成纯文本段，避免借道注入文件段。
            if let Some(text) = data.get("content").and_then(Value::as_str) {
                let text = text.to_owned();
                data.insert(
                    "content".to_owned(),
                    json!([{ "type": "text", "data": { "text": text } }]),
                );
            }
            if let Some(content) = data.get_mut("content") {
                collect_segment_references(
                    content,
                    &format!("{prefix}/data/content"),
                    depth + 1,
                    out,
                )?;
            }
        } else if let Some(key) = FOREIGN_FILE_KEYS
            .iter()
            .find(|key| data.contains_key(**key))
        {
            // 其他段类型本身不承载文件；带文件字段的未知段无法安全改写。
            return Err(format!(
                "{prefix}: segment type {kind:?} cannot carry a `{key}` reference; \
                 use an image/record/video/file segment"
            ));
        }
    }
    Ok(())
}

/// 把一个引用换成 `base64://`；`remaining` 是本次调用剩余的内联额度。
async fn inline_one(
    raw: &str,
    vfs: &VfsStore,
    net: &NetPolicy,
    remaining: &mut u64,
) -> Result<String, String> {
    if let Some(payload) = raw.strip_prefix("base64://") {
        // 已经是内容本身；按解码后的大小计入额度。
        charge(remaining, (payload.len() as u64) / 4 * 3)?;
        return Ok(raw.to_owned());
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        let bytes = download(raw, net, *remaining).await?;
        charge(remaining, bytes.len() as u64)?;
        return Ok(encode(&bytes));
    }
    if raw.starts_with('/') {
        // 以 `/` 开头的只可能是 VFS 路径：宿主机路径从不交给 NapCat。
        let path =
            VfsPath::parse(raw).map_err(|error| format!("invalid VFS path {raw:?}: {error}"))?;
        let meta = vfs.stat(&path).map_err(|error| error.to_string())?;
        if !meta.kind.is_file() {
            return Err(format!("{raw} is not a file"));
        }
        // 先看大小再读，避免把超大 blob 读进内存。
        charge(remaining, meta.size)?;
        let bytes = vfs.read_bytes(&path).map_err(|error| error.to_string())?;
        return Ok(encode(&bytes));
    }
    Err(format!(
        "{raw:?} is not a VFS absolute path, a public http(s) URL, or base64:// data; \
         host paths and file:// URIs are never sent to NapCat"
    ))
}

/// 按网络策略下载一个 URL；只接受成功响应，大小不超过 `limit`。
async fn download(raw: &str, net: &NetPolicy, limit: u64) -> Result<Vec<u8>, String> {
    let url = net.check_url(raw)?;
    let client = net.client()?;
    let response = net
        .send(&client, url, reqwest::Method::GET, Default::default(), None)
        .await?;
    if !response.status().is_success() {
        return Err(format!(
            "downloading {raw} returned HTTP {}",
            response.status()
        ));
    }
    let (bytes, _) = read_limited(response, limit.min(net.max_bytes)).await?;
    Ok(bytes)
}

/// 从额度里扣除 `bytes`；超额时报错。
fn charge(remaining: &mut u64, bytes: u64) -> Result<(), String> {
    if bytes > *remaining {
        return Err(format!(
            "inline content exceeds the {MAX_INLINE_BYTES}-byte limit for one call"
        ));
    }
    *remaining -= bytes;
    Ok(())
}

/// 编码成 NapCat 认识的 `base64://` 形式。
fn encode(bytes: &[u8]) -> String {
    format!(
        "base64://{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// 发往 NapCat 的媒体下载策略：只连公网，大小与单次内联额度一致。
pub(super) fn media_net_policy() -> NetPolicy {
    NetPolicy {
        max_bytes: MAX_INLINE_BYTES,
        timeout_secs: 30,
        ..NetPolicy::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::NodeKind;

    /// 一个带 `/tmp/a.png`（二进制）与 `/tmp/note.txt`（文本）的临时 VFS。
    fn vfs() -> (VfsStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = VfsStore::open(
            std::sync::Arc::new(crate::storage::Database::open_in_memory().unwrap()),
            std::sync::Arc::new(crate::storage::crypto::MasterKey::from_bytes([3u8; 32])),
            dir.path().to_path_buf(),
            crate::vfs::quota::Limits::default(),
        )
        .unwrap();
        store
            .write_binary(
                &VfsPath::parse("/tmp/a.png").unwrap(),
                b"\x89PNG-bytes",
                NodeKind::Image,
            )
            .unwrap();
        store
            .write_text(&VfsPath::parse("/tmp/note.txt").unwrap(), "hello", None)
            .unwrap();
        (store, dir)
    }

    fn b64(bytes: &[u8]) -> String {
        format!(
            "base64://{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    }

    async fn inline(action: &str, params: Value) -> Result<Value, String> {
        let (store, _dir) = vfs();
        inline_references(action, &params, &store, &media_net_policy()).await
    }

    #[tokio::test]
    async fn vfs_paths_become_base64_and_other_sources_are_dropped() {
        let params = json!({"group_id": 1, "message": [
            {"type": "text", "data": {"text": "hi"}},
            {"type": "image", "data": {"file": "/tmp/a.png", "path": "/etc/passwd"}}
        ]});
        let wire = inline("napcat.send_group_msg", params.clone())
            .await
            .unwrap();
        // 只剩一个来源，而且是 VFS 内容本身；`path` 被删掉，NapCat 读不到宿主机文件。
        assert_eq!(
            wire["message"][1]["data"],
            json!({"file": b64(b"\x89PNG-bytes")})
        );
        // 文本段原样保留，原参数不被改动（账本与归档仍看到 VFS 路径）。
        assert_eq!(wire["message"][0], params["message"][0]);
        assert_eq!(params["message"][1]["data"]["file"], "/tmp/a.png");
    }

    #[tokio::test]
    async fn text_files_can_be_sent_as_files() {
        let params = json!({"user_id": 2, "file": "/tmp/note.txt", "name": "note.txt"});
        let wire = inline("napcat.upload_private_file", params).await.unwrap();
        assert_eq!(wire["file"], b64(b"hello"));
        assert_eq!(wire["name"], "note.txt");
    }

    #[tokio::test]
    async fn host_paths_and_file_uris_are_rejected() {
        for raw in [
            "file:///C:/Users/me/.sbot/auth.json",
            "file:///root/.sbot/key",
            "config/onebot11.json",
            r"C:\Users\me\.sbot\key",
            "data:image/png;base64,AAAA",
        ] {
            let params = json!({"group_id": 1, "message": [
                {"type": "file", "data": {"file": raw}}
            ]});
            let error = inline("napcat.send_group_msg", params).await.unwrap_err();
            assert!(error.contains("/message/0/data/file"), "{raw}: {error}");
        }
        // 参数级的文件引用同样拒绝。
        let error = inline(
            "napcat.upload_group_file",
            json!({"group_id": 1, "file": "file:///etc/passwd", "name": "x"}),
        )
        .await
        .unwrap_err();
        assert!(error.starts_with("/file:"), "{error}");
    }

    #[tokio::test]
    async fn url_takes_precedence_like_napcat_and_private_hosts_are_refused() {
        // NapCat 先看 url：这里 url 指向回环地址，即使 file 是合法 VFS 路径也必须拒绝。
        let params = json!({"group_id": 1, "message": [
            {"type": "image", "data": {"url": "http://127.0.0.1:6099/api", "file": "/tmp/a.png"}}
        ]});
        let error = inline("napcat.send_group_msg", params).await.unwrap_err();
        assert!(error.contains("not public"), "{error}");
    }

    #[tokio::test]
    async fn base64_passes_through_and_counts_toward_the_limit() {
        let params = json!({"group_id": 1, "message": [
            {"type": "record", "data": {"file": "base64://AAAA"}}
        ]});
        let wire = inline("napcat.send_group_msg", params.clone())
            .await
            .unwrap();
        assert_eq!(wire, params);

        let huge = format!(
            "base64://{}",
            "A".repeat((MAX_INLINE_BYTES as usize / 3 + 1) * 4)
        );
        let params = json!({"group_id": 1, "message": [
            {"type": "record", "data": {"file": huge}}
        ]});
        let error = inline("napcat.send_group_msg", params).await.unwrap_err();
        assert!(error.contains("limit"), "{error}");
    }

    #[tokio::test]
    async fn forward_nodes_are_walked_and_string_content_is_neutralized() {
        let params = json!({"group_id": 1, "message": [
            {"type": "node", "data": {"name": "a", "uin": "1", "content": [
                {"type": "image", "data": {"file": "/tmp/a.png"}}
            ]}},
            {"type": "node", "data": {"name": "b", "uin": "1",
                "content": "[CQ:file,file=file:///root/.sbot/key]"}}
        ]});
        let wire = inline("napcat.send_group_msg", params).await.unwrap();
        assert_eq!(
            wire["message"][0]["data"]["content"][0]["data"]["file"],
            b64(b"\x89PNG-bytes")
        );
        // CQ 字符串变成纯文本段，NapCat 不会再把它解析成文件段。
        assert_eq!(
            wire["message"][1]["data"]["content"],
            json!([{"type": "text", "data": {"text": "[CQ:file,file=file:///root/.sbot/key]"}}])
        );
    }

    #[tokio::test]
    async fn unknown_segments_with_file_fields_are_rejected() {
        let params = json!({"group_id": 1, "message": [
            {"type": "onlinefile", "data": {"file": "/tmp/a.png"}}
        ]});
        let error = inline("napcat.send_group_msg", params).await.unwrap_err();
        assert!(error.contains("onlinefile"), "{error}");
    }

    /// 注册表里任何声明了文件类参数的 action 都必须出现在 [`FILE_PARAMS`] 里，
    /// 否则新增的上传类 action 会绕过内联把字符串直接交给 NapCat。
    #[test]
    fn every_file_param_in_the_registry_is_inlined() {
        for spec in crate::actions::specs() {
            if spec.namespace() != "napcat" {
                continue;
            }
            let Some(params) = spec.params.as_object() else {
                continue;
            };
            for field in ["file", "path", "url", "image", "thumb"] {
                if params.contains_key(field) {
                    assert!(
                        FILE_PARAMS.contains(&(spec.action, field)),
                        "{} declares `{field}` but outbound::FILE_PARAMS does not inline it",
                        spec.action
                    );
                }
            }
        }
    }
}
