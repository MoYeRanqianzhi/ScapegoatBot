use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce, aead::Aead};
use hkdf::Hkdf;
use sha2::Sha256;
use std::io::{BufRead, BufReader};
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let kp = args.next().unwrap();
    let sp = args.next().unwrap();
    let raw = std::fs::read_to_string(kp)?;
    let master = hex::decode(raw.trim().strip_prefix("sbot-key-v1:").unwrap())?;
    let hk = Hkdf::<Sha256>::new(Some(b"scapegoat-bot/master-key/v1"), &master);
    let mut key = [0u8; 32];
    hk.expand(b"session", &mut key).unwrap();
    let c = XChaCha20Poly1305::new((&key).into());
    let lines: Vec<String> = BufReader::new(std::fs::File::open(sp)?)
        .lines()
        .collect::<Result<_, _>>()?;
    let h: serde_json::Value = serde_json::from_str(&lines[0])?;
    let fid = h["file_id"].as_str().unwrap();
    for (i, l) in lines.iter().enumerate().skip(1) {
        let v: serde_json::Value = serde_json::from_str(l)?;
        let n = B64.decode(v["nonce"].as_str().unwrap_or_default())?;
        let ct = B64.decode(v["ct"].as_str().unwrap_or_default())?;
        let mut out = None;
        for cand in 1..=50000usize {
            let aad = format!("session:{fid}:{cand}");
            if let Ok(p) = c.decrypt(
                XNonce::from_slice(&n),
                chacha20poly1305::aead::Payload {
                    msg: &ct,
                    aad: aad.as_bytes(),
                },
            ) {
                out = Some(p);
                break;
            }
        }
        match out {
            Some(p) => println!("{}", String::from_utf8_lossy(&p)),
            None => println!("FAIL {i}"),
        }
    }
    Ok(())
}
