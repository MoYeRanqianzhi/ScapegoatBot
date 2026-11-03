//! `Views` 的 sessions 部分。
//!
//! 由 `views/mod.rs` 的 `list` / `read` 路由调用。

use super::*;

impl Views {
    pub(super) fn list_sessions_root(&self) -> Result<Vec<ViewEntry>, VfsError> {
        let mut entries = vec![text_entry("/sessions/index.jsonl")];
        for file in self.sessions.list().map_err(storage)? {
            entries.push(ViewEntry {
                path: format!("/sessions/{}", file.file),
                kind: "text",
                size: file.bytes,
                mtime: file.started_at,
            });
        }
        Ok(entries)
    }

    pub(super) fn read_sessions_digest(&self) -> Result<ViewRead, VfsError> {
        let list = self.sessions.list().map_err(storage)?;
        let mut out = String::from("# Sessions\n");
        for file in list {
            out.push_str(&format!(
                "- /sessions/{} ({} messages, {} bytes{})\n",
                file.file,
                file.message_count,
                file.bytes,
                if file.ended_at.is_some() {
                    ", closed"
                } else {
                    ", current"
                }
            ));
        }
        Ok(ViewRead::Text(out))
    }

    pub(super) fn read_sessions_index(&self) -> Result<ViewRead, VfsError> {
        let list = self.sessions.list().map_err(storage)?;
        let mut out = String::new();
        for file in list {
            let line = serde_json::json!({
                "file": file.file,
                "prev": file.prev_file,
                "started_at": file.started_at,
                "ended_at": file.ended_at,
                "message_count": file.message_count,
                "bytes": file.bytes,
                "pruned": self.sessions.info(&file.file).map(|info| info.is_none()).unwrap_or(false),
            });
            out.push_str(&line.to_string());
            out.push('\n');
        }
        Ok(ViewRead::Text(out))
    }

    pub(super) fn read_session(&self, file: &str) -> Result<ViewRead, VfsError> {
        let messages = self.sessions.read_segment(file).map_err(storage)?;
        Ok(ViewRead::Text(
            messages
                .iter()
                .map(render_agent_message)
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }

    // ---------------------------------------------------------------------
    // /friends /groups（离线时实时文件报错，notes 仍可读写）
    // ---------------------------------------------------------------------
}
