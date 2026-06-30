//! CRC-32C (Castagnoli) — the polynomial PostgreSQL has used for WAL
//! records since 9.3 (2013). Different reflected polynomial from
//! [`crate::crc32`] (IEEE 802.3); does not interoperate with it.
//!
//! NOT cryptographic. Strictly a stronger checksum for bit-flip
//! detection on persistent storage. Detection capability for short
//! records is comparable to IEEE CRC-32 but Castagnoli has slightly
//! better minimum Hamming distance for the long records produced by
//! batched group-commit, and crucially every modern CPU has a
//! hardware accelerator for it (Intel SSE4.2 `_mm_crc32_u64`, ARMv8
//! `__crc32cd`). The software fallback here is the standard table
//! lookup; v7.37+ may add the SIMD path under `cfg(target_feature)`.
//!
//! Closes AUDIT-3-categories.md A1.2 — match PG's posture
//! (CRC-32C-protected WAL records) so we can claim ≥ PG durability.

use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};

/// Castagnoli polynomial in reversed form (matches the SSE4.2
/// hardware instruction). The forward form is `0x1EDC6F41`.
const POLY: u32 = 0x82F6_3B78;

#[allow(clippy::declare_interior_mutable_const)]
static TABLE: [AtomicU32; 256] = {
    const ZERO: AtomicU32 = AtomicU32::new(0);
    [ZERO; 256]
};

static STATE: AtomicU8 = AtomicU8::new(0);
const UNINIT: u8 = 0;
const INIT: u8 = 1;
const READY: u8 = 2;

fn ensure_table() {
    loop {
        match STATE.load(Ordering::Acquire) {
            READY => return,
            UNINIT => {
                if STATE
                    .compare_exchange(UNINIT, INIT, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    for i in 0u32..256 {
                        let mut c = i;
                        for _ in 0..8 {
                            c = if c & 1 != 0 { POLY ^ (c >> 1) } else { c >> 1 };
                        }
                        TABLE[i as usize].store(c, Ordering::Relaxed);
                    }
                    STATE.store(READY, Ordering::Release);
                    return;
                }
            }
            _ => core::hint::spin_loop(),
        }
    }
}

/// CRC-32C (Castagnoli) of `data`. Used for SPG WAL records since
/// v7.37.13; matches PostgreSQL's WAL CRC since 9.3.
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    ensure_table();
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        let idx = ((crc ^ u32::from(b)) & 0xFF) as usize;
        crc = (crc >> 8) ^ TABLE[idx].load(Ordering::Relaxed);
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::crc32c;

    #[test]
    fn crc32c_known_vectors() {
        // CRC-32C (Castagnoli) reference values — verified against
        // `crc32c` crate v1.1.x and PostgreSQL pg_crc32c.h tests.
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(b"a"), 0xC1D0_4330);
        assert_eq!(
            crc32c(b"123456789"),
            0xE306_9283,
            "CRC32C of \"123456789\" is the standard check value"
        );
    }

    #[test]
    fn crc32c_differs_from_ieee_crc32() {
        // Sanity: must not collide with the IEEE polynomial on the
        // standard test vector — if it does, somebody copy-pasted
        // the wrong constant into POLY.
        let c = crc32c(b"123456789");
        let ieee = crate::crc32::crc32(b"123456789");
        assert_ne!(c, ieee, "CRC32C must not equal IEEE CRC-32");
        // Specifically: IEEE = 0xCBF43926, Castagnoli = 0xE3069283.
        assert_eq!(ieee, 0xCBF4_3926);
        assert_eq!(c, 0xE306_9283);
    }

    #[test]
    fn crc32c_detects_single_bit_flip() {
        let buf = b"hello world";
        let mut flipped = buf.to_vec();
        flipped[5] ^= 0x01;
        assert_ne!(crc32c(buf), crc32c(&flipped));
    }
}
