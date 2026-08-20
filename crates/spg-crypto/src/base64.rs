//! Base64 (RFC 4648 standard alphabet) — encode + decode.
//!
//! Used by SPG v4.8 SCRAM-SHA-256, which exchanges client/server
//! proofs in base64. We don't pull `base64` from crates.io to keep
//! the no-external-crypto-deps story intact — the implementation
//! is ~30 LOC and easy to test.

use alloc::string::String;
use alloc::vec::Vec;

const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let (chunks, rem) = input.as_chunks::<3>();
    for chunk in chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHA[(n & 0x3f) as usize] as char);
    }
    if rem.len() == 1 {
        let n = u32::from(rem[0]) << 16;
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem.len() == 2 {
        let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    InvalidLength,
    InvalidChar(u8),
}

pub fn decode(input: &str) -> Result<Vec<u8>, DecodeError> {
    let bytes = input.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(DecodeError::InvalidLength);
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let (quads, _) = bytes.as_chunks::<4>();
    for chunk in quads {
        let mut quad = [0u32; 4];
        let mut padding = 0;
        for (i, &b) in chunk.iter().enumerate() {
            if b == b'=' {
                padding += 1;
                continue;
            }
            if padding > 0 {
                return Err(DecodeError::InvalidChar(b));
            }
            quad[i] = u32::from(decode_char(b)?);
        }
        let n = (quad[0] << 18) | (quad[1] << 12) | (quad[2] << 6) | quad[3];
        out.push((n >> 16) as u8);
        if padding < 2 {
            out.push((n >> 8) as u8);
        }
        if padding < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

fn decode_char(b: u8) -> Result<u8, DecodeError> {
    Ok(match b {
        b'A'..=b'Z' => b - b'A',
        b'a'..=b'z' => b - b'a' + 26,
        b'0'..=b'9' => b - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        other => return Err(DecodeError::InvalidChar(other)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_test_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn decode_round_trip() {
        for original in [
            b"".as_slice(),
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
        ] {
            let encoded = encode(original);
            let decoded = decode(&encoded).unwrap();
            assert_eq!(decoded, original);
        }
    }

    #[test]
    fn decode_rejects_bad_length() {
        assert_eq!(decode("Zg=").unwrap_err(), DecodeError::InvalidLength);
    }

    #[test]
    fn decode_rejects_bad_chars() {
        let err = decode("Zm9v!Yg=").unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidLength | DecodeError::InvalidChar(_)
        ));
    }
}
