//! `Download` 工具：把 HTTP/HTTPS 内容写进 VFS。
//!
//! 设计依据：设计 §5.2 / §6.4。
//!
//! - 强制 SSRF 防护、域名允许/阻止、重定向上限与 `max_bytes`；
//! - 流式读取，超限即中止；
//! - 返回 VFS 路径、字节数、MIME 与 hash；发送文件用 `API`，不在这里做。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::ToolResult;
use crate::vfs::sqlite::VfsStore;
use crate::vfs::{NodeKind, Vfs, VfsPath, guess_mime};

use super::net::{NetPolicy, read_limited};
use super::{Permission, Tool, ToolContext, arg_opt_u64, arg_str, object_schema};

/// `Download` 工具。
pub struct DownloadTool {
    vfs: Arc<VfsStore>,
    client: reqwest::Client,
    policy: NetPolicy,
}

impl DownloadTool {
    pub fn new(vfs: Arc<VfsStore>, policy: NetPolicy) -> Result<Self, String> {
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
        let (bytes, content_type) = match read_limited(response, limit).await {
            Ok(value) => value,
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

        match self.vfs.write_binary(&path, &bytes, kind) {
            Ok(meta) => ToolResult::ok(
                serde_json::to_string_pretty(&json!({
                    "path": meta.path.to_string(),
                    "bytes": meta.size,
                    "mime": mime,
                    "hash": meta.hash,
                }))
                .unwrap_or_default(),
            ),
            Err(error) => ToolResult::error(error.to_string()),
        }
    }
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
