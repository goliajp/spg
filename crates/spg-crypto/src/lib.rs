//! BLAKE3 cryptographic hash — self-built single-thread reference
//! implementation. Follows the spec at
//! <https://github.com/BLAKE3-team/BLAKE3-specs/blob/master/blake3.pdf>.
//!
//! Scope: unkeyed `hash(input) -> [u8; 32]` only. KDF / keyed-hash modes and
//! parallel/SIMD optimisations are out of scope for v0.7 — single-thread
//! correctness is what the audit log needs.
#![no_std]
// BLAKE3 intentionally splits a 64-bit counter into two 32-bit words and
// writes a u32 block length that is always ≤ 64. Clippy's truncation warning
// is correct in general but here the truncation is the protocol.
#![allow(clippy::cast_possible_truncation)]

extern crate alloc;

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
fn compress(
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
