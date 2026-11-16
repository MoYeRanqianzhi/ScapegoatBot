//! 主密钥与内容加密。
//!
//! 设计依据：设计 §6.6 / §13「加密：内容加密、索引明文」。
//!
//! 形态：
//! - 主密钥 32 字节，首次初始化时自动生成到 `~/.sbot/key`，无口令、无 keychain、无解锁步骤。
//! - 文件内容为 `sbot-key-v1:<64 位十六进制>\n`；格式不对直接拒绝启动，不尝试“修复”。
//! - 不同用途（VFS、消息、transcript、blob）用 HKDF 从主密钥派生独立子密钥，避免跨域复用。
//! - AEAD 为 XChaCha20-Poly1305；输出为 `nonce(24 字节) || 密文`，AAD 由调用方给出。

use std::path::Path;

use anyhow::{Context, Result, bail};
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;

/// 密钥文件头；格式变化时必须换版本号。
const KEY_FILE_PREFIX: &str = "sbot-key-v1:";
/// HKDF 的固定 salt：与密钥文件版本绑定。
const HKDF_SALT: &[u8] = b"scapegoat-bot/master-key/v1";
/// 主密钥长度。
const KEY_LEN: usize = 32;
/// XChaCha20 的 nonce 长度。
const NONCE_LEN: usize = 24;

/// 主密钥。
pub struct MasterKey {
    bytes: [u8; KEY_LEN],
}

impl MasterKey {
    /// 从已有字节构造（测试与迁移使用）。
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self { bytes }
    }

    /// 读取密钥文件；不存在时生成并落盘。
    ///
    /// 生成流程是“写临时文件 → fsync → rename”，避免半截文件被当成有效密钥。
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::generate(path),
            Err(error) => Err(anyhow::anyhow!(
                "cannot read key file {}: {error}",
                path.display()
            )),
        }
    }

    /// 只读取已存在的密钥文件；不存在时报错而不是生成。
    ///
    /// `sbot doctor` 用这个：诊断不应该顺手创建新密钥。
    pub fn load_existing(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read key file {}", path.display()))?;
        Self::parse(&text, path)
    }

    /// 解析密钥文件内容。
    fn parse(text: &str, path: &Path) -> Result<Self> {
        let trimmed = text.trim();
        let Some(hex_part) = trimmed.strip_prefix(KEY_FILE_PREFIX) else {
            bail!(
                "key file {} has an unknown format; expected {KEY_FILE_PREFIX}<64 hex>",
                path.display()
            );
        };
        if hex_part.len() != KEY_LEN * 2 {
            bail!(
                "key file {} has the wrong length; expected {} hex characters",
                path.display(),
                KEY_LEN * 2
            );
        }
        let raw = hex::decode(hex_part)
            .with_context(|| format!("key file {} is not valid hex", path.display()))?;
        let mut bytes = [0u8; KEY_LEN];
        bytes.copy_from_slice(&raw);
        Ok(Self { bytes })
    }

    /// 生成新密钥并写入文件。
    fn generate(path: &Path) -> Result<Self> {
        let mut bytes = [0u8; KEY_LEN];
        getrandom::fill(&mut bytes).map_err(|error| {
            anyhow::anyhow!("cannot obtain random bytes for the master key: {error}")
        })?;
        let text = format!("{KEY_FILE_PREFIX}{}\n", hex::encode(bytes));

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }

        // 先写临时文件再改名：即便进程在写一半时被杀死，也不会留下可被误用的密钥文件。
        let tmp = path.with_extension("tmp");
        write_private(&tmp, text.as_bytes())?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("cannot install key file {}", path.display()))?;

        Ok(Self { bytes })
    }

    /// 派生某个用途的子密钥。
    fn derive(&self, purpose: &str) -> [u8; KEY_LEN] {
        // HKDF-SHA256：salt 固定、info 用用途名，保证不同域互不通用。
        let hkdf = Hkdf::<Sha256>::new(Some(HKDF_SALT), &self.bytes);
        let mut out = [0u8; KEY_LEN];
        hkdf.expand(purpose.as_bytes(), &mut out)
            .expect("HKDF output length is valid");
        out
    }

    /// 加密：返回 `nonce || ciphertext`。
    pub fn seal(&self, purpose: &str, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
        let key = self.derive(purpose);
        let cipher = XChaCha20Poly1305::new((&key).into());

        let mut nonce_bytes = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce_bytes)
            .map_err(|error| anyhow::anyhow!("cannot obtain random nonce: {error}"))?;
        let nonce = XNonce::from(nonce_bytes);

        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("encryption failed"))?;

        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// 解密 `nonce || ciphertext`。
    pub fn open(&self, purpose: &str, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
        // nonce 之后至少还要有 16 字节的认证标签。
        let Some((nonce_bytes, ciphertext)) = sealed
            .split_first_chunk::<NONCE_LEN>()
            .filter(|(_, ciphertext)| ciphertext.len() >= 16)
        else {
            bail!("ciphertext is too short");
        };
        let key = self.derive(purpose);
        let cipher = XChaCha20Poly1305::new((&key).into());
        cipher
            .decrypt(
                &XNonce::from(*nonce_bytes),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| {
                anyhow::anyhow!("decryption failed: wrong key, wrong purpose or tampered data")
            })
    }
}

