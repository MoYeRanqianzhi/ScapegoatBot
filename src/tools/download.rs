//! `Download` 工具：把 HTTP/HTTPS 内容写进 VFS。
//!
//! 设计依据：设计 §5.2 / §6.4。
//!
//! - 强制 SSRF 防护、域名允许/阻止、重定向上限与 `max_bytes`；
//! - 流式写入：响应体每攒满约 1 MiB 就交给阻塞线程加密暂存
//!   （[`crate::vfs::sqlite::BlobStage`]），内存里不留整个文件，超限即中止；
//!   提交同样在阻塞线程里做，不占用异步工作线程；
//! - 文本类型（`text/*`、JSON、XML、JS）且不超过文本上限、是合法 UTF-8 时存成文本节点
//!   （可 `Read` / `Edit`），否则存成二进制；
//! - 返回 VFS 路径、字节数、MIME、节点类型与 hash；发送文件用 `API`，不在这里做。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::transcript::ToolResult;
use crate::vfs::sqlite::{BlobStage, VfsStore};
use crate::vfs::{NodeKind, NodeMeta, Vfs, VfsError, VfsPath, guess_mime};

use super::net::{LimitedBody, NetPolicy};

/// 交给暂存线程的批量大小：与 VFS 分块一致，每个分块切一次线程。
const STAGE_BATCH_BYTES: usize = 1024 * 1024;
use super::{Permission, Tool, ToolContext, arg_opt_u64, arg_str, object_schema};

/// `Download` 工具。
pub struct DownloadTool {
    vfs: Arc<VfsStore>,
    client: reqwest::Client,
    policy: NetPolicy,
}

impl DownloadTool {
    pub(crate) fn new(vfs: Arc<VfsStore>, policy: NetPolicy) -> Result<Self, String> {
        let client = policy.client()?;
        Ok(Self {
            vfs,
            client,
            policy,
        })
    }
}

/// 由 Content-Type 推断节点类型。
fn kind_for_mime(mime: &str) -> NodeKind {
    if mime.starts_with("image/") {
        NodeKind::Image
    } else if mime.starts_with("audio/") {
        NodeKind::Audio
    } else if mime.starts_with("video/") {
        NodeKind::Video
    } else if mime.starts_with("text/")
        || mime.contains("json")
        || mime.contains("xml")
        || mime.contains("javascript")
    {
        NodeKind::Text
    } else {
        NodeKind::Binary
    }
}

