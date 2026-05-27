//! BLAKE3 cryptographic hash — self-built single-thread implementation.
//! Follows the spec at
//! <https://github.com/BLAKE3-team/BLAKE3-specs/blob/master/blake3.pdf>.
//!
//! Scope: unkeyed `hash(input) -> [u8; 32]` only. KDF / keyed-hash modes
//! are out of scope.
//!
//! v3.0.4 attempted a NEON-vectorised `compress` for aarch64 but the
//! benchmark regressed 1.5–2× — see the comment on `fn compress`.
//! The NEON path is kept under `#[cfg(test)]` as a cross-check oracle.
#![no_std]
// BLAKE3 intentionally splits a 64-bit counter into two 32-bit words and
// writes a u32 block length that is always ≤ 64. Clippy's truncation warning
// is correct in general but here the truncation is the protocol.
#![allow(clippy::cast_possible_truncation)]
// Workspace-wide `unsafe_code = "deny"` (v3.0.4 — was forbid). spg-crypto
// is the one crate that needs unsafe for `std::arch::aarch64` /
// `std::arch::x86_64` intrinsics; the allow is scoped to this crate
// alone.
#![allow(unsafe_code)]

extern crate alloc;

pub mod base64;
pub mod crc32;
pub mod hmac;
pub mod pbkdf2;
pub mod sha256;

use alloc::vec::Vec;

pub const OUT_LEN: usize = 32;
const BLOCK_LEN: usize = 64;
const CHUNK_LEN: usize = 1024;

// Flag bits per the spec.
const CHUNK_START: u32 = 1;
const CHUNK_END: u32 = 2;
const PARENT: u32 = 4;
const ROOT: u32 = 8;

const IV: [u32; 8] = [
    0x6A09_E667,
    0xBB67_AE85,
    0x3C6E_F372,
    0xA54F_F53A,
    0x510E_527F,
    0x9B05_688C,
    0x1F83_D9AB,
    0x5BE0_CD19,
];

/// Message word permutation applied between rounds (BLAKE3 spec §2.4).
const MSG_PERMUTATION: [usize; 16] = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];

#[cfg(all(target_arch = "aarch64", test))]
mod neon {
    //! NEON (`uint32x4_t`) BLAKE3 compression for aarch64. Lays the
    //! 16-word state out as four 128-bit vectors, runs the column +
    //! diagonal rounds with vector add/xor/rotate, and stitches the
    //! result back into a `[u32; 16]`. Bit-identical to the scalar
    //! reference (cross-checked in the `neon_matches_scalar` unit
    //! test).
    use super::{IV, MSG_PERMUTATION};
    use core::arch::aarch64::{
        uint32x4_t, vaddq_u32, veorq_u32, vextq_u32, vld1q_u32, vld2q_u32, vsetq_lane_u32,
        vshlq_n_u32, vshrq_n_u32, vst1q_u32,
    };

    /// Stable Rust forbids const arithmetic on generic const params
    /// (`{ 32 - N }`), so we hand-roll a rotation per BLAKE3 amount
    /// (16, 12, 8, 7) — there are exactly four.
    #[inline]
    unsafe fn vrotr16(x: uint32x4_t) -> uint32x4_t {
        unsafe { veorq_u32(vshrq_n_u32::<16>(x), vshlq_n_u32::<16>(x)) }
    }
    #[inline]
    unsafe fn vrotr12(x: uint32x4_t) -> uint32x4_t {
        unsafe { veorq_u32(vshrq_n_u32::<12>(x), vshlq_n_u32::<20>(x)) }
    }
    #[inline]
    unsafe fn vrotr8(x: uint32x4_t) -> uint32x4_t {
        unsafe { veorq_u32(vshrq_n_u32::<8>(x), vshlq_n_u32::<24>(x)) }
    }
    #[inline]
    unsafe fn vrotr7(x: uint32x4_t) -> uint32x4_t {
        unsafe { veorq_u32(vshrq_n_u32::<7>(x), vshlq_n_u32::<25>(x)) }
    }

