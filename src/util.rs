use anyhow::Result;
use futures_util::{Stream, StreamExt};
use rand::Rng;
use std::time::Duration;

fn random_from(chars: &[u8], len: usize) -> String {
    let mut rng = rand::rng();
    (0..len).map(|_| chars[rng.random_range(0..chars.len())] as char).collect()
}

/// Short random id like `k3j2h1x9q0ab`.
pub fn id() -> String {
    random_string(12)
}

/// Random lowercase letters and digits.
pub fn random_string(len: usize) -> String {
    random_from(b"0123456789abcdefghijklmnopqrstuvwxyz", len)
}

/// Random token with mixed case for keys and session ids.
pub fn token(len: usize) -> String {
    random_from(b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", len)
}

pub fn sha256_hex(data: impl AsRef<[u8]>) -> String {
    hex::encode(ring::digest::digest(&ring::digest::SHA256, data.as_ref()))
}

pub fn hmac_sha256(key: impl AsRef<[u8]>, data: impl AsRef<[u8]>) -> [u8; 32] {
    let tag = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key.as_ref()), data.as_ref());
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

/// Percent-encoding for a URL path segment or query value: everything but A-Z a-z 0-9 - _ . ~.
pub fn urlencode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// An HTTP client builder that names Tugport and gives up after `secs`.
pub fn http(secs: u64) -> reqwest::ClientBuilder {
    reqwest::Client::builder().user_agent(concat!("tugport/", env!("CARGO_PKG_VERSION"))).timeout(Duration::from_secs(secs))
}

/// Stream a response into `path`; returns the SHA-256 (hex) of what was written.
pub async fn download(resp: reqwest::Response, path: &std::path::Path) -> Result<String> {
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::File::create(path).await?;
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut stream = resp.error_for_status()?.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hash.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(hex::encode(hash.finish()))
}

/// A broadcast channel as a stream; messages missed by a slow reader are skipped.
pub fn subscribe<T: Clone + Send + 'static>(rx: tokio::sync::broadcast::Receiver<T>) -> impl Stream<Item = T> + Send + 'static {
    futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(v) => return Some((v, rx)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

/// Run `tick` every `secs` seconds (the first time after one wait), logging what fails.
/// Each tick's future owns what it uses: one borrowing from the closure would not be `Send`.
pub async fn every<F: Future<Output = Result<()>>>(secs: u64, what: &str, mut tick: impl FnMut() -> F) {
    loop {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        if let Err(e) = tick().await {
            tracing::warn!("{what}: {e:#}");
        }
    }
}

/// Write a file only root can read (0600), replacing what was there.
pub fn write_private(path: &std::path::Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    // The mode above only applies to a new file.
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(data)?;
    Ok(())
}

/// Ask a question on the terminal; the answer trimmed and lowercased.
pub fn prompt(question: &str) -> std::io::Result<String> {
    use std::io::{BufRead, Write};
    print!("{question} ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_lowercase())
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// DNS-safe lowercase name: letters, digits and dashes, starting with a letter.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 40
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && !name.ends_with('-')
}

/// Lock a mutex, recovering the data if a panicking thread poisoned it.
pub fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn read<T>(l: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

pub fn write<T>(l: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

/// `rel` (a path from a service's settings) under `base`: never absolute or with `..`, so it
/// cannot leave `base`. Checkouts are free of symlinks that lead out (`git::check_links`).
pub fn inside(base: &std::path::Path, rel: &str) -> Option<std::path::PathBuf> {
    use std::path::Component;
    let rel = std::path::Path::new(rel.trim_start_matches('/'));
    rel.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)).then(|| base.join(rel))
}

/// A response body, refused past `limit` bytes (for answers from servers Tugport does not run).
pub async fn read_capped(resp: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk?);
        if body.len() > limit {
            anyhow::bail!("the answer is larger than {} KB", limit / 1024);
        }
    }
    Ok(body)
}

pub fn ct_eq(a: impl AsRef<[u8]>, b: impl AsRef<[u8]>) -> bool {
    use subtle::ConstantTimeEq;
    let (a, b) = (a.as_ref(), b.as_ref());
    a.len() == b.len() && a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_name("api"));
        assert!(valid_name("my-app-2"));
        assert!(!valid_name("2app"));
        assert!(!valid_name("App"));
        assert!(!valid_name("app-"));
        assert!(!valid_name(""));
        assert!(!valid_name("a_b"));
    }

    #[test]
    fn paths_stay_inside() {
        let base = std::path::Path::new("/w/repo");
        assert_eq!(inside(base, "").as_deref(), Some(base));
        assert_eq!(inside(base, "/api/").unwrap(), base.join("api"));
        assert_eq!(inside(base, "./a/Dockerfile").unwrap(), base.join("a/Dockerfile"));
        assert!(inside(base, "../x").is_none());
        assert!(inside(base, "a/../../x").is_none());
        assert!(inside(base, "//etc/../..").is_none());
    }

    #[test]
    fn hashes() {
        // RFC 4231 test case 2.
        assert_eq!(hex::encode(hmac_sha256("Jefe", "what do ya want for nothing?")), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        assert_eq!(sha256_hex("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(urlencode("a b/c~"), "a%20b%2Fc~");
    }

    #[test]
    fn ids_are_random() {
        assert_ne!(id(), id());
        assert_eq!(id().len(), 12);
    }
}
