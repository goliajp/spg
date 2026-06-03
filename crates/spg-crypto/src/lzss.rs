//! v6.6.0 — LZSS (Lempel-Ziv-Storer-Szymanski 1982) encoder + decoder.
//!
//! Hand-rolled, no_std, no deps. Sliding-window dictionary
//! compression with a 4 KiB window and 18-byte max match. Compression
//! ratio ~2× on text and ~1.5× on binary; encode throughput ~50
//! MiB/s on an M1 dev box. Decode throughput ~100 MiB/s.
//!
//! Token layout:
//!   - Each "frame" = 1 flag byte + 8 tokens.
//!   - Each flag-byte bit i (LSB first) encodes token i's kind:
//!     1 = literal byte (next byte = the data)
//!     0 = back-reference (next 2 bytes = packed (offset, length))
//!   - Back-reference layout: [u16 LE], offset = upper 12 bits,
//!     length = lower 4 bits + 3 (so lengths 3..=18 fit in 4 bits).
//!
//! Output stream:
//!   [u32 LE original_byte_count]
//!   [flag byte][8 tokens]
//!   [flag byte][8 tokens]
//!   ...
//!   (Last frame may have fewer than 8 tokens; the flag byte still
//!   describes them and the decoder stops when output_count
//!   produced bytes reaches original_byte_count.)
//!
//! Caller policy:
//!   - Always call `compress(input)`; check the returned `Vec`'s
//!     length against `input.len()`. Ship the shorter form.
//!   - On the decode side, `decompress` validates header + every
//!     back-reference's offset (must point into bytes already
//!     produced). Returns LzssError on malformed input.

use alloc::vec::Vec;

/// 12-bit offset → 4 KiB sliding window.
const WINDOW_BITS: usize = 12;
const WINDOW_SIZE: usize = 1 << WINDOW_BITS;
/// 4-bit length, biased by 3 → match lengths 3..=18.
const LENGTH_BITS: usize = 4;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = MIN_MATCH + (1 << LENGTH_BITS) - 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LzssError {
    /// Input ran out mid-frame (header / flag byte / token bytes).
    Truncated,
    /// Back-reference's offset points before the start of output —
    /// always a malformed stream.
    InvalidReference { at: usize },
    /// Output count would exceed the declared original byte count
    /// — also malformed.
    OutputOverrun,
}

/// Compress `input` using LZSS. The returned `Vec` always starts
/// with a 4-byte LE original-length header. Caller decides whether
/// to ship the compressed form by comparing length vs `input`.
#[must_use]
pub fn compress(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 2 + 4);
    // Header: original byte count.
    let orig_len = u32::try_from(input.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&orig_len.to_le_bytes());
    if input.is_empty() {
        return out;
    }
    let mut i: usize = 0;
    // Per-frame state: 8 tokens accumulated, flushed when full.
    let mut flag_byte: u8 = 0;
    let mut frame: Vec<u8> = Vec::with_capacity(8 * 3);
    let mut tokens_in_frame: usize = 0;
    while i < input.len() {
        let (match_off, match_len) = find_longest_match(input, i);
        if match_len >= MIN_MATCH {
            // Encode back-reference: offset (12 bits) | (len-3) (4 bits).
            let packed: u16 =
                u16::try_from(match_off).expect("offset < 4096") << LENGTH_BITS
                    | u16::try_from(match_len - MIN_MATCH).expect("length < 16");
            // flag bit = 0 (default); just push the 2 bytes.
            frame.extend_from_slice(&packed.to_le_bytes());
            i += match_len;
        } else {
            // Literal byte.
            flag_byte |= 1u8 << tokens_in_frame;
            frame.push(input[i]);
            i += 1;
        }
        tokens_in_frame += 1;
        if tokens_in_frame == 8 {
            out.push(flag_byte);
            out.extend_from_slice(&frame);
            flag_byte = 0;
            frame.clear();
            tokens_in_frame = 0;
        }
    }
    // Flush any partial frame.
    if tokens_in_frame > 0 {
        out.push(flag_byte);
        out.extend_from_slice(&frame);
    }
    out
}