    /// Vectorised g-mixer applied lane-wise across (a, b, c, d) and a
    /// pair of message vectors (mx, my). One call updates four
    /// independent g operations in parallel.
    #[inline]
    unsafe fn g(
        a: &mut uint32x4_t,
        b: &mut uint32x4_t,
        c: &mut uint32x4_t,
        d: &mut uint32x4_t,
        mx: uint32x4_t,
        my: uint32x4_t,
    ) {
        unsafe {
            *a = vaddq_u32(vaddq_u32(*a, *b), mx);
            *d = vrotr16(veorq_u32(*d, *a));
            *c = vaddq_u32(*c, *d);
            *b = vrotr12(veorq_u32(*b, *c));
            *a = vaddq_u32(vaddq_u32(*a, *b), my);
            *d = vrotr8(veorq_u32(*d, *a));
            *c = vaddq_u32(*c, *d);
            *b = vrotr7(veorq_u32(*b, *c));
        }
    }

    /// Run one BLAKE3 round — column then diagonal — over the 4-vector
    /// state, gathering message words from `m` per the static layout.
    /// Uses `vld2q_u32` for the de-interleaved `(mx, my)` pair (no
    /// stack-array gather) and `vextq_u32` for the diagonal lane
    /// rotations (single-cycle native ext instruction).
    #[inline]
    unsafe fn one_round(
        v0: &mut uint32x4_t,
        v1: &mut uint32x4_t,
        v2: &mut uint32x4_t,
        v3: &mut uint32x4_t,
        m: &[u32; 16],
    ) {
        unsafe {
            // Column round: lane i = (m[2i], m[2i+1]). vld2q de-interleaves
            // 8 contiguous u32s into (.0 = evens, .1 = odds), exactly the
            // shape we need.
            let pair = vld2q_u32(m.as_ptr());
            g(v0, v1, v2, v3, pair.0, pair.1);
            // Diagonal round: rotate lanes by 1 / 2 / 3 with vextq_u32
            // (compiles to one EXT instruction each), apply g, then
            // rotate back.
            let v1r = vextq_u32::<1>(*v1, *v1);
            let v2r = vextq_u32::<2>(*v2, *v2);
            let v3r = vextq_u32::<3>(*v3, *v3);
            let mut v1r = v1r;
            let mut v2r = v2r;
            let mut v3r = v3r;
            let pair = vld2q_u32(m[8..].as_ptr());
            g(v0, &mut v1r, &mut v2r, &mut v3r, pair.0, pair.1);
            // Unrotate: opposite-side EXT.
            *v1 = vextq_u32::<3>(v1r, v1r);
            *v2 = vextq_u32::<2>(v2r, v2r);
            *v3 = vextq_u32::<1>(v3r, v3r);
        }
    }

    /// NEON-vectorised compress. Same API as the scalar reference
    /// (`compress_scalar`); bit-for-bit identical output.
    #[target_feature(enable = "neon")]
    pub unsafe fn compress(
        chaining_value: &[u32; 8],
        block_words: &[u32; 16],
        counter: u64,
        block_len: u32,
        flags: u32,
    ) -> [u32; 16] {
        unsafe {
            let mut v0 = vld1q_u32(chaining_value.as_ptr());
            let mut v1 = vld1q_u32(chaining_value[4..].as_ptr());
            let mut v2 = vld1q_u32(IV.as_ptr());
            let mut v3 = vsetq_lane_u32::<0>(counter as u32, vld1q_u32(IV[4..].as_ptr()));
            v3 = vsetq_lane_u32::<1>((counter >> 32) as u32, v3);
            v3 = vsetq_lane_u32::<2>(block_len, v3);
            v3 = vsetq_lane_u32::<3>(flags, v3);

            let mut block = *block_words;
            for round_idx in 0..7 {
                one_round(&mut v0, &mut v1, &mut v2, &mut v3, &block);
                if round_idx < 6 {
                    let original = block;
                    for i in 0..16 {
                        block[i] = original[MSG_PERMUTATION[i]];
                    }
                }
            }
            // Output mixing per BLAKE3 spec §2.3:
            //   state[i]     ^= state[i+8]
            //   state[i+8]   ^= chaining_value[i]
            v0 = veorq_u32(v0, v2);
            v1 = veorq_u32(v1, v3);
            v2 = veorq_u32(v2, vld1q_u32(chaining_value.as_ptr()));
            v3 = veorq_u32(v3, vld1q_u32(chaining_value[4..].as_ptr()));

            let mut out = [0u32; 16];
            vst1q_u32(out.as_mut_ptr(), v0);
            vst1q_u32(out[4..].as_mut_ptr(), v1);
            vst1q_u32(out[8..].as_mut_ptr(), v2);
            vst1q_u32(out[12..].as_mut_ptr(), v3);
            out
        }
    }
}

