//! SHA-256 — self-built per FIPS 180-4 §6.2.
//!
//! Used by SPG's v4.8 SCRAM-SHA-256 implementation. Like the
//! existing BLAKE3 implementation, this is hand-written rather than
//! pulled from a crate — same "no external crypto deps" frame.
//! Correctness is checked against the NIST short-message test
//! vectors at module-test time.

const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

const H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

pub const OUT_LEN: usize = 32;
pub const BLOCK_LEN: usize = 64;

#[inline]
const fn ch(x: u32, y: u32, z: u32) -> u32 {
    (x & y) ^ (!x & z)
}
#[inline]
const fn maj(x: u32, y: u32, z: u32) -> u32 {
    (x & y) ^ (x & z) ^ (y & z)
}
#[inline]
const fn big_sigma0(x: u32) -> u32 {
    x.rotate_right(2) ^ x.rotate_right(13) ^ x.rotate_right(22)
}
#[inline]
const fn big_sigma1(x: u32) -> u32 {
    x.rotate_right(6) ^ x.rotate_right(11) ^ x.rotate_right(25)
}
#[inline]
const fn small_sigma0(x: u32) -> u32 {
    x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3)
}
#[inline]
const fn small_sigma1(x: u32) -> u32 {
    x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10)
}

/// One-shot SHA-256 hash. `input` is any length.
pub fn hash(input: &[u8]) -> [u8; OUT_LEN] {
    let mut h = H0;
    let len_bits = (input.len() as u64).wrapping_mul(8);
    // Process every full 64-byte block. The final partial block + padding
    // is handled in the tail section.
    let full_blocks = input.len() / BLOCK_LEN;
    for i in 0..full_blocks {
        compress(&mut h, &input[i * BLOCK_LEN..(i + 1) * BLOCK_LEN]);
    }
    // Tail: copy the remaining bytes into a scratch block, append the
    // 0x80 terminator, zero-pad. If less than 9 bytes remain after the
    // terminator, we need a second padding block for the length field.
    let tail_start = full_blocks * BLOCK_LEN;
    let tail = &input[tail_start..];
    let mut block = [0u8; BLOCK_LEN];
    block[..tail.len()].copy_from_slice(tail);
    block[tail.len()] = 0x80;
    if tail.len() + 1 + 8 > BLOCK_LEN {
        // Doesn't fit in one block — flush this padding-only block, then
        // emit a fresh block carrying only the length.
        compress(&mut h, &block);
        block = [0u8; BLOCK_LEN];
    }
    block[BLOCK_LEN - 8..].copy_from_slice(&len_bits.to_be_bytes());
    compress(&mut h, &block);
    // Pack the eight H words big-endian.
    let mut out = [0u8; OUT_LEN];
    for (i, w) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
    }
    out
}

#[allow(clippy::needless_range_loop, clippy::many_single_char_names)] // FIPS 180-4 names a-h
fn compress(h: &mut [u32; 8], block: &[u8]) {
    debug_assert_eq!(block.len(), BLOCK_LEN);
    let mut w = [0u32; 64];
    for t in 0..16 {
        w[t] = u32::from_be_bytes([
            block[t * 4],
            block[t * 4 + 1],
            block[t * 4 + 2],
            block[t * 4 + 3],
        ]);
    }
    for t in 16..64 {
        w[t] = small_sigma1(w[t - 2])
            .wrapping_add(w[t - 7])
            .wrapping_add(small_sigma0(w[t - 15]))
            .wrapping_add(w[t - 16]);
    }
    let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
        (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
    for t in 0..64 {
        let t1 = hh
            .wrapping_add(big_sigma1(e))
            .wrapping_add(ch(e, f, g))
            .wrapping_add(K[t])
            .wrapping_add(w[t]);
        let t2 = big_sigma0(a).wrapping_add(maj(a, b, c));
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
    h[5] = h[5].wrapping_add(f);
    h[6] = h[6].wrapping_add(g);
    h[7] = h[7].wrapping_add(hh);
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn hex(b: &[u8]) -> alloc::string::String {
        use alloc::string::String;
        use core::fmt::Write;
        let mut s = String::with_capacity(b.len() * 2);
        for x in b {
            let _ = write!(s, "{x:02x}");
        }
        s
    }

    // NIST CAVS / FIPS 180-4 sample vectors.
    #[test]
    fn empty_input() {
        assert_eq!(
            hex(&hash(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn abc() {
        assert_eq!(
            hex(&hash(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn fips_long_string() {
        let input = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(
            hex(&hash(input)),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn block_boundary_64_bytes() {
        // Exactly one block — exercises the "padding spills to a second
        // block" path.
        let input = [b'a'; 64];
        assert_eq!(
            hex(&hash(&input)),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
    }

    #[test]
    fn one_million_a() {
        let input = vec![b'a'; 1_000_000];
        assert_eq!(
            hex(&hash(&input)),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }
}
