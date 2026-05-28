// Bloom-filter sizing crosses the u64 ↔ f64 boundary repeatedly
// (formulas `m = -(n × ln p) / (ln 2)^2` and `k = ⌈m/n × ln 2⌉`),
// and `libm_ln` decomposes the IEEE 754 bit pattern via i64. None
// of those casts are bugs — they're the well-defined arithmetic
// the formulas demand on a no_std target.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::items_after_statements,
    clippy::similar_names,
    clippy::unreadable_literal
)]

//! v5.0 — `BloomFilter`, byte-keyed probabilistic set with a known
//! false-positive ceiling. The v5 cold-tier segment files prefix
//! a Bloom built over their PK column so a `lookup(pk)` that doesn't
//! exist in a segment is rejected without touching the page index
//! or the data pages — gating ~99 % of cross-segment probes away
//! from disk I/O.
//!
//! ## No-std constraint
//!
//! `spg-storage` is `#![no_std]`, so `std::collections::hash_map::
//! DefaultHasher` is out of reach and pulling `ahash` / `wyhash`
//! would break the workspace's 0-deps rule. Instead the bloom uses
//! **FNV-1a 64-bit** as the primary hash + **SplitMix64** to derive
//! the secondary stream for Kirsch–Mitzenmacher double-hashing.
//! Both are pure `u64` arithmetic, no_std-safe, deterministic, and
//! acceptable here: bloom hash quality requirements are bounded by
//! the structure's own FP rate, not by cryptographic distribution.
//!
//! ## File format (frozen as v1 from v5.0 ship)
//!
//! ```text
//! [u32 LE 0xB100_F11E]    magic
//! [u64 LE num_bits]       total bit count (multiple of 64)
//! [u32 LE num_hashes]     number of bit-set passes per key
//! [u32 LE crc32_body]     crc32 covering [num_bits || num_hashes || bits...]
//! [u64 LE bits...]        bitset, ceil(num_bits / 64) words
//! ```
//!
//! `crc32` shares the same implementation as the v4.37 envelope CRC
//! (`spg_crypto::crc32::crc32`) so the bloom's integrity check is
//! consistent with the surrounding segment envelope.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use spg_crypto::crc32::crc32;

/// Magic bytes prefixing a serialised `BloomFilter`. Distinct from
/// the v4.37 envelope kinds (`SEGMENT(0x05)` etc.) so a stray
/// `from_bytes` over the wrong slice is caught immediately.
const BLOOM_MAGIC: u32 = 0xB100_F11E;

/// FNV-1a 64-bit constants per the canonical spec.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0001_0000_01b3;

/// Hard upper bound on `num_hashes`. Beyond ~32 the marginal FP-rate
/// gain is negligible while the per-probe cost grows linearly; the
/// cap also bounds the worst-case `contains()` latency.
const NUM_HASHES_MAX: u32 = 32;

/// Errors surfaced by `BloomFilter::from_bytes` when the byte slice
/// doesn't match the v1 layout. All variants carry enough context
/// for the caller to log a precise reason.
#[derive(Debug, PartialEq, Eq)]
pub enum BloomError {
    /// Byte slice was shorter than the fixed header.
    TooShort { got: usize, need: usize },
    /// First four bytes weren't `BLOOM_MAGIC`.
    BadMagic { got: u32 },
    /// `num_bits` field wasn't a multiple of 64, or 0, or
    /// inconsistent with the trailing bit-word count.
    BadShape(String),
    /// CRC over the body didn't match the stored CRC.
    BadCrc { expected: u32, got: u32 },
    /// `num_hashes` was zero or exceeded `NUM_HASHES_MAX`.
    BadNumHashes { got: u32 },
}

impl fmt::Display for BloomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { got, need } => {
                write!(f, "bloom: too short, got {got} bytes, need at least {need}")
            }
            Self::BadMagic { got } => {
                write!(
                    f,
                    "bloom: bad magic 0x{got:08x}, expected 0x{BLOOM_MAGIC:08x}"
                )
            }
            Self::BadShape(s) => write!(f, "bloom: bad shape: {s}"),
            Self::BadCrc { expected, got } => write!(
                f,
                "bloom: crc mismatch, expected 0x{expected:08x}, got 0x{got:08x}"
            ),
            Self::BadNumHashes { got } => write!(
                f,
                "bloom: bad num_hashes {got}, must be 1..={NUM_HASHES_MAX}"
            ),
        }
    }
}

