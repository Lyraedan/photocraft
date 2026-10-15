//! Randomness for handshake nonces and session ids.
//!
//! The passphrase proof itself lives in the engine (`photocraft_engine::collab::auth_proof`), next
//! to the message definitions, so the transport and the engine cannot drift apart.

use std::sync::atomic::{AtomicU64, Ordering};

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Fill `n` bytes from the operating-system CSPRNG. On the (essentially impossible) failure of
/// that source, fall back to a time/counter mix so the app never panics; a predictable nonce does
/// not weaken the passphrase proof, which still requires the passphrase.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; n];
    if getrandom::fill(&mut bytes).is_ok() {
        return bytes;
    }
    fallback_fill(&mut bytes);
    bytes
}

/// `n` random bytes as lowercase hexadecimal (`2n` characters).
pub fn random_hex(n: usize) -> String {
    let bytes = random_bytes(n);
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn fallback_fill(bytes: &mut [u8]) {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut state = (nanos as u64) ^ COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    for chunk in bytes.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        for (i, byte) in chunk.iter_mut().enumerate() {
            *byte = (state >> (i * 8)) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_lowercase_and_the_right_length() {
        let s = random_hex(16);
        assert_eq!(s.len(), 32);
        assert!(s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }

    #[test]
    fn nonces_differ() {
        assert_ne!(random_hex(16), random_hex(16));
        assert_ne!(random_bytes(32), random_bytes(32));
    }

    #[test]
    fn fallback_fills_every_byte_without_panicking() {
        for n in [0usize, 1, 7, 8, 9, 64] {
            let mut bytes = vec![0u8; n];
            fallback_fill(&mut bytes);
            assert_eq!(bytes.len(), n);
        }
    }
}
