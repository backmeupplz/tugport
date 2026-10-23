use rand::Rng;
use sha2::{Digest, Sha256};

const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Short random id like `k3j2h1x9q0ab`.
pub fn id() -> String {
    random_string(12)
}

pub fn random_string(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len).map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char).collect()
}

/// Random token with mixed case for keys and session ids.
pub fn token(len: usize) -> String {
    const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::rng();
    (0..len).map(|_| CHARS[rng.random_range(0..CHARS.len())] as char).collect()
}

pub fn sha256_hex(data: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(data.as_ref()))
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

pub fn ct_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && a.as_bytes().ct_eq(b.as_bytes()).into()
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
    fn ids_are_random() {
        assert_ne!(id(), id());
        assert_eq!(id().len(), 12);
    }
}