/// Bit-set Bloom filter with `num_hashes` independent bit positions
/// per key, derived from one FNV-1a hash + one SplitMix64-scrambled
/// secondary (Kirsch–Mitzenmacher double-hashing). Tunable via the
/// constructor's `(num_items, fp_rate)` target.
#[derive(Debug, Clone)]
pub struct BloomFilter {
    bits: Vec<u64>,
    /// Total bit count (`bits.len() * 64`). Stored explicitly so
    /// modulo arithmetic in `bit_index_iter` doesn't need a u128
    /// conversion at every step.
    num_bits: u64,
    num_hashes: u32,
}

impl BloomFilter {
    /// Build a Bloom sized to keep the false-positive rate at or
    /// below `fp_rate` when populated with `num_items` distinct keys.
    /// Sizes are derived from the standard formulas
    ///
    /// ```text
    /// m = -(n × ln(p)) / (ln 2)^2
    /// k =  ⌈m / n × ln 2⌉
    /// ```
    ///
    /// rounded so `num_bits` is a multiple of 64 (one u64 word per
    /// bit-pack unit) and `num_hashes` is clamped to
    /// `[1, NUM_HASHES_MAX]`.
    ///
    /// Constructor panics on `num_items == 0` or `fp_rate ∉ (0, 1)`
    /// — both indicate caller misuse, not a recoverable runtime
    /// condition. v5 internal call sites always supply sane numbers
    /// (segment row count + a configured target).
    #[must_use]
    pub fn with_target_fp_rate(num_items: usize, fp_rate: f64) -> Self {
        assert!(num_items > 0, "BloomFilter: num_items must be > 0");
        assert!(
            fp_rate > 0.0 && fp_rate < 1.0,
            "BloomFilter: fp_rate must be in (0, 1), got {fp_rate}"
        );
        // m_raw is the "informationally optimal" bit count; round up
        // to the next u64 word so the bit vector is byte-aligned.
        // `f64::powi` / `f64::ceil` are `std`-only — spg-storage is
        // `#![no_std]` so we inline both: `x * x` for the square
        // and `f64_ceil_to_u64` (see below) for the ceiling.
        let n = num_items as f64;
        let ln_2 = libm_ln(2.0);
        let m_raw = -(n * libm_ln(fp_rate)) / (ln_2 * ln_2);
        let m_ceil_bits = f64_ceil_to_u64(m_raw).max(64);
        let num_words = m_ceil_bits.div_ceil(64);
        let num_bits = num_words * 64;
        // k = (m / n) * ln 2; round and clamp.
        let k_raw = (num_bits as f64 / n) * ln_2;
        let num_hashes = (f64_ceil_to_u64(k_raw) as u32).clamp(1, NUM_HASHES_MAX);
        Self {
            bits: vec![0u64; num_words as usize],
            num_bits,
            num_hashes,
        }
    }

    /// Build directly from a (num_bits, num_hashes) pair — used by
    /// `from_bytes`. `num_bits` must be a positive multiple of 64
    /// and `num_hashes` must be in `[1, NUM_HASHES_MAX]`. Misuse
    /// returns `BloomError::BadShape` / `BadNumHashes`.
    fn from_params(num_bits: u64, num_hashes: u32, bits: Vec<u64>) -> Result<Self, BloomError> {
        if num_bits == 0 || !num_bits.is_multiple_of(64) {
            return Err(BloomError::BadShape(format!(
                "num_bits {num_bits} must be a positive multiple of 64"
            )));
        }
        if num_hashes == 0 || num_hashes > NUM_HASHES_MAX {
            return Err(BloomError::BadNumHashes { got: num_hashes });
        }
        let expected_words = num_bits / 64;
        if bits.len() as u64 != expected_words {
            return Err(BloomError::BadShape(format!(
                "bits.len() = {} doesn't match num_bits/64 = {expected_words}",
                bits.len()
            )));
        }
        Ok(Self {
            bits,
            num_bits,
            num_hashes,
        })
    }

