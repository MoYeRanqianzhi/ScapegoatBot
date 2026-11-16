use super::*;

impl MessageStore {
    /// 确保文件头存在，返回 `file_id`。
    pub(super) fn ensure_header(
        &self,
        path: &Path,
        scope: &str,
        peer: u64,
        date: &str,
    ) -> Result<String> {
        if path.exists()
            && let Some(file_id) = read_file_id(path)?
        {
            return Ok(file_id);
        }
        // 空文件或残行：补写文件头。

        let file_id = new_file_id();
        let header = json!({
            "v": LINE_VERSION,
            "kind": "header",
            "scope": scope,
            "peer_id": peer,
            "date": date,
            // 建文件时的配置，只作记录：读取按每一行自己的封装方式（见 `envelope::open_line`）。
            "encryption": match self.encryption {
                EncryptionMode::XChaCha20Poly1305 => "xchacha20poly1305",
                EncryptionMode::None => "none",
            },
            "created_at": Utc::now().to_rfc3339(),
            "file_id": file_id,
        });
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        file.write_all(format!("{header}\n").as_bytes())?;
        file.flush()?;
        Ok(file_id)
    }

    /// 编码一行。
    pub(super) fn encode_line(
        &self,
        message: &QqMessage,
        scope: &str,
        peer: u64,
        file_id: &str,
        line_no: usize,
    ) -> Result<String> {
        let payload = serde_json::to_vec(message)?;
        let aad = format!(
            "message:{scope}:{peer}:{}:{file_id}:{line_no}",
            message.message_id
        );
        let (nonce, ciphertext) =
            envelope::seal_as(self.encryption, &self.key, "message", &aad, &payload)?;

        let line = json!({
            "v": LINE_VERSION,
            "kind": "message",
            "id": message.message_id,
            "ts": message.time.to_rfc3339(),
            "scope": scope,
            "peer_id": peer,
            "sender": message.sender.label(),
            "nonce": nonce,
            "ct": ciphertext,
        });
        Ok(format!("{line}\n"))
    }

    /// 读取某文件的某一行并解密。
    ///
    /// `expected_id` 用于校验索引行号与文件内容一致：索引可能因崩溃或并发写入
    /// 落后/错位，解密本身不会发现这种事（AAD 绑定的是行内 id），必须在返回前比对。
    pub(super) fn read_line(
        &self,
        path: &Path,
        line_no: usize,
        expected_id: Option<i64>,
    ) -> Result<Option<QqMessage>> {
        let Some(file) = open_message_file(path)? else {
            return Ok(None);
        };
        self.decode_message_at(&file, line_no, expected_id)
    }

    /// 在已读入内存的消息文件上解码一行；索引视图按文件复用，避免 O(N²) 读取。
    pub(super) fn decode_message_at(
        &self,
        file: &MessageFile,
        line_no: usize,
        expected_id: Option<i64>,
    ) -> Result<Option<QqMessage>> {
        let Some(line) = file.lines.get(line_no) else {
            return Ok(None);
        };
        let value: Value = serde_json::from_str(line)?;
        let scope = value
            .get("scope")
            .and_then(Value::as_str)
            .context("message line has no scope")?;
        let peer = value
            .get("peer_id")
            .and_then(Value::as_u64)
            .context("message line has no peer_id")?;
        let message_id = value
            .get("id")
            .and_then(Value::as_i64)
            .context("message line has no id")?;
        if let Some(expected) = expected_id
            && expected != message_id
        {
            warn!(
                file = %file.path,
                line = line_no,
                expected,
                actual = message_id,
                "message index line points at a different message"
            );
            return Ok(None);
        }
        let ct_b64 = value.get("ct").and_then(Value::as_str).unwrap_or_default();

        let nonce_b64 = value
            .get("nonce")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // AAD 与写入时一致：作用域、对端、消息 id、文件 id 与行号。
        let plain = envelope::open_line(
            &self.key,
            "message",
            || {
                format!(
                    "message:{scope}:{peer}:{message_id}:{}:{line_no}",
                    file.file_id
                )
            },
            nonce_b64,
            ct_b64,
        )?;
        Ok(Some(serde_json::from_slice(&plain)?))
    }
}

/// 已读入内存的消息文件；同一索引视图按文件复用，避免逐行重读整文件。
pub(super) struct MessageFile {
    pub(super) path: String,
    pub(super) file_id: String,
    pub(super) lines: Vec<String>,
}

/// 打开并读入一个消息文件；不存在返回 `None`。
pub(super) fn open_message_file(path: &Path) -> Result<Option<MessageFile>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let lines: Vec<String> = BufReader::new(file)
        .lines()
        .collect::<Result<Vec<_>, _>>()?;
    let Some(header) = lines.first() else {
        return Ok(None);
    };
    let header: Value = serde_json::from_str(header)?;
    let file_id = header
        .get("file_id")
        .and_then(Value::as_str)
        .context("message file has no file_id")?
        .to_owned();
    Ok(Some(MessageFile {
        path: path.display().to_string(),
        file_id,
        lines,
    }))
}
