//! Encryption at rest. Every env var, secret and credential is sealed with a 256-bit
//! master key that never lives in the database: it is sealed to the TPM through
//! `systemd-creds` when the machine has one, otherwise kept in a root-only key file.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use std::path::Path;
use zeroize::Zeroizing;

const VERSION: u8 = 1;

#[derive(Clone)]
pub struct Keyring {
    cipher: XChaCha20Poly1305,
    mac_key: Zeroizing<[u8; 32]>,
}

impl Keyring {
    pub fn new(master: &[u8; 32]) -> Self {
        Keyring { cipher: XChaCha20Poly1305::new(master.into()), mac_key: Zeroizing::new(*master) }
    }

    /// Seal `plain`, binding it to `context` (e.g. `env:<service>:<key>`) so a ciphertext
    /// copied into another row fails to open.
    pub fn seal(&self, context: &str, plain: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; 24];
        rand::rng().fill_bytes(&mut nonce);
        let ct = self
            .cipher
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad: context.as_bytes() })
            .map_err(|_| anyhow!("encryption failed for {context}"))?;
        let mut out = Vec::with_capacity(1 + 24 + ct.len());
        out.push(VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn open(&self, context: &str, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        if sealed.len() < 25 || sealed[0] != VERSION {
            bail!("unsupported ciphertext");
        }
        let plain = self
            .cipher
            .decrypt(XNonce::from_slice(&sealed[1..25]), Payload { msg: &sealed[25..], aad: context.as_bytes() })
            .map_err(|_| anyhow!("decryption failed for {context}"))?;
        Ok(Zeroizing::new(plain))
    }

    pub fn seal_str(&self, context: &str, plain: &str) -> Result<Vec<u8>> {
        self.seal(context, plain.as_bytes())
    }

    pub fn open_str(&self, context: &str, sealed: &[u8]) -> Result<Zeroizing<String>> {
        let bytes = self.open(context, sealed)?;
        Ok(Zeroizing::new(String::from_utf8(bytes.to_vec()).context("not utf-8")?))
    }

    /// Key derived for a purpose (restic repo passwords, signing...): HMAC-SHA256 of the purpose.
    pub fn derive(&self, purpose: &str) -> Result<String> {
        use hmac::{Hmac, Mac};
        let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(self.mac_key.as_ref()).map_err(|_| anyhow!("invalid key length"))?;
        mac.update(b"tugport:");
        mac.update(purpose.as_bytes());
        Ok(hex::encode(mac.finalize().into_bytes()))
    }
}

/// How the master key is stored on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStore {
    Tpm,
    File,
}

const TPM_FILE: &str = "master.key.cred";
const PLAIN_FILE: &str = "master.key";

