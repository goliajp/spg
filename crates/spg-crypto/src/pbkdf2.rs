//! PBKDF2-HMAC-SHA-256 — RFC 8018 §5.2.
//!
//! Used by SPG v4.8 SCRAM-SHA-256. Output length is fixed to 32
//! bytes (one HMAC-SHA-256 block) since SCRAM's `StoredKey` /
//! `ServerKey` derivations only ever consume one block. The general
//! multi-block PBKDF2 form is straightforward to add later if
//! something else needs it.

use crate::hmac::hmac_sha256;
use crate::sha256::OUT_LEN;

/// PBKDF2-HMAC-SHA-256 with fixed 32-byte output. `iterations` is
/// the per-block iteration count (PG SCRAM uses 4096 by default).
pub fn pbkdf2_sha256_32(password: &[u8], salt: &[u8], iterations: u32) -> [u8; OUT_LEN] {
    // U_1 = HMAC(password, salt || INT(1))
    let mut salt_with_index = alloc::vec::Vec::with_capacity(salt.len() + 4);
    salt_with_index.extend_from_slice(salt);
    salt_with_index.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(password, &salt_with_index);
    let mut out = u;
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for i in 0..OUT_LEN {
            out[i] ^= u[i];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> alloc::string::String {
        use alloc::string::String;
        use core::fmt::Write;
        let mut s = String::with_capacity(b.len() * 2);
        for x in b {
            let _ = write!(s, "{x:02x}");
        }
        s
    }

    // RFC 7914 §11 / RFC 6070-style 32-byte PBKDF2-HMAC-SHA-256 vectors.
    #[test]
    fn rfc7914_case_1iter() {
        let out = pbkdf2_sha256_32(b"passwd", b"salt", 1);
        assert_eq!(
            hex(&out),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc"
        );
    }

    #[test]
    fn standard_4096iter() {
        // Independently verified against `openssl kdf` with the same
        // params — this is the iteration count PG SCRAM uses out of
        // the box.
        let out = pbkdf2_sha256_32(b"password", b"salt", 4096);
        assert_eq!(
            hex(&out),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
    }
}