#[async_trait]
impl Tool for DownloadTool {
    fn name(&self) -> &'static str {
        "Download"
    }

    fn description(&self) -> &'static str {
        "Download an HTTP/HTTPS URL into the VFS. Enforces SSRF protection, domain allow/block \
         lists, redirect limits, and max_bytes. Streams the response and returns VFS path, \
         bytes, MIME, and hash. Does not send; use `API` to send the file."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "url": {"type": "string"},
                "path": {"type": "string", "description": "Absolute VFS path."},
                "max_bytes": {"type": "number", "description": "Optional lower per-file limit."}
            }),
            &["url", "path"],
        )
    }

    fn permissions(&self) -> &'static [Permission] {
        &[Permission::Network, Permission::Write]
    }

    async fn execute(&self, _ctx: &ToolContext, args: Value) -> ToolResult {
        let raw_url = match arg_str(&args, "url") {
            Ok(url) => url,
            Err(error) => return error,
        };
        let path = match arg_str(&args, "path") {
            Ok(path) => path,
            Err(error) => return error,
        };
        let path = match VfsPath::parse(path) {
            Ok(path) => path,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        let limit = match arg_opt_u64(&args, "max_bytes") {
            Ok(value) => value
                .unwrap_or(self.policy.max_bytes)
                .min(self.policy.max_bytes),
            Err(error) => return error,
        };

        let url = match self.policy.check_url(raw_url) {
            Ok(url) => url,
            Err(error) => return ToolResult::error(error),
        };
        let response = match self
            .policy
            .send(
                &self.client,
                url,
                reqwest::Method::GET,
                Default::default(),
                None,
            )
            .await
        {
            Ok(response) => response,
            Err(error) => return ToolResult::error(error),
        };
        if !response.status().is_success() {
            return ToolResult::error(format!("HTTP {}", response.status()));
        }
        let stage = match self.vfs.stage_blob() {
            Ok(stage) => stage,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        let (stage, content_type) = match receive(response, limit, stage).await {
            Ok(received) => received,
            Err(error) => return ToolResult::error(error),
        };
        let mime = content_type
            .split(';')
            .next()
            .unwrap_or("application/octet-stream")
            .trim()
            .to_owned();
        let kind = if mime == "application/octet-stream" {
            guess_mime(path.file_name().unwrap_or_default())
                .map(kind_for_mime)
                .unwrap_or(NodeKind::Binary)
        } else {
            kind_for_mime(&mime)
        };

        let vfs = Arc::clone(&self.vfs);
        let committed = tokio::task::spawn_blocking(move || commit(&vfs, stage, &path, kind)).await;
        match committed {
            Ok(Ok(meta)) => ToolResult::ok(
                serde_json::to_string_pretty(&json!({
                    "path": meta.path.to_string(),
                    "bytes": meta.size,
                    "mime": mime,
                    "kind": meta.kind.as_str(),
                    "hash": meta.hash,
                }))
                .unwrap_or_default(),
            ),
            Ok(Err(error)) => ToolResult::error(error.to_string()),
            Err(error) => ToolResult::error(format!("the download writer failed: {error}")),
        }
    }
}

/// 把响应体收进暂存会话；返回会话与 `Content-Type`。
///
/// 网络分段很小（几 KB），攒满一个分块才交给阻塞线程一次。会话在异步任务与阻塞线程
/// 之间来回移交；出错时它被丢弃，暂存目录随之删除。
async fn receive(
    response: reqwest::Response,
    limit: u64,
    mut stage: BlobStage,
) -> Result<(BlobStage, String), String> {
    let mut body = LimitedBody::new(response, limit)?;
    let mut batch: Vec<u8> = Vec::with_capacity(STAGE_BATCH_BYTES);
    while let Some(chunk) = body.next_chunk().await? {
        batch.extend_from_slice(chunk.as_ref());
        if batch.len() >= STAGE_BATCH_BYTES {
            let full = std::mem::replace(&mut batch, Vec::with_capacity(STAGE_BATCH_BYTES));
            stage = push_blocking(stage, full).await?;
        }
    }
    // 不满一批的尾巴。
    let stage = push_blocking(stage, batch).await?;
    Ok((stage, body.content_type))
}

/// 在阻塞线程里把一批字节加密暂存，交回暂存会话。
async fn push_blocking(mut stage: BlobStage, bytes: Vec<u8>) -> Result<BlobStage, String> {
    if bytes.is_empty() {
        return Ok(stage);
    }
    tokio::task::spawn_blocking(move || stage.push(&bytes).map(|()| stage))
        .await
        .map_err(|error| format!("the download writer failed: {error}"))?
        .map_err(|error| error.to_string())
}

/// 提交暂存内容：文本类型里小而合法的 UTF-8 存成文本节点，其余存成二进制。
fn commit(
    vfs: &VfsStore,
    stage: BlobStage,
    path: &VfsPath,
    kind: NodeKind,
) -> Result<NodeMeta, VfsError> {
    if kind == NodeKind::Text {
        if let Some(text) = stage.text_within(vfs.text_limit())? {
            return vfs.write_text(path, &text, None);
        }
        // 太大或不是 UTF-8：按二进制保存，内容不丢。
        return vfs.commit_stage(stage, path, NodeKind::Binary);
    }
    vfs.commit_stage(stage, path, kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_maps_to_kind() {
        assert_eq!(kind_for_mime("image/png"), NodeKind::Image);
        assert_eq!(kind_for_mime("text/markdown"), NodeKind::Text);
        assert_eq!(kind_for_mime("audio/mpeg"), NodeKind::Audio);
        assert_eq!(kind_for_mime("video/mp4"), NodeKind::Video);
        assert_eq!(kind_for_mime("application/pdf"), NodeKind::Binary);
    }

    #[tokio::test]
    async fn downloads_stream_into_blobs_and_text_nodes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // 本地 HTTP 服务：/big 是 2.5 MiB 二进制，/note 是小文本，/huge 声明超限。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let big: Vec<u8> = (0..(5 * 1024 * 1024 / 2))
            .map(|index| (index % 253) as u8)
            .collect();
        let served = big.clone();
        tokio::spawn(async move {
            let crlf = format!("{}{}", char::from(13), char::from(10));
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut buffer = [0u8; 2048];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                let (content_type, body): (&str, Vec<u8>) = if request.starts_with("GET /big ") {
                    ("application/octet-stream", served.clone())
                } else if request.starts_with("GET /note ") {
                    (
                        "text/plain; charset=utf-8",
                        "下载的文本".as_bytes().to_vec(),
                    )
                } else {
                    ("application/octet-stream", vec![1u8; 4096])
                };
                let head = format!(
                    "HTTP/1.1 200 OK{crlf}Content-Type: {content_type}{crlf}\
                     Content-Length: {}{crlf}Connection: close{crlf}{crlf}",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let vfs = Arc::new(
            crate::vfs::sqlite::VfsStore::open(
                Arc::new(crate::storage::Database::open_in_memory().unwrap()),
                Arc::new(crate::storage::crypto::MasterKey::from_bytes([2u8; 32])),
                dir.path().join("data"),
                crate::vfs::quota::Limits::default(),
            )
            .unwrap(),
        );
        let tool = DownloadTool::new(
            Arc::clone(&vfs),
            NetPolicy {
                allow_private: true,
                ..NetPolicy::default()
            },
        )
        .unwrap();
        let ctx = ToolContext::new(Arc::clone(&vfs), None);

        let result = tool
            .execute(
                &ctx,
                json!({"url": format!("http://{addr}/big"), "path": "/tmp/big.bin"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        let path = VfsPath::parse("/tmp/big.bin").unwrap();
        assert_eq!(vfs.read_bytes(&path).unwrap(), big);

        // 声称是文本、内容是合法 UTF-8：存成可读的文本节点。
        let result = tool
            .execute(
                &ctx,
                json!({"url": format!("http://{addr}/note"), "path": "/tmp/note.txt"}),
            )
            .await;
        assert!(
            result.content.contains("\"kind\": \"text\""),
            "{}",
            result.content
        );
        let note = vfs
            .read(&VfsPath::parse("/tmp/note.txt").unwrap(), None, None)
            .unwrap();
        assert!(format!("{note:?}").contains("下载的文本"), "{note:?}");

        // 超过 max_bytes：报错，不留节点，也不留暂存目录。
        let result = tool
            .execute(
                &ctx,
                json!({"url": format!("http://{addr}/huge"), "path": "/tmp/huge.bin", "max_bytes": 1024}),
            )
            .await;
        assert!(result.is_error);
        assert!(vfs.stat(&VfsPath::parse("/tmp/huge.bin").unwrap()).is_err());
        let stages = std::fs::read_dir(dir.path().join("data").join("blobs"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("stage-"))
            .count();
        assert_eq!(stages, 0);
    }

    #[test]
    fn tool_rejects_private_targets_before_network() {
        // 只需要构造成功；实际请求在测试里不发出。
        let dir = tempfile::tempdir().unwrap();
        let vfs = Arc::new(
            crate::vfs::sqlite::VfsStore::open(
                Arc::new(crate::storage::Database::open_in_memory().unwrap()),
                Arc::new(crate::storage::crypto::MasterKey::from_bytes([2u8; 32])),
                dir.path().join("data"),
                crate::vfs::quota::Limits::default(),
            )
            .unwrap(),
        );
        let tool = DownloadTool::new(vfs, NetPolicy::default()).unwrap();
        // check_url 不解析 DNS；127.0.0.1 会在 resolve_public 被拒绝。
        let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        assert!(!super::super::net::is_public_ip(ip));
        assert!(tool.policy.check_url("file:///etc/passwd").is_err());
    }
}
