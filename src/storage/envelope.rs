//! 加密信封：AEAD 输出在 JSON 行里拆成 `nonce` / `ct` 两个 base64 字段。
//!
//! transcript 与消息归档共用同一套拆装逻辑。`MasterKey` 的 AEAD 输出是
//! `nonce || ciphertext`；这里负责拆开编码，以及反向拼回后解密。

use anyhow::Result;
use base64::Engine;

use crate::config::EncryptionMode;
use crate::storage::crypto::MasterKey;

/// XChaCha20-Poly1305 的 nonce 长度；必须与 `storage::crypto` 内部一致。
const NONCE_LEN: usize = 24;

/// 加密 `payload`，返回 `(nonce_b64, ct_b64)`。
pub(crate) fn seal(
    key: &MasterKey,
    purpose: &str,
    aad: &[u8],
    payload: &[u8],
) -> Result<(String, String)> {
    let sealed = key.seal(purpose, aad, payload)?;
    let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
    Ok((
        base64::engine::general_purpose::STANDARD.encode(nonce),
        base64::engine::general_purpose::STANDARD.encode(ciphertext),
    ))
}

/// 按文件的加密模式封装一行的载荷：`XChaCha20Poly1305` 走 [`seal`]；`None`（仅调试）
/// 只做 base64，`nonce` 为空。transcript 与消息归档的写入共用它。
pub(crate) fn seal_as(
    mode: EncryptionMode,
    key: &MasterKey,
    purpose: &str,
    aad: &str,
    payload: &[u8],
) -> Result<(String, String)> {
    match mode {
        EncryptionMode::XChaCha20Poly1305 => seal(key, purpose, aad.as_bytes(), payload),
        EncryptionMode::None => Ok((
            String::new(),
            base64::engine::general_purpose::STANDARD.encode(payload),
        )),
    }
}

/// [`seal_as`] 的反向：按文件的加密模式取回载荷。`aad` 只在加密模式下才构造（闭包）。
pub(crate) fn open_as(
    mode: EncryptionMode,
    key: &MasterKey,
    purpose: &str,
    aad: impl FnOnce() -> String,
    nonce_b64: &str,
    ct_b64: &str,
) -> Result<Vec<u8>> {
    match mode {
        EncryptionMode::XChaCha20Poly1305 => {
            open(key, purpose, aad().as_bytes(), nonce_b64, ct_b64)
        }
        EncryptionMode::None => Ok(base64::engine::general_purpose::STANDARD.decode(ct_b64)?),
    }
}

/// 解密 `seal` 的输出。
pub(crate) fn open(
    key: &MasterKey,
    purpose: &str,
    aad: &[u8],
    nonce_b64: &str,
    ct_b64: &str,
) -> Result<Vec<u8>> {
    let sealed = join(nonce_b64, ct_b64)?;
    key.open(purpose, aad, &sealed)
}

/// 把两个 base64 字段解码并拼成 `nonce || ciphertext`，不做解密。
///
/// 恢复扫描要对同一个信封尝试多个 AAD，先拼一次再反复解密；base64 错误
/// 也因此不会被误当成“这个候选行号解不开”。
pub(crate) fn join(nonce_b64: &str, ct_b64: &str) -> Result<Vec<u8>> {
    let ciphertext = base64::engine::general_purpose::STANDARD.decode(ct_b64)?;
    let nonce = base64::engine::general_purpose::STANDARD.decode(nonce_b64)?;
    let mut sealed = Vec::with_capacity(nonce.len() + ciphertext.len());
    sealed.extend_from_slice(&nonce);
    sealed.extend_from_slice(&ciphertext);
    Ok(sealed)
}