impl Drop for MasterKey {
    fn drop(&mut self) {
        // 尽力清零；Rust 不保证优化器不消除，但成本极低，仍然做。
        for byte in &mut self.bytes {
            // 使用 volatile 写入，避免被优化掉。
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}

/// 以“仅当前用户可读”的权限写文件。
#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("cannot write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("cannot fsync {}", path.display()))?;
    Ok(())
}

/// Windows 上不做额外 ACL 处理：密钥位于用户目录，默认继承用户配置目录的权限。
/// 设计 §6.6 已声明威胁边界不包含同机管理员。
#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let mut file =
        std::fs::File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("cannot write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("cannot fsync {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_key() -> MasterKey {
        MasterKey::from_bytes([7u8; KEY_LEN])
    }

    /// 磁盘上的密文必须一直解得开：这段是 chacha20poly1305 0.10 加密出来的（密钥、用途、
    /// AAD 与明文见下）。升级加密库或改动密钥派生、封装格式之后它解不开，就是把所有已有
    /// 数据变成了读不出来的东西。
    #[test]
    fn data_sealed_by_an_older_build_still_opens() {
        let sealed = hex::decode(
            "625d7dab14773bd503ddbc2696614a51d537be98e1ecb990f0943041984a43e2f58b6e325c73fb50\
             cdee90c63d0485bfec93abd92d00319af7ccf1ce0c668d23ae",
        )
        .unwrap();
        let opened = fixed_key().open("vfs", b"vfs:42:7", &sealed).unwrap();
        assert_eq!(opened, b"written by an older build");
        // 同一段密文换一个 AAD 就解不开：认证确实覆盖了 AAD。
        assert!(fixed_key().open("vfs", b"vfs:42:8", &sealed).is_err());
    }

    /// 太短的输入（不够 nonce 加认证标签）是错误，不是 panic。
    #[test]
    fn truncated_input_is_rejected() {
        let key = fixed_key();
        let sealed = key.seal("vfs", b"aad", b"").unwrap();
        assert_eq!(sealed.len(), NONCE_LEN + 16);
        assert_eq!(key.open("vfs", b"aad", &sealed).unwrap(), b"");
        for length in [0, 1, NONCE_LEN - 1, NONCE_LEN, NONCE_LEN + 15] {
            let error = key.open("vfs", b"aad", &sealed[..length]).unwrap_err();
            assert!(error.to_string().contains("too short"), "{length}: {error}");
        }
    }

    #[test]
    fn seal_open_roundtrip() {
        let key = fixed_key();
        let sealed = key.seal("vfs", b"aad", b"hello").unwrap();
        assert_ne!(&sealed[..], b"hello");
        let opened = key.open("vfs", b"aad", &sealed).unwrap();
        assert_eq!(opened, b"hello");
    }

    #[test]
    fn different_purpose_cannot_open() {
        let key = fixed_key();
        let sealed = key.seal("vfs", b"", b"secret").unwrap();
        assert!(key.open("messages", b"", &sealed).is_err());
    }

    #[test]
    fn wrong_aad_fails() {
        let key = fixed_key();
        let sealed = key.seal("vfs", b"path:/a", b"secret").unwrap();
        assert!(key.open("vfs", b"path:/b", &sealed).is_err());
    }

    #[test]
    fn nonce_is_random_each_time() {
        let key = fixed_key();
        let first = key.seal("vfs", b"", b"same").unwrap();
        let second = key.seal("vfs", b"", b"same").unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn create_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        let created = MasterKey::load_or_create(&path).unwrap();
        let sealed = created.seal("transcript", b"", b"data").unwrap();

        let loaded = MasterKey::load_or_create(&path).unwrap();
        assert_eq!(loaded.open("transcript", b"", &sealed).unwrap(), b"data");
    }

    #[test]
    fn malformed_key_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, "not-a-key").unwrap();
        let error = MasterKey::load_or_create(&path).unwrap_err().to_string();
        assert!(error.contains("unknown format"));

        std::fs::write(&path, format!("{KEY_FILE_PREFIX}abcd\n")).unwrap();
        let error = MasterKey::load_or_create(&path).unwrap_err().to_string();
        assert!(error.contains("wrong length"));
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        MasterKey::load_or_create(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