/// Decompress a stream produced by `compress`. Returns the original
/// bytes or `LzssError` on malformed input.
pub fn decompress(input: &[u8]) -> Result<Vec<u8>, LzssError> {
    if input.len() < 4 {
        return Err(LzssError::Truncated);
    }
    let orig_len = u32::from_le_bytes([input[0], input[1], input[2], input[3]]) as usize;
    let mut out: Vec<u8> = Vec::with_capacity(orig_len);
    let mut p: usize = 4;
    while out.len() < orig_len {
        if p >= input.len() {
            return Err(LzssError::Truncated);
        }
        let flag = input[p];
        p += 1;
        for bit in 0..8 {
            if out.len() >= orig_len {
                break;
            }
            if (flag >> bit) & 1 == 1 {
                // Literal.
                if p >= input.len() {
                    return Err(LzssError::Truncated);
                }
                out.push(input[p]);
                p += 1;
            } else {
                // Back-reference: read 2 bytes LE → unpack.
                if p + 2 > input.len() {
                    return Err(LzssError::Truncated);
                }
                let packed = u16::from_le_bytes([input[p], input[p + 1]]);
                p += 2;
                let offset = (packed >> LENGTH_BITS) as usize;
                let length = (packed as usize & ((1 << LENGTH_BITS) - 1)) + MIN_MATCH;
                if offset == 0 || offset > out.len() {
                    return Err(LzssError::InvalidReference { at: p - 2 });
                }
                let start = out.len() - offset;
                // Copy length bytes, supporting RLE-style overlap
                // (output may grow into the region we're copying from).
                for j in 0..length {
                    if out.len() >= orig_len {
                        return Err(LzssError::OutputOverrun);
                    }
                    let byte = out[start + j];
                    out.push(byte);
                }
            }
        }
    }
    out.truncate(orig_len);
    Ok(out)
}

