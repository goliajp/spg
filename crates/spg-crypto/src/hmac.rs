//! HMAC-SHA-256 — self-built per RFC 2104.
//!
//! Used by SPG v4.8 SCRAM-SHA-256. Block-aligned key handling lifted
//! straight from §2 of the RFC: keys longer than the block are
//! hashed; shorter keys are zero-padded; the inner / outer pads are
//! XOR'd block-wide.

use crate::sha256::{self, BLOCK_LEN, OUT_LEN};

/// HMAC-SHA-256(key, message) → 32 bytes.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; OUT_LEN] {
    let mut block_key = [0u8; BLOCK_LEN];
    if key.len() > BLOCK_LEN {
        block_key[..OUT_LEN].copy_from_slice(&sha256::hash(key));
    } else {
        block_key[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0u8; BLOCK_LEN];
    let mut opad = [0u8; BLOCK_LEN];
    for i in 0..BLOCK_LEN {
        ipad[i] = block_key[i] ^ 0x36;
        opad[i] = block_key[i] ^ 0x5c;
    }
    // SHA256(opad || SHA256(ipad || message))
    let mut inner_input = alloc::vec::Vec::with_capacity(BLOCK_LEN + message.len());
    inner_input.extend_from_slice(&ipad);
    inner_input.extend_from_slice(message);
    let inner = sha256::hash(&inner_input);
    let mut outer_input = alloc::vec::Vec::with_capacity(BLOCK_LEN + OUT_LEN);
    outer_input.extend_from_slice(&opad);
    outer_input.extend_from_slice(&inner);
    sha256::hash(&outer_input)
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

    // RFC 4231 test vectors.
    #[test]
    fn rfc4231_case_1() {
        let key = [0x0bu8; 20];
        let msg = b"Hi There";
        assert_eq!(
            hex(&hmac_sha256(&key, msg)),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn rfc4231_case_2() {
        // key = "Jefe"
        let msg = b"what do ya want for nothing?";
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", msg)),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn rfc4231_case_3() {
        let key = [0xaau8; 20];
        let msg = [0xddu8; 50];
        assert_eq!(
            hex(&hmac_sha256(&key, &msg)),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
    }

    #[test]
    fn rfc4231_case_7_long_key_long_data() {
        // 131-byte key (triggers the long-key SHA-256 prehash branch)
        let key = [0xaau8; 131];
        let msg = b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm.";
        assert_eq!(
            hex(&hmac_sha256(&key, msg)),
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
        );
    }
}
