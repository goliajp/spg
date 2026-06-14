//! v6.4.3 — `encode(text, format)` / `decode(text, format)` and the
//! byte-level base64 / base64url / base32hex / hex codecs behind them.
//! SPG's value space treats Text as the raw-UTF-8 byte container.
//! Split out of `eval.rs` (cut 25).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use spg_storage::Value;

use super::EvalError;

/// v6.4.3 — `encode(bytes_as_text, format)`. PG works on bytea
/// arguments; SPG's value space treats Text as the byte container
/// (raw UTF-8 bytes). Supported formats: base64 (PG default),
/// base64url (RFC 4648 §5), base32hex (RFC 4648 §7 extended-hex),
/// hex.
pub(super) fn encode_text(args: &[Value]) -> Result<Value, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("encode() takes 2 args, got {}", args.len()),
        });
    }
    if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
        return Ok(Value::Null);
    }
    let bytes: &[u8] = match &args[0] {
        Value::Text(s) => s.as_bytes(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!("encode() expects text bytes, got {:?}", other.data_type()),
            });
        }
    };
    let fmt = match &args[1] {
        Value::Text(s) => s.to_ascii_lowercase(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!("encode() format must be text, got {:?}", other.data_type()),
            });
        }
    };
    let out = match fmt.as_str() {
        "base64" => b64_encode(bytes, B64_STD),
        "base64url" => b64_encode(bytes, B64_URL),
        "base32hex" => b32hex_encode(bytes),
        "hex" => hex_encode(bytes),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!("encode(): unknown format `{other}`"),
            });
        }
    };
    Ok(Value::Text(out))
}

/// v6.4.3 — `decode(text, format)`. Inverse of `encode`; returns
/// Text containing the raw decoded bytes (caller may CAST to bytea
/// equivalent if SPG adds bytea later).
pub(super) fn decode_text(args: &[Value]) -> Result<Value, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("decode() takes 2 args, got {}", args.len()),
        });
    }
    if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
        return Ok(Value::Null);
    }
    let text = match &args[0] {
        Value::Text(s) => s.as_str(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!("decode() expects text, got {:?}", other.data_type()),
            });
        }
    };
    let fmt = match &args[1] {
        Value::Text(s) => s.to_ascii_lowercase(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!("decode() format must be text, got {:?}", other.data_type()),
            });
        }
    };
    let bytes = match fmt.as_str() {
        "base64" => b64_decode(text, B64_STD)?,
        "base64url" => b64_decode(text, B64_URL)?,
        "base32hex" => b32hex_decode(text)?,
        "hex" => hex_decode(text)?,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!("decode(): unknown format `{other}`"),
            });
        }
    };
    let s = String::from_utf8(bytes).map_err(|_| EvalError::TypeMismatch {
        detail: "decode(): result bytes are not valid UTF-8 (SPG stores raw bytes as Text)".into(),
    })?;
    Ok(Value::Text(s))
}

// ── byte-level encoders ───────────────────────────────────────────

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const B32HEX_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHIJKLMNOPQRSTUV";

fn b64_encode(bytes: &[u8], alpha: &[u8; 64]) -> String {
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        out.push(alpha[((n >> 18) & 0x3f) as usize] as char);
        out.push(alpha[((n >> 12) & 0x3f) as usize] as char);
        out.push(alpha[((n >> 6) & 0x3f) as usize] as char);
        out.push(alpha[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(alpha[((n >> 18) & 0x3f) as usize] as char);
        out.push(alpha[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(alpha[((n >> 18) & 0x3f) as usize] as char);
        out.push(alpha[((n >> 12) & 0x3f) as usize] as char);
        out.push(alpha[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

fn b64_decode(text: &str, alpha: &[u8; 64]) -> Result<Vec<u8>, EvalError> {
    let mut lookup = [255u8; 256];
    for (i, &c) in alpha.iter().enumerate() {
        lookup[c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in text.bytes() {
        if c == b'=' {
            break;
        }
        if c == b'\n' || c == b'\r' || c == b' ' {
            continue;
        }
        let v = lookup[c as usize];
        if v == 255 {
            return Err(EvalError::TypeMismatch {
                detail: format!("decode(base64): invalid char {:?}", c as char),
            });
        }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

fn b32hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8 + 4) / 5);
    let mut buf: u64 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        buf = (buf << 8) | b as u64;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32HEX_ALPHABET[((buf >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32HEX_ALPHABET[((buf << (5 - bits)) & 0x1f) as usize] as char);
    }
    // Pad to multiple of 8.
    while out.len() % 8 != 0 {
        out.push('=');
    }
    out
}

fn b32hex_decode(text: &str) -> Result<Vec<u8>, EvalError> {
    let mut lookup = [255u8; 256];
    for (i, &c) in B32HEX_ALPHABET.iter().enumerate() {
        lookup[c as usize] = i as u8;
        // base32hex is case-insensitive — also map lowercase.
        let lower = (c as char).to_ascii_lowercase() as u8;
        lookup[lower as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let mut buf: u64 = 0;
    let mut bits: u32 = 0;
    for c in text.bytes() {
        if c == b'=' {
            break;
        }
        if c == b'\n' || c == b'\r' || c == b' ' {
            continue;
        }
        let v = lookup[c as usize];
        if v == 255 {
            return Err(EvalError::TypeMismatch {
                detail: format!("decode(base32hex): invalid char {:?}", c as char),
            });
        }
        buf = (buf << 5) | v as u64;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn hex_decode(text: &str) -> Result<Vec<u8>, EvalError> {
    let trimmed = text.trim();
    if trimmed.len() % 2 != 0 {
        return Err(EvalError::TypeMismatch {
            detail: "decode(hex): input length must be even".into(),
        });
    }
    let mut out = Vec::with_capacity(trimmed.len() / 2);
    let mut hi: u8 = 0;
    for (i, c) in trimmed.bytes().enumerate() {
        let v = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => {
                return Err(EvalError::TypeMismatch {
                    detail: format!("decode(hex): invalid char {:?}", c as char),
                });
            }
        };
        if i % 2 == 0 {
            hi = v;
        } else {
            out.push((hi << 4) | v);
        }
    }
    Ok(out)
}
