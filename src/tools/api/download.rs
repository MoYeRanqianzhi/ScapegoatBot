//! `API` 的 `local.net.download`：把 HTTP/HTTPS 内容写进 VFS。
//!
//! - 强制 SSRF 防护、域名允许/阻止、重定向上限与 `max_bytes`（`[tools.download]`）；
//! - 流式写入：响应体每攒满约 1 MiB 就交给阻塞线程加密暂存
//!   （[`crate::vfs::sqlite::BlobStage`]），内存里不留整个文件，超限即中止；
//!   提交同样在阻塞线程里做，不占用异步工作线程；
//! - 文本类型（`text/*`、JSON、XML、JS）且不超过文本上限、是合法 UTF-8 时存成文本节点
//!   （可 `Read` / `Edit`），否则存成二进制；
//! - 返回 VFS 路径、字节数、MIME、节点类型与 hash；发送文件是 `napcat.*` 的事，不在这里做。
//!
//! 它只写 VFS，不碰 QQ：`hold` 期间照常可用，子代理也能用（受写保护约束）。

use std::sync::Arc;

use serde_json::{Value, json};

use super::ApiTool;
use crate::tools::net::{LimitedBody, NetPolicy};
use crate::tools::{ToolContext, arg_opt_u64, arg_str};
use crate::transcript::ToolResult;
use crate::vfs::sqlite::{BlobStage, VfsStore};
use crate::vfs::{NodeKind, NodeMeta, Vfs, VfsError, VfsPath, guess_mime};

/// 交给暂存线程的批量大小：与 VFS 分块一致，每个分块切一次线程。
const STAGE_BATCH_BYTES: usize = 1024 * 1024;

/// 下载用的 HTTP 客户端与它遵守的网络策略；装配时由 `[tools.download]` 建好。
pub(crate) struct Downloader {
    client: reqwest::Client,
    policy: NetPolicy,
}

