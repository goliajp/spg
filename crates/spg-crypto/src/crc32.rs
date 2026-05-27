//! IEEE 802.3 CRC-32 (the "zip / png / ethernet" variant).
//!
//! NOT cryptographic. v4.37 uses it as a fast checksum on persistent
//! storage envelopes (catalog snapshot, WAL records, backup bundles)
//! so bit-flips surface as explicit "CRC mismatch" errors instead of
//! deserializing into silent corruption.
//!
//! Implementation: standard polynomial `0xEDB88320`, byte-at-a-time
//! table lookup. The 256-entry table is built at first call into an
//! array of `AtomicU32`; thread-safe and zero-allocation after that.

use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};

const POLY: u32 = 0xEDB8_8320;

/// Precomputed table (filled lazily by `ensure_table`). `AtomicU32`
/// is `Sync` and gives us interior mutability without an
/// `UnsafeCell` dance. The `[ZERO; 256]` initialization
/// deliberately gets 256 distinct atomics — the
/// declare-interior-mutable-const lint warns about the const-as-
/// expression footgun (where two readers would share the same
/// underlying value), which doesn't apply here.
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

/// CRC-32 of `data`. Standard IEEE polynomial; the "zip / png /
/// ethernet" check value.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
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
    use super::crc32;

    #[test]
    fn crc32_known_vectors() {
        // CRC-32 IEEE (poly `0xEDB88320`) reference values.
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"a"), 0xE8B7_BE43);
        assert_eq!(crc32(b"abc"), 0x3524_41C2);
        assert_eq!(
            crc32(b"123456789"),
            0xCBF4_3926,
            "CRC32 of \"123456789\" is the standard check value"
        );
    }

    #[test]
    fn crc32_detects_single_bit_flip() {
        let buf = b"hello world";
        let mut flipped = buf.to_vec();
        flipped[5] ^= 0x01;
        assert_ne!(crc32(buf), crc32(&flipped));
    }
}
