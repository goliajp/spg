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
pub(super) fn encode_text(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("encode() takes 2 args, got {}", args.len()),
        });
    }
    if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
        return Ok(Value::Null);
    }
    // PG's signature is `encode(bytea, format)`; SPG also accepts a
    // Text value whose bytes are encoded directly.
    let bytes: &[u8] = match &args[0] {
        Value::Bytes(b) => b.as_ref(),
        Value::Text(s) => s.as_bytes(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!("encode() expects bytea / text, got {:?}", other.data_type()),
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
        // PG's encode(bytea,'base64') breaks the body into 76-char lines
        // separated by `\n` (no trailing newline); decode ignores the
        // whitespace on the way back. base64url is an SPG extension and
        // stays single-line (URL-safe payloads shouldn't carry newlines).
        "base64" => wrap_base64_76(&b64_encode(bytes, B64_STD)),
        "base64url" => b64_encode(bytes, B64_URL),
        "base32hex" => b32hex_encode(bytes),
        "hex" => hex_encode(bytes),
        "escape" => escape_encode(bytes),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!("encode(): unknown format `{other}`"),
            });
        }
    };
    Ok(Value::text(out))
}

/// v6.4.3 — `decode(text, format)`. Inverse of `encode`; returns
/// Text containing the raw decoded bytes (caller may CAST to bytea
/// equivalent if SPG adds bytea later).
pub(super) fn decode_text(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("decode() takes 2 args, got {}", args.len()),
        });
    }
    if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
        return Ok(Value::Null);
    }
    let text = match &args[0] {
        Value::Text(s) => s.as_ref(),
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
        "escape" => escape_decode(text)?,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!("decode(): unknown format `{other}`"),
            });
        }
    };
    // PG's `decode` returns bytea; return raw bytes so `decode(...)::text`
    // renders as `\xHEX` and non-UTF-8 output (e.g. `decode('deadbeef','hex')`)
    // round-trips through `encode` instead of erroring.
    Ok(Value::Bytes(alloc::borrow::Cow::Owned(bytes)))
}

/// PG's `escape` bytea format: printable ASCII stays literal, a backslash
/// doubles, and any other byte becomes `\ooo` (3-digit octal).
fn escape_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            0x5c => out.push_str("\\\\"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\{b:03o}")),
        }
    }
    out
}

/// Inverse of `escape_encode` (PG `byteain` escape form): `\\` → one
/// backslash, `\ooo` (3 octal digits) → that byte, any other byte is
/// literal. A lone `\` not forming one of those errors, matching PG's
/// "invalid input syntax for type bytea".
fn escape_decode(text: &str) -> Result<Vec<u8>, EvalError> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let is_octal = |b: u8| (b'0'..=b'7').contains(&b);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
            out.push(b'\\');
            i += 2;
        } else if i + 3 < bytes.len()
            && is_octal(bytes[i + 1])
            && is_octal(bytes[i + 2])
            && is_octal(bytes[i + 3])
        {
            let d = |b: u8| u32::from(b - b'0');
            let v = d(bytes[i + 1]) * 64 + d(bytes[i + 2]) * 8 + d(bytes[i + 3]);
            // \777 = 511 > 255; PG rejects an out-of-range octal escape.
            let byte = u8::try_from(v).map_err(|_| EvalError::TypeMismatch {
                detail: "decode(escape): octal escape out of range".into(),
            })?;
            out.push(byte);
            i += 4;
        } else {
            return Err(EvalError::TypeMismatch {
                detail: "decode(escape): invalid escape sequence".into(),
            });
        }
    }
    Ok(out)
}

/// Break a base64 body into 76-character lines joined by `\n`, with no
/// trailing newline — PG's `encode(bytea,'base64')` wire form.
fn wrap_base64_76(body: &str) -> String {
    if body.len() <= 76 {
        return alloc::string::String::from(body);
    }
    // base64 is pure ASCII, so byte offsets are char offsets.
    let mut out = String::with_capacity(body.len() + body.len() / 76 + 1);
    let mut i = 0;
    while i < body.len() {
        if i > 0 {
            out.push('\n');
        }
        let end = (i + 76).min(body.len());
        out.push_str(&body[i..end]);
        i = end;
    }
    out
}