pub fn load_master(etc: &Path) -> Result<Zeroizing<[u8; 32]>> {
    let tpm = etc.join(TPM_FILE);
    if tpm.exists() {
        let out = std::process::Command::new("systemd-creds")
            .args(["decrypt", "--name=tugport-master"])
            .arg(&tpm)
            .arg("-")
            .output()
            .context("running systemd-creds to unseal the master key")?;
        if !out.status.success() {
            bail!("systemd-creds could not unseal the master key: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        return parse_key(&String::from_utf8_lossy(&out.stdout));
    }
    let file = etc.join(PLAIN_FILE);
    let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    parse_key(&text)
}

/// Create a new master key. Returns it with where it was stored; the caller shows
/// `recovery_key(&key)` to the operator once.
pub fn create_master(etc: &Path) -> Result<(Zeroizing<[u8; 32]>, KeyStore)> {
    let mut key = Zeroizing::new([0u8; 32]);
    rand::rng().fill_bytes(key.as_mut());
    let hex_key = Zeroizing::new(hex::encode(key.as_ref()));
    std::fs::create_dir_all(etc)?;
    if tpm_available() {
        let mut child = std::process::Command::new("systemd-creds")
            .args(["encrypt", "--with-key=tpm2", "--name=tugport-master", "-"])
            .arg(etc.join(TPM_FILE))
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        use std::io::Write;
        child.stdin.take().ok_or_else(|| anyhow!("systemd-creds has no stdin"))?.write_all(hex_key.as_bytes())?;
        if child.wait()?.success() {
            return Ok((key, KeyStore::Tpm));
        }
        tracing::warn!("sealing the master key to the TPM failed, falling back to a key file");
    }
    write_private(&etc.join(PLAIN_FILE), hex_key.as_bytes())?;
    Ok((key, KeyStore::File))
}

/// Restore a master key from its recovery key (after a reinstall).
pub fn restore_master(etc: &Path, recovery: &str) -> Result<()> {
    let key = parse_recovery(recovery)?;
    std::fs::create_dir_all(etc)?;
    write_private(&etc.join(PLAIN_FILE), hex::encode(key.as_ref()).as_bytes())
}

fn tpm_available() -> bool {
    Path::new("/dev/tpmrm0").exists()
        && std::process::Command::new("systemd-creds")
            .arg("has-tpm2")
            .stdout(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(data)?;
    Ok(())
}

fn parse_key(text: &str) -> Result<Zeroizing<[u8; 32]>> {
    let bytes = Zeroizing::new(hex::decode(text.trim()).context("master key is not hex")?);
    let mut key = Zeroizing::new([0u8; 32]);
    if bytes.len() != 32 {
        bail!("master key must be 32 bytes");
    }
    key.copy_from_slice(&bytes);
    Ok(key)
}

/// Human-friendly recovery key: `TP1-` + base32 in groups of five.
pub fn recovery_key(key: &[u8; 32]) -> String {
    let mut out = String::from("TP1-");
    for (i, c) in base32(key).chars().enumerate() {
        if i > 0 && i % 5 == 0 {
            out.push('-');
        }
        out.push(c);
    }
    out
}

pub fn parse_recovery(text: &str) -> Result<Zeroizing<[u8; 32]>> {
    let body = text.trim().strip_prefix("TP1-").ok_or_else(|| anyhow!("recovery keys start with TP1-"))?;
    let clean: String = body.chars().filter(|c| *c != '-').collect::<String>().to_uppercase();
    let bytes = Zeroizing::new(unbase32(&clean).ok_or_else(|| anyhow!("recovery key has invalid characters"))?);
    if bytes.len() < 32 {
        bail!("recovery key is too short");
    }
    let mut key = Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&bytes[..32]);
    Ok(key)
}

const B32: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn unbase32(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in text.bytes() {
        let v = B32.iter().position(|&x| x == c)? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

pub fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

pub fn unb64(text: &str) -> Result<Vec<u8>> {
    Ok(base64::engine::general_purpose::STANDARD.decode(text.trim())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_roundtrip_and_context_binding() {
        let k = Keyring::new(&[9u8; 32]);
        let sealed = k.seal_str("env:a:KEY", "hunter2").unwrap();
        assert_eq!(&*k.open_str("env:a:KEY", &sealed).unwrap(), "hunter2");
        assert!(k.open_str("env:b:KEY", &sealed).is_err(), "context must bind the ciphertext");
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(k.open_str("env:a:KEY", &tampered).is_err());
    }

    #[test]
    fn recovery_roundtrip() {
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = (i * 7 + 3) as u8;
        }
        let r = recovery_key(&key);
        assert!(r.starts_with("TP1-"));
        assert_eq!(*parse_recovery(&r).unwrap(), key);
        assert_eq!(*parse_recovery(&r.to_lowercase().replace("tp1-", "TP1-")).unwrap(), key);
    }

    #[test]
    fn key_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("tp-crypto-{}", crate::util::id()));
        let (key, store) = create_master(&dir).unwrap();
        if store == KeyStore::File {
            assert_eq!(*load_master(&dir).unwrap(), *key);
        }
        std::fs::remove_dir_all(dir).ok();
    }
}
