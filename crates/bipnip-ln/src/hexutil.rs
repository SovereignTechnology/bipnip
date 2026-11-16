//! Strict lower-case hex, and the argument checks every backend shares.

use crate::BackendError;

/// Lower-case hex of `bytes`.
pub fn encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 15)]));
    }
    out
}

/// Decodes lower-case hex; `None` on odd length, upper case or any other character.
pub fn decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    s.as_bytes()
        .chunks(2)
        .map(|p| Some((digit(p[0])? << 4) | digit(p[1])?))
        .collect()
}

/// A 32-byte value (payment hash or preimage) from 64 lower-case hex characters.
pub fn hash32(s: &str, what: &'static str) -> Result<Vec<u8>, BackendError> {
    match decode(s) {
        Some(b) if b.len() == 32 => Ok(b),
        _ => Err(BackendError::Rejected(format!(
            "{what} must be 64 lower-case hex characters"
        ))),
    }
}

/// A compressed node public key from 66 lower-case hex characters (02… or 03…).
pub fn node_key(s: &str) -> Result<Vec<u8>, BackendError> {
    match decode(s) {
        Some(b) if b.len() == 33 && (b[0] == 2 || b[0] == 3) => Ok(b),
        _ => Err(BackendError::Rejected(
            "node pubkey must be 66 lower-case hex characters".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_is_strict() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&bytes)), Some(bytes));
        assert_eq!(decode("AB"), None);
        assert_eq!(decode("abc"), None);
        assert_eq!(decode("zz"), None);
        assert_eq!(decode(""), Some(vec![]));
    }

    #[test]
    fn checks_lengths_and_key_prefix() {
        assert!(hash32(&"ab".repeat(32), "hash").is_ok());
        assert!(hash32(&"ab".repeat(31), "hash").is_err());
        assert!(node_key(&format!("02{}", "11".repeat(32))).is_ok());
        assert!(node_key(&format!("04{}", "11".repeat(32))).is_err());
        assert!(node_key(&"02".repeat(32)).is_err());
    }
}