/// v7.37.17 (17.6 siblings) — pgcrypto armor(bytea): OpenPGP
/// ASCII-armor (RFC 4880 §6) — base64 body in 76-char lines plus a
/// CRC-24 trailer line.
pub(super) fn pgp_armor(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let bytes: &[u8] = match args {
        [Value::Null] => return Ok(Value::Null),
        [Value::Bytes(b)] => b.as_ref(),
        [Value::Text(s)] => s.as_bytes(),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: format!("armor() takes 1 bytea arg, got {} args", args.len()),
            });
        }
    };
    let body = b64_encode(bytes, B64_STD);
    let crc = crc24(bytes);
    let crc_bytes = [(crc >> 16) as u8, (crc >> 8) as u8, crc as u8];
    let mut out = String::from("-----BEGIN PGP MESSAGE-----\n\n");
    for chunk in body.as_bytes().chunks(76) {
        out.push_str(core::str::from_utf8(chunk).unwrap_or(""));
        out.push('\n');
    }
    out.push('=');
    out.push_str(&b64_encode(&crc_bytes, B64_STD));
    out.push('\n');
    out.push_str("-----END PGP MESSAGE-----\n");
    Ok(Value::text(out))
}

/// dearmor(text) — inverse of armor(): strips the BEGIN/END lines
/// and armor headers, base64-decodes the body, and verifies the
/// CRC-24 trailer when present.
pub(super) fn pgp_dearmor(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let text = match args {
        [Value::Null] => return Ok(Value::Null),
        [Value::Text(s)] => s.as_ref(),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: format!("dearmor() takes 1 text arg, got {} args", args.len()),
            });
        }
    };
    let mut body = String::new();
    let mut crc_line: Option<&str> = None;
    let mut in_body = false;
    let mut past_headers = false;
    for line in text.lines() {
        let line = line.trim_end();
        if line.starts_with("-----BEGIN") {
            in_body = true;
            past_headers = false;
            continue;
        }
        if line.starts_with("-----END") {
            break;
        }
        if !in_body {
            continue;
        }
        if !past_headers {
            // Armor headers end at the first blank line; a line
            // without ':' also means the body started directly.
            if line.is_empty() {
                past_headers = true;
                continue;
            }
            if line.contains(": ") {
                continue;
            }
            past_headers = true;
        }
        if let Some(rest) = line.strip_prefix('=') {
            crc_line = Some(rest);
            continue;
        }
        body.push_str(line);
    }
    if !in_body {
        return Err(EvalError::TypeMismatch {
            detail: "dearmor(): no armor boundary found".into(),
        });
    }
    let bytes = b64_decode(&body, B64_STD)?;
    if let Some(crc_text) = crc_line {
        let crc_bytes = b64_decode(crc_text, B64_STD)?;
        if crc_bytes.len() == 3 {
            let stated = (u32::from(crc_bytes[0]) << 16)
                | (u32::from(crc_bytes[1]) << 8)
                | u32::from(crc_bytes[2]);
            if stated != crc24(&bytes) {
                return Err(EvalError::TypeMismatch {
                    detail: "dearmor(): CRC-24 mismatch".into(),
                });
            }
        }
    }
    Ok(Value::Bytes(alloc::borrow::Cow::Owned(bytes)))
}

/// OpenPGP CRC-24 (RFC 4880 §6.1): init 0xB704CE, poly 0x1864CFB.
fn crc24(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0x00B7_04CE;
    for &b in bytes {
        crc ^= u32::from(b) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x0100_0000 != 0 {
                crc ^= 0x0186_4CFB;
            }
        }
    }
    crc & 0x00FF_FFFF
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
