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
    fn ids_are_random() {
        assert_ne!(id(), id());
        assert_eq!(id().len(), 12);
    }
}