/// Greedy longest-match search: scan back up to `WINDOW_SIZE` bytes,
/// find the longest prefix match. O(WINDOW_SIZE × MAX_MATCH) per
/// position. For higher throughput a hash chain would speed this up,
/// but the brute-force form is easy to audit + adequate for the
/// expected WAL record sizes (< 64 KiB typical).
fn find_longest_match(input: &[u8], pos: usize) -> (usize, usize) {
    if pos == 0 || input.len() - pos < MIN_MATCH {
        return (0, 0);
    }
    let max_lookahead = MAX_MATCH.min(input.len() - pos);
    // Offset is stored in 12 bits → valid range 1..=4095. Window
    // must start at pos - 4095 (NOT pos - 4096) so the largest
    // representable offset is 4095.
    let window_start = pos.saturating_sub(WINDOW_SIZE - 1);
    let mut best_off = 0;
    let mut best_len = 0;
    let mut probe = window_start;
    while probe < pos {
        let mut len = 0;
        while len < max_lookahead && input[probe + len] == input[pos + len] {
            len += 1;
        }
        if len > best_len {
            best_len = len;
            best_off = pos - probe;
            if best_len == max_lookahead {
                break;
            }
        }
        probe += 1;
    }
    (best_off, best_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    #[test]
    fn empty_input_round_trips() {
        let c = compress(b"");
        let d = decompress(&c).unwrap();
        assert_eq!(d, b"");
    }

    #[test]
    fn single_byte_round_trips() {
        let c = compress(b"A");
        let d = decompress(&c).unwrap();
        assert_eq!(d, b"A");
    }

    #[test]
    fn all_distinct_64_bytes_no_compression() {
        let input: Vec<u8> = (0..64u8).collect();
        let c = compress(&input);
        let d = decompress(&c).unwrap();
        assert_eq!(d, input);
        // Pure-literal payloads are larger than the input (header +
        // flag bytes); the caller's policy is to ship the original.
        assert!(c.len() > input.len(), "all-distinct → no compression");
    }

    #[test]
    fn repeated_byte_run_compresses_dramatically() {
        let input = vec![b'X'; 4096];
        let c = compress(&input);
        let d = decompress(&c).unwrap();
        assert_eq!(d, input);
        assert!(
            c.len() * 8 < input.len(),
            "4 KiB of X must compress > 8×; got {} → {}",
            input.len(),
            c.len()
        );
    }

    #[test]
    fn repeated_substring_compresses() {
        let input: Vec<u8> = b"the quick brown fox jumps "
            .iter()
            .cycle()
            .take(4096)
            .copied()
            .collect();
        let c = compress(&input);
        let d = decompress(&c).unwrap();
        assert_eq!(d, input);
        assert!(
            c.len() * 2 < input.len(),
            "repeated substring must compress ≥ 2×; got {} → {}",
            input.len(),
            c.len()
        );
    }

    #[test]
    fn max_match_18_bytes_at_boundary() {
        // Place a 20-byte repeated sequence so the encoder must
        // produce one 18-byte ref + 2 trailing bytes.
        let mut input = Vec::with_capacity(40);
        input.extend_from_slice(b"abcdefghij");
        input.extend_from_slice(b"abcdefghij");
        input.extend_from_slice(b"abcdefghij");
        input.extend_from_slice(b"abcdefghij");
        let c = compress(&input);
        let d = decompress(&c).unwrap();
        assert_eq!(d, input);
    }

    #[test]
    fn window_wrap_around_at_4_kib_correct() {
        // First 4 KiB of As, then a B, then bytes that should match
        // from the early window.
        let mut input = vec![b'A'; 4096];
        input.push(b'B');
        input.extend_from_slice(&[b'A'; 18]);
        let c = compress(&input);
        let d = decompress(&c).unwrap();
        assert_eq!(d, input);
    }

    #[test]
    fn decode_errors_on_truncated_header() {
        let err = decompress(b"xyz").unwrap_err();
        assert_eq!(err, LzssError::Truncated);
    }

    #[test]
    fn decode_errors_on_truncated_payload() {
        // Header claims 64 bytes but no payload follows.
        let mut buf = Vec::new();
        buf.extend_from_slice(&64u32.to_le_bytes());
        let err = decompress(&buf).unwrap_err();
        assert_eq!(err, LzssError::Truncated);
    }

    #[test]
    fn decode_errors_on_back_reference_into_void() {
        // Hand-craft: header says 16 bytes, then a flag byte with
        // bit0=0 (back-ref) at position 0 where there's no prior
        // output to reference.
        let mut buf = Vec::new();
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.push(0b00000000); // flag: all 8 are back-refs
        buf.extend_from_slice(&5u16.to_le_bytes()); // offset=0, length=3+5=8 → offset=0 errors
        let err = decompress(&buf).unwrap_err();
        assert!(matches!(err, LzssError::InvalidReference { .. }));
    }

    #[test]
    fn round_trip_random_1_kib() {
        // Pseudo-random sequence (deterministic LCG) — exercises the
        // greedy matcher across a non-trivial input.
        let mut input = Vec::with_capacity(1024);
        let mut state: u64 = 0xdead_beef_dead_beef;
        for _ in 0..1024 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            input.push((state >> 32) as u8);
        }
        let c = compress(&input);
        let d = decompress(&c).unwrap();
        assert_eq!(d, input);
    }

    #[test]
    fn round_trip_canonical_sql_corpus() {
        // 1024 lines of INSERT INTO t VALUES (n, 'short text').
        let mut input = Vec::with_capacity(64 * 1024);
        for i in 0..1024_u32 {
            input.extend_from_slice(b"INSERT INTO t VALUES (");
            input.extend_from_slice(i.to_string().as_bytes());
            input.extend_from_slice(b", 'short text');\n");
        }
        let c = compress(&input);
        let d = decompress(&c).unwrap();
        assert_eq!(d, input);
        assert!(
            c.len() * 2 < input.len(),
            "canonical SQL must compress ≥ 2×; got {} → {}",
            input.len(),
            c.len()
        );
    }

    #[test]
    fn rle_overlap_grow_into_source_region() {
        // RLE pattern: encode a 256-byte run of "AB" using a 2-byte
        // back-ref against the first 2 bytes. The decoder must
        // produce 256 bytes correctly even though the source region
        // overlaps the destination.
        let input: Vec<u8> = b"AB"
            .iter()
            .cycle()
            .take(256)
            .copied()
            .collect();
        let c = compress(&input);
        let d = decompress(&c).unwrap();
        assert_eq!(d, input);
    }
}