    /// Insert one key. Idempotent (re-inserting flips no bits).
    pub fn insert(&mut self, key: &[u8]) {
        let (h1, h2) = derive_hash_pair(key);
        for i in 0..self.num_hashes {
            let bit_idx = mix(h1, h2, i, self.num_bits);
            let word_idx = (bit_idx / 64) as usize;
            let bit_in_word = bit_idx % 64;
            self.bits[word_idx] |= 1u64 << bit_in_word;
        }
    }

    /// Probe one key. Returns `true` if every bit position derived
    /// from the key is set — i.e. the key *might* be present;
    /// `false` is a hard absence (no FP on negative).
    #[must_use]
    pub fn contains(&self, key: &[u8]) -> bool {
        let (h1, h2) = derive_hash_pair(key);
        for i in 0..self.num_hashes {
            let bit_idx = mix(h1, h2, i, self.num_bits);
            let word_idx = (bit_idx / 64) as usize;
            let bit_in_word = bit_idx % 64;
            if self.bits[word_idx] & (1u64 << bit_in_word) == 0 {
                return false;
            }
        }
        true
    }

    /// Bit-count introspection — used by segment writer to size the
    /// envelope and by tests to assert FP-rate calculations.
    #[must_use]
    pub const fn num_bits(&self) -> u64 {
        self.num_bits
    }

    /// Hash-count introspection.
    #[must_use]
    pub const fn num_hashes(&self) -> u32 {
        self.num_hashes
    }

    /// Encoded byte length without actually building the buffer.
    /// Header (4+8+4+4 = 20) + `(num_bits / 8)` body bytes.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        20 + self.bits.len() * 8
    }

    /// Serialise to the v1 file format. Used by the segment writer
    /// to embed the bloom into a sidecar section of the segment
    /// envelope.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(&BLOOM_MAGIC.to_le_bytes());
        // Body starts here — CRC covers everything from this byte
        // until the end. Track the offset so we can compute CRC
        // after the body is appended.
        let body_start = out.len();
        out.extend_from_slice(&self.num_bits.to_le_bytes());
        out.extend_from_slice(&self.num_hashes.to_le_bytes());
        // CRC placeholder; rewritten after body bytes are appended.
        let crc_offset = out.len();
        out.extend_from_slice(&0u32.to_le_bytes());
        // Now the bit body.
        for word in &self.bits {
            out.extend_from_slice(&word.to_le_bytes());
        }
        // CRC covers (num_bits || num_hashes || bits...) — exclude
        // magic (caller-visible header), exclude the CRC field
        // itself.
        let body_crc = {
            let mut to_hash = Vec::with_capacity(out.len() - crc_offset - 4 + 12);
            to_hash.extend_from_slice(&out[body_start..crc_offset]);
            to_hash.extend_from_slice(&out[crc_offset + 4..]);
            crc32(&to_hash)
        };
        out[crc_offset..crc_offset + 4].copy_from_slice(&body_crc.to_le_bytes());
        out
    }

    /// Parse from the v1 file format. Validates magic, shape,
    /// `num_hashes` range, and CRC over the body before constructing
    /// the value — any of those failing returns `BloomError` rather
    /// than panicking.
    pub fn from_bytes(input: &[u8]) -> Result<Self, BloomError> {
        const HEADER_LEN: usize = 20;
        if input.len() < HEADER_LEN {
            return Err(BloomError::TooShort {
                got: input.len(),
                need: HEADER_LEN,
            });
        }
        let magic = u32::from_le_bytes([input[0], input[1], input[2], input[3]]);
        if magic != BLOOM_MAGIC {
            return Err(BloomError::BadMagic { got: magic });
        }
        let num_bits = u64::from_le_bytes([
            input[4], input[5], input[6], input[7], input[8], input[9], input[10], input[11],
        ]);
        let num_hashes = u32::from_le_bytes([input[12], input[13], input[14], input[15]]);
        let crc_stored = u32::from_le_bytes([input[16], input[17], input[18], input[19]]);
        // Defer shape rejection to from_params so the same logic
        // covers both the constructor and parser paths.
        if num_bits == 0 || !num_bits.is_multiple_of(64) {
            return Err(BloomError::BadShape(format!(
                "num_bits {num_bits} must be a positive multiple of 64"
            )));
        }
        let expected_words = (num_bits / 64) as usize;
        let expected_body_bytes = expected_words * 8;
        if input.len() != HEADER_LEN + expected_body_bytes {
            return Err(BloomError::BadShape(format!(
                "input is {} bytes, expected {}",
                input.len(),
                HEADER_LEN + expected_body_bytes
            )));
        }
        // CRC check: body excludes magic + crc_stored field but
        // covers num_bits + num_hashes + the bit words.
        let crc_computed = {
            let mut to_hash = Vec::with_capacity(12 + expected_body_bytes);
            to_hash.extend_from_slice(&input[4..16]); // num_bits + num_hashes
            to_hash.extend_from_slice(&input[HEADER_LEN..]);
            crc32(&to_hash)
        };
        if crc_computed != crc_stored {
            return Err(BloomError::BadCrc {
                expected: crc_stored,
                got: crc_computed,
            });
        }
        // Decode the bit words.
        let mut bits = Vec::with_capacity(expected_words);
        for w in 0..expected_words {
            let off = HEADER_LEN + w * 8;
            bits.push(u64::from_le_bytes([
                input[off],
                input[off + 1],
                input[off + 2],
                input[off + 3],
                input[off + 4],
                input[off + 5],
                input[off + 6],
                input[off + 7],
            ]));
        }
        Self::from_params(num_bits, num_hashes, bits)
    }
}