impl Downloader {
    /// 按策略建客户端；失败（TLS 后端初始化等）时返回错误文字，调用方记日志并不启用下载。
    pub(crate) fn new(policy: NetPolicy) -> Result<Self, String> {
        let client = policy.client()?;
        Ok(Self { client, policy })
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

impl ApiTool {
    /// `local.net.download`：`url` → VFS 的 `path`。
    ///
    /// 顺序：解析参数 → 子代理写保护 → URL 策略检查 → 请求 → 流式暂存 → 提交。任何一步
    /// 失败都不留下节点；暂存会话被丢弃时暂存目录随之删除。
    pub(super) async fn run_download(&self, params: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(downloader) = &self.downloader else {
            return ToolResult::error("Downloads are not available in this runtime.");
        };
        let raw_url = match arg_str(params, "url") {
            Ok(url) => url,
            Err(error) => return error,
        };
        let path = match arg_str(params, "path") {
            Ok(path) => path,
            Err(error) => return error,
        };
        let path = match VfsPath::parse(path) {
            Ok(path) => path,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        if let Some(denied) = crate::tools::subagent_write_denied(ctx, &path) {
            return denied;
        }
        // 调用方只能把上限调低，不能超过配置。
        let limit = match arg_opt_u64(params, "max_bytes") {
            Ok(value) => value
                .unwrap_or(downloader.policy.max_bytes)
                .min(downloader.policy.max_bytes),
            Err(error) => return error,
        };

        let url = match downloader.policy.check_url(raw_url) {
            Ok(url) => url,
            Err(error) => return ToolResult::error(error),
        };
        let response = match downloader
            .policy
            .send(
                &downloader.client,
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
        let stage = match ctx.vfs.stage_blob() {
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
        // 服务端没说类型时按文件名猜；猜不出就是二进制。
        let kind = if mime == "application/octet-stream" {
            guess_mime(path.file_name().unwrap_or_default())
                .map(kind_for_mime)
                .unwrap_or(NodeKind::Binary)
        } else {
            kind_for_mime(&mime)
        };

        let vfs = Arc::clone(&ctx.vfs);
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
    use crate::tools::Tool;

    /// 临时目录里的一个空 VFS。
    fn test_vfs(dir: &tempfile::TempDir) -> Arc<VfsStore> {
        Arc::new(
            VfsStore::open(
                Arc::new(crate::storage::Database::open_in_memory().unwrap()),
                Arc::new(crate::storage::crypto::MasterKey::from_bytes([2u8; 32])),
                dir.path().join("data"),
                crate::vfs::quota::Limits::default(),
            )
            .unwrap(),
        )
    }

    /// 只接了下载器的 `API`：没有 NapCat、没有 agent 操作。
    fn api_with(policy: NetPolicy) -> ApiTool {
        ApiTool::new(None, None, 1, 1, None, Vec::new(), None)
            .with_downloader(Arc::new(Downloader::new(policy).unwrap()))
    }

    /// 经 `API` 的正常入口调用 `local.net.download`（参数检查与分派都走一遍）。
    async fn download(tool: &ApiTool, ctx: &ToolContext, params: Value) -> ToolResult {
        tool.execute(
            ctx,
            json!({"action": "local.net.download", "params": params}),
        )
        .await
    }

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
        let vfs = test_vfs(&dir);
        let tool = api_with(NetPolicy {
            allow_private: true,
            ..NetPolicy::default()
        });
        let ctx = ToolContext::new(Arc::clone(&vfs), None);

        let result = download(
            &tool,
            &ctx,
            json!({"url": format!("http://{addr}/big"), "path": "/tmp/big.bin"}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let path = VfsPath::parse("/tmp/big.bin").unwrap();
        assert_eq!(vfs.read_bytes(&path).unwrap(), big);

        // 声称是文本、内容是合法 UTF-8：存成可读的文本节点。
        let result = download(
            &tool,
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
        let result = download(
            &tool,
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

    /// 默认策略下：非 http(s) 协议在发请求前就被拒绝；loopback 在解析阶段被拒绝，
    /// 两种情况都不留下节点。没有接入下载器的 `API`（测试、最小装配）直接说明不可用。
    #[tokio::test]
    async fn rejects_non_public_targets_and_reports_when_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let vfs = test_vfs(&dir);
        let ctx = ToolContext::new(Arc::clone(&vfs), None);
        let tool = api_with(NetPolicy::default());

        for url in ["file:///etc/passwd", "http://127.0.0.1:9/x"] {
            let result = download(&tool, &ctx, json!({"url": url, "path": "/tmp/x.bin"})).await;
            assert!(result.is_error, "{url}: {}", result.content);
        }
        assert!(vfs.stat(&VfsPath::parse("/tmp/x.bin").unwrap()).is_err());

        let bare = ApiTool::new(None, None, 1, 1, None, Vec::new(), None);
        let result = download(
            &bare,
            &ctx,
            json!({"url": "https://example.com/a", "path": "/tmp/a"}),
        )
        .await;
        assert!(result.is_error);
        assert!(
            result.content.contains("not available"),
            "{}",
            result.content
        );
    }

    /// 子代理下载同样受写保护：不能把内容下进 `/memory`、`/config` 等路径（在发请求之前拒绝）。
    #[tokio::test]
    async fn subagents_cannot_download_into_protected_paths() {
        let dir = tempfile::tempdir().unwrap();
        let vfs = test_vfs(&dir);
        let ctx = ToolContext::new(Arc::clone(&vfs), None).with_subagent("sa-1");
        let tool = api_with(NetPolicy::default());
        for path in ["/memory/x.md", "/config/x.toml", "/AGENTS.md"] {
            let result = download(
                &tool,
                &ctx,
                json!({"url": "https://example.com/a", "path": path}),
            )
            .await;
            assert!(result.is_error, "{path}: {}", result.content);
            assert!(
                result.content.contains("Subagents cannot modify"),
                "{path}: {}",
                result.content
            );
        }
    }
}