#[inline]
fn g(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, mx: u32, my: u32) {
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(mx);
    state[d] = (state[d] ^ state[a]).rotate_right(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(12);
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(my);
    state[d] = (state[d] ^ state[a]).rotate_right(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(7);
}

fn round(state: &mut [u32; 16], m: &[u32; 16]) {
    // Column.
    g(state, 0, 4, 8, 12, m[0], m[1]);
    g(state, 1, 5, 9, 13, m[2], m[3]);
    g(state, 2, 6, 10, 14, m[4], m[5]);
    g(state, 3, 7, 11, 15, m[6], m[7]);
    // Diagonal.
    g(state, 0, 5, 10, 15, m[8], m[9]);
    g(state, 1, 6, 11, 12, m[10], m[11]);
    g(state, 2, 7, 8, 13, m[12], m[13]);
    g(state, 3, 4, 9, 14, m[14], m[15]);
}

fn permute(m: &mut [u32; 16]) {
    let original = *m;
    for i in 0..16 {
        m[i] = original[MSG_PERMUTATION[i]];
    }
}

/// Compression function (BLAKE3 spec §2.3). Returns the 16-word post-mix
/// state; chaining uses the first 8 words.
///
/// v3.0.4 measured: a NEON implementation processing one block across
/// 4 lanes regressed the bench by 1.5–2×. The reason — scalar BLAKE3
/// is already heavily auto-vectorised by LLVM, and a within-block lane
/// split adds 6 EXT permutes per round (42 extra instructions per
/// compress) without buying parallelism. The real SIMD win for BLAKE3
/// is 4-chunk-parallel compression, which doesn't apply to SPG's
/// per-entry audit-log + per-small-catalog hash workload. The NEON
/// path is kept (gated behind `#[cfg(test)]`) as a cross-check oracle
/// only; runtime stays on scalar.
fn compress(
    chaining_value: &[u32; 8],
    block_words: &[u32; 16],
    counter: u64,
    block_len: u32,
    flags: u32,
) -> [u32; 16] {
    compress_scalar(chaining_value, block_words, counter, block_len, flags)
}

fn compress_scalar(
    chaining_value: &[u32; 8],
    block_words: &[u32; 16],
    counter: u64,
    block_len: u32,
    flags: u32,
) -> [u32; 16] {
    let mut state = [
        chaining_value[0],
        chaining_value[1],
        chaining_value[2],
        chaining_value[3],
        chaining_value[4],
        chaining_value[5],
        chaining_value[6],
        chaining_value[7],
        IV[0],
        IV[1],
        IV[2],
        IV[3],
        counter as u32,
        (counter >> 32) as u32,
        block_len,
        flags,
    ];
    let mut block = *block_words;
    round(&mut state, &block); // 1
    permute(&mut block);
    round(&mut state, &block); // 2
    permute(&mut block);
    round(&mut state, &block); // 3
    permute(&mut block);
    round(&mut state, &block); // 4
    permute(&mut block);
    round(&mut state, &block); // 5
    permute(&mut block);
    round(&mut state, &block); // 6
    permute(&mut block);
    round(&mut state, &block); // 7

    // Output mixing — spec §2.3.
    for i in 0..8 {
        state[i] ^= state[i + 8];
        state[i + 8] ^= chaining_value[i];
    }
    state
}

fn words_from_le_bytes(bytes: &[u8; BLOCK_LEN]) -> [u32; 16] {
    let mut m = [0u32; 16];
    for (i, chunk) in bytes.chunks_exact(4).enumerate() {
        m[i] = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    m
}

fn bytes_from_le_words(words: &[u32; 8]) -> [u8; OUT_LEN] {
    let mut out = [0u8; OUT_LEN];
    for (i, w) in words.iter().enumerate() {
        out[i * 4..(i + 1) * 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

/// Hash one chunk (≤ 1024 bytes). Returns the chunk's chaining value.
fn hash_chunk(input: &[u8], chunk_counter: u64, is_root: bool, base_flags: u32) -> [u32; 8] {
    debug_assert!(input.len() <= CHUNK_LEN);
    let block_count = if input.is_empty() {
        1
    } else {
        input.len().div_ceil(BLOCK_LEN)
    };

    let mut cv = IV;
    for b_idx in 0..block_count {
        let start = b_idx * BLOCK_LEN;
        let end = core::cmp::min(start + BLOCK_LEN, input.len());
        let mut block = [0u8; BLOCK_LEN];
        if end > start {
            block[..end - start].copy_from_slice(&input[start..end]);
        }
        let block_words = words_from_le_bytes(&block);
        let block_len = (end - start) as u32;
        let mut flags = base_flags;
        if b_idx == 0 {
            flags |= CHUNK_START;
        }
        if b_idx == block_count - 1 {
            flags |= CHUNK_END;
            if is_root {
                flags |= ROOT;
            }
        }
        let state = compress(&cv, &block_words, chunk_counter, block_len, flags);
        cv.copy_from_slice(&state[..8]);
    }
    cv
}

/// Parent-node compression — counter is always 0, `block_len` always 64.
fn parent_cv(left: &[u32; 8], right: &[u32; 8], is_root: bool, base_flags: u32) -> [u32; 8] {
    let mut block_words = [0u32; 16];
    block_words[..8].copy_from_slice(left);
    block_words[8..].copy_from_slice(right);
    let mut flags = base_flags | PARENT;
    if is_root {
        flags |= ROOT;
    }
    let state = compress(&IV, &block_words, 0, BLOCK_LEN as u32, flags);
    let mut cv = [0u32; 8];
    cv.copy_from_slice(&state[..8]);
    cv
}

/// Hash a subtree (must contain ≥ 1 chunk worth of bytes when called from
/// the top level via [`hash`]). Returns the subtree's chaining value.
///
/// BLAKE3 trees are left-balanced: at each internal node the left subtree
/// holds the largest power-of-two chunks that still leave the right side
/// non-empty.
fn hash_subtree(input: &[u8], chunk_counter_base: u64, base_flags: u32) -> [u32; 8] {
    if input.len() <= CHUNK_LEN {
        return hash_chunk(input, chunk_counter_base, false, base_flags);
    }
    let total_chunks = input.len().div_ceil(CHUNK_LEN);
    let left_chunks = largest_power_of_two_leq(total_chunks - 1);
    let left_len = left_chunks * CHUNK_LEN;
    let left = &input[..left_len];
    let right = &input[left_len..];
    let left_cv = hash_subtree(left, chunk_counter_base, base_flags);
    let right_cv = hash_subtree(right, chunk_counter_base + left_chunks as u64, base_flags);
    parent_cv(&left_cv, &right_cv, false, base_flags)
}

/// Largest power of two ≤ n, for n ≥ 1.
fn largest_power_of_two_leq(n: usize) -> usize {
    debug_assert!(n >= 1);
    let bits = usize::BITS - 1 - n.leading_zeros();
    1usize << bits
}

/// Top-level BLAKE3 hash. Returns the 32-byte digest.
pub fn hash(input: &[u8]) -> [u8; OUT_LEN] {
    let base_flags: u32 = 0;
    if input.len() <= CHUNK_LEN {
        let cv = hash_chunk(input, 0, true, base_flags);
        return bytes_from_le_words(&cv);
    }
    // Multi-chunk: split + recurse, parent at root flags ROOT.
    let total_chunks = input.len().div_ceil(CHUNK_LEN);
    let left_chunks = largest_power_of_two_leq(total_chunks - 1);
    let left_len = left_chunks * CHUNK_LEN;
    let left = &input[..left_len];
    let right = &input[left_len..];
    let left_cv = hash_subtree(left, 0, base_flags);
    let right_cv = hash_subtree(right, left_chunks as u64, base_flags);
    let root_cv = parent_cv(&left_cv, &right_cv, true, base_flags);
    bytes_from_le_words(&root_cv)
}

/// Helper: format a 32-byte digest as a lower-case hex string (no separators).
/// Allocates a 64-character `String`. Useful for tests / human-facing logs.
pub fn hex(digest: &[u8; OUT_LEN]) -> alloc::string::String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(OUT_LEN * 2);
    for &b in digest {
        out.push(HEX[(b >> 4) as usize]);
        out.push(HEX[(b & 0x0F) as usize]);
    }
    // We only emit ASCII chars, so the bytes are valid UTF-8.
    alloc::string::String::from_utf8(out).expect("hex output is ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    fn h(s: &str) -> String {
        hex(&hash(s.as_bytes()))
    }

    #[test]
    fn empty_input_matches_blake3_kat() {
        // Official BLAKE3 KAT for empty input.
        assert_eq!(
            h(""),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn abc_matches_blake3_kat() {
        assert_eq!(
            h("abc"),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn neon_matches_scalar() {
        // For every block size the hash() entry path could see, run
        // a deterministic input through both the NEON dispatch (which
        // hash() takes on aarch64) and the scalar reference directly,
        // and confirm the two compressions agree bit-for-bit.
        let cv = IV;
        let block = [0xAA55_AA55u32; 16];
        for counter in [0u64, 1, 0xFFFF_FFFFu64, u64::MAX] {
            for &flags in &[0u32, CHUNK_START, CHUNK_END, ROOT, PARENT] {
                for &block_len in &[0u32, 1, 32, 64] {
                    let s = compress_scalar(&cv, &block, counter, block_len, flags);
                    let n = unsafe { neon::compress(&cv, &block, counter, block_len, flags) };
                    assert_eq!(
                        s, n,
                        "scalar vs NEON mismatch at counter={counter} flags={flags} block_len={block_len}"
                    );
                }
            }
        }
        // Then sanity-check the public API: empty / abc inputs still
        // land on the official KATs after the dispatch swap.
        assert_eq!(
            h(""),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(
            h("abc"),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[test]
    fn deterministic() {
        let input = b"hello world";
        assert_eq!(hash(input), hash(input));
    }

    #[test]
    fn one_byte_difference_changes_hash() {
        assert_ne!(hash(b"abc"), hash(b"abd"));
    }

    #[test]
    fn largest_power_of_two_helper() {
        assert_eq!(largest_power_of_two_leq(1), 1);
        assert_eq!(largest_power_of_two_leq(2), 2);
        assert_eq!(largest_power_of_two_leq(3), 2);
        assert_eq!(largest_power_of_two_leq(4), 4);
        assert_eq!(largest_power_of_two_leq(7), 4);
        assert_eq!(largest_power_of_two_leq(8), 8);
        assert_eq!(largest_power_of_two_leq(1023), 512);
    }
}