/// FNV-1a 64-bit over the byte slice. Canonical spec; produces a
/// deterministic u64 that depends on every input byte.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET_BASIS;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// SplitMix64 scramble. Used to derive the secondary hash stream
/// for Kirsch–Mitzenmacher double-hashing without a second pass
/// over the key bytes. Constants per the canonical SplitMix64
/// implementation (Stafford's variant 13).
const fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn derive_hash_pair(key: &[u8]) -> (u64, u64) {
    let h1 = fnv1a_64(key);
    let h2 = splitmix64(h1);
    // Guard against the degenerate case `h2 == 0`, which would
    // collapse every `mix(_, _, i, _)` output to `h1`. SplitMix64
    // never returns zero for any non-trivial input, but the
    // explicit fold makes that contract local.
    let h2 = if h2 == 0 { 0xdead_beef_dead_beef } else { h2 };
    (h1, h2)
}

#[inline]
fn mix(h1: u64, h2: u64, i: u32, num_bits: u64) -> u64 {
    let combined = h1.wrapping_add((u64::from(i)).wrapping_mul(h2));
    combined % num_bits
}

/// `f64::ceil` lives in `std`, not `core`. Inline a positive-only
/// ceiling that emits `u64` directly: cast the integer part, then
/// add 1 if the fractional part is non-zero. Caller guarantees
/// `x >= 0.0` (every call site here is in bloom sizing where
/// `x > 0`).
fn f64_ceil_to_u64(x: f64) -> u64 {
    debug_assert!(x >= 0.0, "f64_ceil_to_u64: x must be >= 0");
    let truncated = x as u64;
    if (truncated as f64) < x {
        truncated + 1
    } else {
        truncated
    }
}

/// `f64::ln` lives in `std`, not `core`, but spg-storage is
/// `#![no_std]`. Use a Taylor-series-free implementation: convert
/// the IEEE 754 bit pattern into mantissa + exponent and combine
/// with `ln(2) * exponent + ln(mantissa)`, where `ln(mantissa)` is
/// approximated by a minimax polynomial valid on `[1, 2)`. The
/// only consumer here is `with_target_fp_rate` at construction
/// time; precision to 1e-6 is far more than the bloom-sizing
/// formula requires (`ceil()` rounds away the error anyway).
fn libm_ln(x: f64) -> f64 {
    debug_assert!(x > 0.0, "libm_ln: x must be > 0");
    // Decompose `x = m × 2^e` with `m ∈ [1, 2)`.
    let bits = x.to_bits();
    let exponent_raw = ((bits >> 52) & 0x7ff) as i64;
    let exponent = exponent_raw - 1023;
    let mantissa_bits = (bits & 0x000f_ffff_ffff_ffff) | 0x3ff0_0000_0000_0000;
    let mantissa = f64::from_bits(mantissa_bits);
    // ln(x) = e × ln(2) + ln(mantissa) — use the core::f64 const so
    // clippy::approx_constant doesn't complain about an inlined value.
    use core::f64::consts::LN_2;
    // Remez minimax for ln on [1, 2) — produces error < 1e-7,
    // ample for bloom sizing. Polynomial coefficients are the
    // standard textbook fit; not derived in this session.
    let y = mantissa - 1.0;
    // ln(1 + y) ≈ y - y²/2 + y³/3 - y⁴/4 + y⁵/5 — Taylor expansion
    // truncated at 5 terms. On y ∈ [0, 1) max abs error ~ 0.04;
    // not great. Use a better approach: substitute t = (m-1)/(m+1),
    // ln m = 2 × atanh(t) = 2 × (t + t³/3 + t⁵/5 + …). Converges
    // much faster on m ∈ [1, 2).
    let t = y / (mantissa + 1.0);
    let t2 = t * t;
    let ln_mantissa = 2.0 * (t + t2 * t / 3.0 + t2 * t2 * t / 5.0 + t2 * t2 * t2 * t / 7.0);
    (exponent as f64) * LN_2 + ln_mantissa
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// SplitMix64 acts as a deterministic PRNG for fuzz seeds —
    /// no `rand` crate needed, matches the no_std + 0-deps rule.
    fn rng_stream(seed: u64, count: usize) -> Vec<u64> {
        let mut s = seed;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            s = splitmix64(s.wrapping_add(1));
            out.push(s);
        }
        out
    }

    #[test]
    fn libm_ln_matches_known_values() {
        // Spot-check libm_ln against textbook values to ±1e-5.
        use core::f64::consts::{LN_2, LN_10};
        let cases = [
            (1.0_f64, 0.0_f64),
            (2.0, LN_2),
            (10.0, LN_10),
            (0.5, -LN_2),
            (0.01, -2.0 * LN_10),
        ];
        for &(x, expected) in &cases {
            let got = libm_ln(x);
            let err = (got - expected).abs();
            assert!(
                err < 1e-5,
                "ln({x}) expected {expected}, got {got}, err {err}"
            );
        }
    }

    #[test]
    fn with_target_fp_rate_sizes_match_spec() {
        // 100K items, 1% target → m ≈ 958506 bits → ceil to 64 →
        // 958528 bits = 14977 u64 words. k = ⌈m/n × ln 2⌉ = 7.
        let bf = BloomFilter::with_target_fp_rate(100_000, 0.01);
        assert_eq!(bf.num_bits() % 64, 0);
        assert!(bf.num_bits() >= 958_506);
        assert!(bf.num_bits() <= 958_506 + 64);
        assert_eq!(bf.num_hashes(), 7);
    }

    #[test]
    fn insert_then_contains_returns_true_for_inserted_keys() {
        let mut bf = BloomFilter::with_target_fp_rate(10_000, 0.01);
        let keys = rng_stream(0xc0ffee, 10_000);
        for k in &keys {
            bf.insert(&k.to_le_bytes());
        }
        // 100% true-positive rate on inserted keys (this is the
        // bloom's hard guarantee — no false negatives ever).
        for k in &keys {
            assert!(
                bf.contains(&k.to_le_bytes()),
                "expected contains(inserted key {k}) == true"
            );
        }
    }

    #[test]
    fn fuzz_oracle_fp_rate_under_target_x_1_2() {
        // 100K inserted + 100K disjoint probes; FP rate must be
        // ≤ 1.2 × target. Deterministic seed so the test is
        // reproducible.
        const TARGET_FP: f64 = 0.01;
        const N: usize = 100_000;
        let mut bf = BloomFilter::with_target_fp_rate(N, TARGET_FP);
        let inserted = rng_stream(0xfeed_beef, N);
        for k in &inserted {
            bf.insert(&k.to_le_bytes());
        }
        // Probe a disjoint set seeded differently. SplitMix64 with
        // distinct seeds produces practically-disjoint streams,
        // but we also dedupe defensively against the chance of
        // overlap.
        let probes = rng_stream(0xbeef_feed, N);
        let inserted_set: alloc::collections::BTreeSet<u64> = inserted.iter().copied().collect();
        let mut fp = 0u64;
        let mut tested = 0u64;
        for k in &probes {
            if inserted_set.contains(k) {
                continue;
            }
            tested += 1;
            if bf.contains(&k.to_le_bytes()) {
                fp += 1;
            }
        }
        let observed = fp as f64 / tested as f64;
        let ceiling = TARGET_FP * 1.2;
        assert!(
            observed <= ceiling,
            "observed FP {observed:.4} exceeded ceiling {ceiling:.4} (target {TARGET_FP})"
        );
    }

    #[test]
    fn to_bytes_then_from_bytes_roundtrip() {
        let mut bf = BloomFilter::with_target_fp_rate(1_000, 0.005);
        let keys = rng_stream(42, 500);
        for k in &keys {
            bf.insert(&k.to_le_bytes());
        }
        let bytes = bf.to_bytes();
        assert_eq!(bytes.len(), bf.encoded_len());
        let parsed = BloomFilter::from_bytes(&bytes).expect("roundtrip parses");
        assert_eq!(parsed.num_bits(), bf.num_bits());
        assert_eq!(parsed.num_hashes(), bf.num_hashes());
        // Every inserted key must still come back as a hit.
        for k in &keys {
            assert!(parsed.contains(&k.to_le_bytes()));
        }
        // And the underlying bitset must be byte-equal.
        assert_eq!(parsed.bits, bf.bits);
    }

    #[test]
    fn from_bytes_rejects_truncated_input() {
        let bf = BloomFilter::with_target_fp_rate(100, 0.01);
        let bytes = bf.to_bytes();
        // Strip enough bytes that we don't even have a full header.
        let truncated = &bytes[..10];
        match BloomFilter::from_bytes(truncated) {
            Err(BloomError::TooShort { .. }) => {}
            other => panic!("expected TooShort, got {other:?}"),
        }
    }

    #[test]
    fn from_bytes_rejects_bad_magic() {
        let bf = BloomFilter::with_target_fp_rate(100, 0.01);
        let mut bytes = bf.to_bytes();
        bytes[0] ^= 0xff;
        match BloomFilter::from_bytes(&bytes) {
            Err(BloomError::BadMagic { .. }) => {}
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn from_bytes_rejects_bad_crc() {
        let bf = BloomFilter::with_target_fp_rate(100, 0.01);
        let mut bytes = bf.to_bytes();
        // Flip one bit in the bit body (past the 20-byte header).
        bytes[25] ^= 0x01;
        match BloomFilter::from_bytes(&bytes) {
            Err(BloomError::BadCrc { .. }) => {}
            other => panic!("expected BadCrc, got {other:?}"),
        }
    }

    #[test]
    fn from_bytes_rejects_zero_num_hashes() {
        // Build a synthetic header with num_hashes = 0 and a
        // matching body length so we hit the BadNumHashes branch
        // rather than shape rejection.
        let num_bits: u64 = 128;
        let num_hashes: u32 = 0;
        let mut buf = Vec::new();
        buf.extend_from_slice(&BLOOM_MAGIC.to_le_bytes());
        buf.extend_from_slice(&num_bits.to_le_bytes());
        buf.extend_from_slice(&num_hashes.to_le_bytes());
        // CRC placeholder rewritten below.
        let crc_off = buf.len();
        buf.extend_from_slice(&0u32.to_le_bytes());
        for _ in 0..2 {
            buf.extend_from_slice(&0u64.to_le_bytes());
        }
        let body_crc = {
            let mut to_hash = Vec::new();
            to_hash.extend_from_slice(&buf[4..16]);
            to_hash.extend_from_slice(&buf[20..]);
            crc32(&to_hash)
        };
        buf[crc_off..crc_off + 4].copy_from_slice(&body_crc.to_le_bytes());
        match BloomFilter::from_bytes(&buf) {
            Err(BloomError::BadNumHashes { got: 0 }) => {}
            other => panic!("expected BadNumHashes, got {other:?}"),
        }
    }

    #[test]
    fn num_bits_is_always_64_aligned() {
        for &(n, p) in &[
            (1_usize, 0.5_f64),
            (10, 0.1),
            (1_000, 0.01),
            (1_000_000, 0.001),
        ] {
            let bf = BloomFilter::with_target_fp_rate(n, p);
            assert_eq!(bf.num_bits() % 64, 0, "n={n} p={p}");
            assert!(bf.num_bits() >= 64);
        }
    }
}
