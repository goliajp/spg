//! Self-built wire-frame codec for SPG.
//!
//! Frame layout (little-endian):
//!
//! ```text
//! +-----------------+--------+----------------------------+
//! | payload_len:u32 | op:u8  | payload[payload_len bytes] |
//! +-----------------+--------+----------------------------+
//! ```
//!
//! Header is always [`FRAME_HEADER_LEN`] bytes. Maximum payload is
//! [`MAX_PAYLOAD`] bytes; oversized frames are rejected before allocation.
//!
//! Endianness is little-endian everywhere (modern CPUs are LE; the protocol is
//! self-defined so we drop the PG/MySQL big-endian baggage).
#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

/// Fixed-header byte count: `u32 length` + `u8 opcode`.
pub const FRAME_HEADER_LEN: usize = 5;

/// Hard ceiling on payload size. Keeps `decode` bounded even when a peer
/// declares an absurd length. 16 MiB is generous for v0.x — revisit alongside
/// streaming result-set support.
pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;

/// Wire opcodes (1 byte each). Numeric values are stable on the wire — never
/// renumber an existing variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Op {
    Ping = 0x00,
    Pong = 0x01,
    /// v1.14 client → server. Payload is the candidate password
    /// (UTF-8 bytes). When the server is configured with a password,
    /// the connection stays unauthenticated and refuses every other
    /// opcode until `Auth` succeeds. A matching password gets a `Pong`
    /// reply; a wrong one gets `ErrorResponse`.
    Auth = 0x02,
    /// v4.1 client → server. Carries `(username, password)` for
    /// per-user authentication. Layout:
    /// `[u16 user_len][user UTF-8][password UTF-8]`. When the server
    /// has a user table configured, this is the only auth that
    /// works; legacy `Op::Auth` (password-only) still works in
    /// single-password mode for backwards compatibility.
    AuthUser = 0x03,
    // Query / result opcodes (v0.5).
    Query = 0x10,           // client → server: SQL text payload
    RowDescription = 0x11,  // server → client: column metadata
    DataRow = 0x12,         // server → client: one result row
    CommandComplete = 0x13, // server → client: affected count
    ErrorResponse = 0x14,   // server → client: human-readable error text
    // v0.12 admin / observability.
    Stats = 0x15,         // client → server: request a human-readable status report
    StatsResponse = 0x16, // server → client: status report text (UTF-8)
    /// v3.3.0 server → client: many result rows packed into one frame.
    /// Layout: `[u16 row_count][u16 cell_count][per-cell WireValue]*`.
    /// `cell_count` is hoisted out (same for every row in the batch,
    /// fixed by schema), saving 2 bytes / row vs sending a stream of
    /// `DataRow` frames. The server only emits this for SELECTs with
    /// more than one returned row — single-row paths still use `DataRow`
    /// so a v3.2 / v3.1 client stays decodable.
    DataRowBatch = 0x17,
    Error = 0xFF,
}

impl Op {
    pub const fn from_byte(b: u8) -> Result<Self, FrameError> {
        match b {
            0x00 => Ok(Self::Ping),
            0x01 => Ok(Self::Pong),
            0x02 => Ok(Self::Auth),
            0x03 => Ok(Self::AuthUser),
            0x10 => Ok(Self::Query),
            0x11 => Ok(Self::RowDescription),
            0x12 => Ok(Self::DataRow),
            0x13 => Ok(Self::CommandComplete),
            0x14 => Ok(Self::ErrorResponse),
            0x15 => Ok(Self::Stats),
            0x16 => Ok(Self::StatsResponse),
            0x17 => Ok(Self::DataRowBatch),
            0xFF => Ok(Self::Error),
            other => Err(FrameError::UnknownOp(other)),
        }
    }
}

/// One decoded frame held in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub op: Op,
    pub payload: Vec<u8>,
}

impl Frame {
    pub const fn new(op: Op, payload: Vec<u8>) -> Self {
        Self { op, payload }
    }

    pub const fn ping() -> Self {
        Self {
            op: Op::Ping,
            payload: Vec::new(),
        }
    }

    pub const fn pong() -> Self {
        Self {
            op: Op::Pong,
            payload: Vec::new(),
        }
    }

    pub fn error(message: &str) -> Self {
        Self {
            op: Op::Error,
            payload: message.as_bytes().to_vec(),
        }
    }
}

/// Decode-side errors. Encode never produces these unless the caller exceeded
/// [`MAX_PAYLOAD`]; see [`encode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// Fewer than [`FRAME_HEADER_LEN`] bytes in the buffer.
    ShortHeader,
    /// Header parsed, but the buffer ran out before the full payload arrived.
    /// The caller should accumulate more bytes and retry.
    ShortPayload,
    /// Declared payload length exceeds [`MAX_PAYLOAD`].
    PayloadTooLarge(u32),
    /// Opcode byte is not a known [`Op`] variant.
    UnknownOp(u8),
    /// Payload decoding ran past the end of the buffer.
    TruncatedPayload,
    /// Payload bytes that were supposed to be UTF-8 weren't.
    InvalidUtf8,
    /// Value-codec type tag byte is not a known [`WireType`].
    UnknownWireType(u8),
    /// A length field (column count, payload sub-length, …) overflowed its
    /// on-wire width — typically `u16` for counts or `u32` for text.
    FieldTooLarge,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShortHeader => {
                write!(f, "frame header truncated (need {FRAME_HEADER_LEN} bytes)")
            }
            Self::ShortPayload => write!(f, "frame payload truncated"),
            Self::PayloadTooLarge(n) => write!(f, "frame payload too large: {n} > {MAX_PAYLOAD}"),
            Self::UnknownOp(b) => write!(f, "unknown opcode: 0x{b:02x}"),
            Self::TruncatedPayload => f.write_str("payload truncated mid-decode"),
            Self::InvalidUtf8 => f.write_str("invalid UTF-8 in payload"),
            Self::UnknownWireType(b) => write!(f, "unknown wire type tag: 0x{b:02x}"),
            Self::FieldTooLarge => f.write_str("field length overflowed its wire width"),
        }
    }
}

/// Encode one frame, appending to `out`.
///
/// Returns `Err(PayloadTooLarge)` if the payload exceeds [`MAX_PAYLOAD`] or
/// does not fit in a `u32` length prefix. On error, `out` is left unmodified.
pub fn encode(frame: &Frame, out: &mut Vec<u8>) -> Result<(), FrameError> {
    let len =
        u32::try_from(frame.payload.len()).map_err(|_| FrameError::PayloadTooLarge(u32::MAX))?;
    if len > MAX_PAYLOAD {
        return Err(FrameError::PayloadTooLarge(len));
    }
    out.reserve(FRAME_HEADER_LEN + frame.payload.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.push(frame.op as u8);
    out.extend_from_slice(&frame.payload);
    Ok(())
}

/// Attempt to decode one frame from the front of `buf`.
///
/// On success returns `(frame, consumed)`. The caller drops `consumed` bytes
/// from the read buffer. `ShortHeader` / `ShortPayload` are *not* fatal — the
/// caller should read more bytes and retry.
pub fn decode(buf: &[u8]) -> Result<(Frame, usize), FrameError> {
    if buf.len() < FRAME_HEADER_LEN {
        return Err(FrameError::ShortHeader);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if len > MAX_PAYLOAD {
        return Err(FrameError::PayloadTooLarge(len));
    }
    let op = Op::from_byte(buf[4])?;

    let payload_end = FRAME_HEADER_LEN + len as usize;
    if buf.len() < payload_end {
        return Err(FrameError::ShortPayload);
    }
    let mut payload = Vec::with_capacity(len as usize);
    payload.extend_from_slice(&buf[FRAME_HEADER_LEN..payload_end]);
    Ok((Frame { op, payload }, payload_end))
}

// =========================================================================
// Wire value codec + opcode-specific frame builders / parsers (v0.5).
// =========================================================================

/// On-wire type tags. Stable bytes — never renumber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WireType {
    Null = 0x00,
    Int = 0x01,    // i32 LE
    BigInt = 0x02, // i64 LE
    Float = 0x03,  // f64 LE
    Text = 0x04,   // u32 LE length + bytes (UTF-8)
    Bool = 0x05,   // single byte, 0 or 1
    Vector = 0x06, // u32 LE dim + dim * f32 LE  (pgvector-style)
}

impl WireType {
    pub const fn from_byte(b: u8) -> Result<Self, FrameError> {
        match b {
            0x00 => Ok(Self::Null),
            0x01 => Ok(Self::Int),
            0x02 => Ok(Self::BigInt),
            0x03 => Ok(Self::Float),
            0x04 => Ok(Self::Text),
            0x05 => Ok(Self::Bool),
            0x06 => Ok(Self::Vector),
            other => Err(FrameError::UnknownWireType(other)),
        }
    }
}

/// One value as it travels on the wire. Mirrors `spg-storage::Value` but
/// `spg-wire` is dep-free of storage — callers convert at the boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum WireValue {
    Null,
    Int(i32),
    BigInt(i64),
    Float(f64),
    Text(alloc::string::String),
    Bool(bool),
    Vector(Vec<f32>),
}

impl WireValue {
    pub const fn wire_type(&self) -> WireType {
        match self {
            Self::Null => WireType::Null,
            Self::Int(_) => WireType::Int,
            Self::BigInt(_) => WireType::BigInt,
            Self::Float(_) => WireType::Float,
            Self::Text(_) => WireType::Text,
            Self::Bool(_) => WireType::Bool,
            Self::Vector(_) => WireType::Vector,
        }
    }

    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), FrameError> {
        out.push(self.wire_type() as u8);
        match self {
            Self::Null => {}
            Self::Int(n) => out.extend_from_slice(&n.to_le_bytes()),
            Self::BigInt(n) => out.extend_from_slice(&n.to_le_bytes()),
            Self::Float(x) => out.extend_from_slice(&x.to_le_bytes()),
            Self::Text(s) => {
                let len = u32::try_from(s.len()).map_err(|_| FrameError::FieldTooLarge)?;
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            Self::Bool(b) => out.push(u8::from(*b)),
            Self::Vector(v) => {
                let dim = u32::try_from(v.len()).map_err(|_| FrameError::FieldTooLarge)?;
                out.extend_from_slice(&dim.to_le_bytes());
                for x in v {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
        }
        Ok(())
    }

    /// Decode one `WireValue` starting at `buf[off]`; returns the value and
    /// the byte offset *after* it. `ShortPayload`/`TruncatedPayload` mean the
    /// caller should accumulate more bytes (during streaming) — but inside a
    /// fully-buffered frame they're a hard error.
    pub fn decode(buf: &[u8], off: usize) -> Result<(Self, usize), FrameError> {
        let (tag, off) = read_u8(buf, off)?;
        match WireType::from_byte(tag)? {
            WireType::Null => Ok((Self::Null, off)),
            WireType::Int => {
                let (n, off) = read_i32(buf, off)?;
                Ok((Self::Int(n), off))
            }
            WireType::BigInt => {
                let (n, off) = read_i64(buf, off)?;
                Ok((Self::BigInt(n), off))
            }
            WireType::Float => {
                let (x, off) = read_f64(buf, off)?;
                Ok((Self::Float(x), off))
            }
            WireType::Text => {
                let (len, off) = read_u32(buf, off)?;
                let end = off
                    .checked_add(len as usize)
                    .ok_or(FrameError::FieldTooLarge)?;
                if buf.len() < end {
                    return Err(FrameError::TruncatedPayload);
                }
                let s =
                    core::str::from_utf8(&buf[off..end]).map_err(|_| FrameError::InvalidUtf8)?;
                Ok((Self::Text(s.into()), end))
            }
            WireType::Bool => {
                let (b, off) = read_u8(buf, off)?;
                Ok((Self::Bool(b != 0), off))
            }
            WireType::Vector => {
                let (dim, mut off) = read_u32(buf, off)?;
                let dim = dim as usize;
                let mut v = Vec::with_capacity(dim);
                for _ in 0..dim {
                    let end = off + 4;
                    if buf.len() < end {
                        return Err(FrameError::TruncatedPayload);
                    }
                    let arr: [u8; 4] = buf[off..end].try_into().expect("checked");
                    v.push(f32::from_le_bytes(arr));
                    off = end;
                }
                Ok((Self::Vector(v), off))
            }
        }
    }
}

/// Column metadata sent in a `RowDescription` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDesc {
    pub name: alloc::string::String,
    pub ty: WireType,
    pub nullable: bool,
}

pub fn build_query(sql: &str) -> Frame {
    Frame::new(Op::Query, sql.as_bytes().to_vec())
}

pub fn parse_query(frame: &Frame) -> Result<&str, FrameError> {
    debug_assert!(matches!(frame.op, Op::Query));
    core::str::from_utf8(&frame.payload).map_err(|_| FrameError::InvalidUtf8)
}

/// Build an `Auth` frame carrying the candidate password.
pub fn build_auth(password: &str) -> Frame {
    Frame::new(Op::Auth, password.as_bytes().to_vec())
}

/// Read the candidate password out of an `Auth` frame. The bytes must
/// be valid UTF-8; non-UTF-8 inputs surface as a clear protocol error.
pub fn parse_auth(frame: &Frame) -> Result<&str, FrameError> {
    debug_assert!(matches!(frame.op, Op::Auth));
    core::str::from_utf8(&frame.payload).map_err(|_| FrameError::InvalidUtf8)
}

/// Build an `AuthUser` frame: `[u16 user_len][user][password]`.
pub fn build_auth_user(user: &str, password: &str) -> Result<Frame, FrameError> {
    let user_len = u16::try_from(user.len()).map_err(|_| FrameError::FieldTooLarge)?;
    let mut p = Vec::with_capacity(2 + user.len() + password.len());
    p.extend_from_slice(&user_len.to_le_bytes());
    p.extend_from_slice(user.as_bytes());
    p.extend_from_slice(password.as_bytes());
    Ok(Frame::new(Op::AuthUser, p))
}

/// Parse `(username, password)` out of an `AuthUser` frame. Both
/// slices must be valid UTF-8; truncated payloads surface as a clear
/// protocol error.
pub fn parse_auth_user(frame: &Frame) -> Result<(&str, &str), FrameError> {
    debug_assert!(matches!(frame.op, Op::AuthUser));
    if frame.payload.len() < 2 {
        return Err(FrameError::TruncatedPayload);
    }
    let user_len = u16::from_le_bytes([frame.payload[0], frame.payload[1]]) as usize;
    if 2 + user_len > frame.payload.len() {
        return Err(FrameError::TruncatedPayload);
    }
    let user_bytes = &frame.payload[2..2 + user_len];
    let pass_bytes = &frame.payload[2 + user_len..];
    let user = core::str::from_utf8(user_bytes).map_err(|_| FrameError::InvalidUtf8)?;
    let password = core::str::from_utf8(pass_bytes).map_err(|_| FrameError::InvalidUtf8)?;
    Ok((user, password))
}

pub fn build_row_description(cols: &[ColumnDesc]) -> Result<Frame, FrameError> {
    let count = u16::try_from(cols.len()).map_err(|_| FrameError::FieldTooLarge)?;
    let mut p = Vec::new();
    p.extend_from_slice(&count.to_le_bytes());
    for c in cols {
        p.push(c.ty as u8);
        let name_len = u16::try_from(c.name.len()).map_err(|_| FrameError::FieldTooLarge)?;
        p.extend_from_slice(&name_len.to_le_bytes());
        p.extend_from_slice(c.name.as_bytes());
        p.push(u8::from(c.nullable));
    }
    Ok(Frame::new(Op::RowDescription, p))
}

pub fn parse_row_description(frame: &Frame) -> Result<Vec<ColumnDesc>, FrameError> {
    let buf = &frame.payload;
    let (count, mut off) = read_u16(buf, 0)?;
    let mut cols = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (ty_byte, o1) = read_u8(buf, off)?;
        let ty = WireType::from_byte(ty_byte)?;
        let (name_len, o2) = read_u16(buf, o1)?;
        let end = o2
            .checked_add(name_len as usize)
            .ok_or(FrameError::FieldTooLarge)?;
        if buf.len() < end {
            return Err(FrameError::TruncatedPayload);
        }
        let name = core::str::from_utf8(&buf[o2..end])
            .map_err(|_| FrameError::InvalidUtf8)?
            .into();
        let (nullable_byte, o3) = read_u8(buf, end)?;
        cols.push(ColumnDesc {
            name,
            ty,
            nullable: nullable_byte != 0,
        });
        off = o3;
    }
    Ok(cols)
}

pub fn build_data_row(values: &[WireValue]) -> Result<Frame, FrameError> {
    let count = u16::try_from(values.len()).map_err(|_| FrameError::FieldTooLarge)?;
    let mut p = Vec::new();
    p.extend_from_slice(&count.to_le_bytes());
    for v in values {
        v.encode(&mut p)?;
    }
    Ok(Frame::new(Op::DataRow, p))
}

pub fn parse_data_row(frame: &Frame) -> Result<Vec<WireValue>, FrameError> {
    let buf = &frame.payload;
    let (count, mut off) = read_u16(buf, 0)?;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (v, next) = WireValue::decode(buf, off)?;
        out.push(v);
        off = next;
    }
    Ok(out)
}

/// Pack many rows into one frame. All rows must have the same
/// `cell_count`; the count is written once at the front of the
/// payload (saving 2 bytes per row vs a stream of `DataRow` frames).
pub fn build_data_row_batch(rows: &[Vec<WireValue>]) -> Result<Frame, FrameError> {
    let row_count = u16::try_from(rows.len()).map_err(|_| FrameError::FieldTooLarge)?;
    let cell_count =
        u16::try_from(rows.first().map_or(0, Vec::len)).map_err(|_| FrameError::FieldTooLarge)?;
    // Defensive: every row must agree on cell count. The server only
    // calls this with rows from one query result, so they always do —
    // assert in debug to catch shape bugs.
    debug_assert!(
        rows.iter().all(|r| r.len() == cell_count as usize),
        "DataRowBatch requires all rows to have the same cell count"
    );
    let mut p = Vec::with_capacity(4 + rows.len() * usize::from(cell_count) * 8);
    p.extend_from_slice(&row_count.to_le_bytes());
    p.extend_from_slice(&cell_count.to_le_bytes());
    for row in rows {
        for v in row {
            v.encode(&mut p)?;
        }
    }
    Ok(Frame::new(Op::DataRowBatch, p))
}

pub fn parse_data_row_batch(frame: &Frame) -> Result<Vec<Vec<WireValue>>, FrameError> {
    let buf = &frame.payload;
    let (row_count, off1) = read_u16(buf, 0)?;
    let (cell_count, mut off) = read_u16(buf, off1)?;
    let mut rows: Vec<Vec<WireValue>> = Vec::with_capacity(row_count as usize);
    for _ in 0..row_count {
        let mut row = Vec::with_capacity(cell_count as usize);
        for _ in 0..cell_count {
            let (v, next) = WireValue::decode(buf, off)?;
            row.push(v);
            off = next;
        }
        rows.push(row);
    }
    Ok(rows)
}

pub fn build_command_complete(affected: u64) -> Frame {
    let mut p = Vec::with_capacity(8);
    p.extend_from_slice(&affected.to_le_bytes());
    Frame::new(Op::CommandComplete, p)
}

pub fn parse_command_complete(frame: &Frame) -> Result<u64, FrameError> {
    let (n, _) = read_u64(&frame.payload, 0)?;
    Ok(n)
}

pub fn build_error_response(msg: &str) -> Frame {
    Frame::new(Op::ErrorResponse, msg.as_bytes().to_vec())
}

pub fn parse_error_response(frame: &Frame) -> Result<&str, FrameError> {
    core::str::from_utf8(&frame.payload).map_err(|_| FrameError::InvalidUtf8)
}

/// Build a `Stats` request frame. Payload is empty.
pub fn build_stats_request() -> Frame {
    Frame::new(Op::Stats, Vec::new())
}

/// Build a `StatsResponse` frame carrying a UTF-8 text body.
pub fn build_stats_response(body: &str) -> Frame {
    Frame::new(Op::StatsResponse, body.as_bytes().to_vec())
}

pub fn parse_stats_response(frame: &Frame) -> Result<&str, FrameError> {
    core::str::from_utf8(&frame.payload).map_err(|_| FrameError::InvalidUtf8)
}

// --- low-level cursor helpers ---------------------------------------------

fn read_u8(buf: &[u8], off: usize) -> Result<(u8, usize), FrameError> {
    if buf.len() <= off {
        return Err(FrameError::TruncatedPayload);
    }
    Ok((buf[off], off + 1))
}

fn read_u16(buf: &[u8], off: usize) -> Result<(u16, usize), FrameError> {
    let end = off + 2;
    if buf.len() < end {
        return Err(FrameError::TruncatedPayload);
    }
    let arr: [u8; 2] = buf[off..end].try_into().expect("checked");
    Ok((u16::from_le_bytes(arr), end))
}

fn read_u32(buf: &[u8], off: usize) -> Result<(u32, usize), FrameError> {
    let end = off + 4;
    if buf.len() < end {
        return Err(FrameError::TruncatedPayload);
    }
    let arr: [u8; 4] = buf[off..end].try_into().expect("checked");
    Ok((u32::from_le_bytes(arr), end))
}

fn read_u64(buf: &[u8], off: usize) -> Result<(u64, usize), FrameError> {
    let end = off + 8;
    if buf.len() < end {
        return Err(FrameError::TruncatedPayload);
    }
    let arr: [u8; 8] = buf[off..end].try_into().expect("checked");
    Ok((u64::from_le_bytes(arr), end))
}

fn read_i32(buf: &[u8], off: usize) -> Result<(i32, usize), FrameError> {
    let end = off + 4;
    if buf.len() < end {
        return Err(FrameError::TruncatedPayload);
    }
    let arr: [u8; 4] = buf[off..end].try_into().expect("checked");
    Ok((i32::from_le_bytes(arr), end))
}

fn read_i64(buf: &[u8], off: usize) -> Result<(i64, usize), FrameError> {
    let end = off + 8;
    if buf.len() < end {
        return Err(FrameError::TruncatedPayload);
    }
    let arr: [u8; 8] = buf[off..end].try_into().expect("checked");
    Ok((i64::from_le_bytes(arr), end))
}

fn read_f64(buf: &[u8], off: usize) -> Result<(f64, usize), FrameError> {
    let end = off + 8;
    if buf.len() < end {
        return Err(FrameError::TruncatedPayload);
    }
    let arr: [u8; 8] = buf[off..end].try_into().expect("checked");
    Ok((f64::from_le_bytes(arr), end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn auth_user_round_trip() {
        let f = build_auth_user("alice", "hunter2").unwrap();
        assert_eq!(f.op, Op::AuthUser);
        let (u, p) = parse_auth_user(&f).unwrap();
        assert_eq!(u, "alice");
        assert_eq!(p, "hunter2");
    }

    #[test]
    fn auth_user_empty_username_is_allowed_and_means_password_only() {
        let f = build_auth_user("", "secret").unwrap();
        let (u, p) = parse_auth_user(&f).unwrap();
        assert!(u.is_empty());
        assert_eq!(p, "secret");
    }

    #[test]
    fn auth_user_payload_too_short_is_caught() {
        let bad = Frame::new(Op::AuthUser, vec![0x05]); // only 1 byte
        assert!(matches!(
            parse_auth_user(&bad),
            Err(FrameError::TruncatedPayload)
        ));
    }

    #[test]
    fn auth_user_declared_user_len_past_end_is_caught() {
        // user_len = 10, but only 2 bytes follow
        let bad = Frame::new(Op::AuthUser, vec![10, 0, b'a', b'b']);
        assert!(matches!(
            parse_auth_user(&bad),
            Err(FrameError::TruncatedPayload)
        ));
    }

    #[test]
    fn round_trip_ping_pong_and_error() {
        let frames = [
            Frame::ping(),
            Frame::pong(),
            Frame::new(Op::Error, vec![b'b', b'a', b'd']),
        ];
        for frame in frames {
            let mut buf = Vec::new();
            encode(&frame, &mut buf).expect("encode");
            let (decoded, n) = decode(&buf).expect("decode");
            assert_eq!(decoded, frame);
            assert_eq!(n, buf.len());
        }
    }

    #[test]
    fn decode_short_header_at_every_partial_length() {
        for n in 0..FRAME_HEADER_LEN {
            let buf = vec![0u8; n];
            assert!(
                matches!(decode(&buf), Err(FrameError::ShortHeader)),
                "buf len {n} should be short-header"
            );
        }
    }

    #[test]
    fn decode_unknown_op() {
        let buf = [0, 0, 0, 0, 0x42];
        assert!(matches!(decode(&buf), Err(FrameError::UnknownOp(0x42))));
    }

    #[test]
    fn decode_payload_too_large() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        buf.push(Op::Ping as u8);
        assert!(
            matches!(decode(&buf), Err(FrameError::PayloadTooLarge(n)) if n == MAX_PAYLOAD + 1)
        );
    }

    #[test]
    fn decode_short_payload_signals_need_more_bytes() {
        // Header claims 4-byte payload; only 2 bytes follow.
        let mut buf = Vec::new();
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.push(Op::Error as u8);
        buf.extend_from_slice(&[0, 0]);
        assert!(matches!(decode(&buf), Err(FrameError::ShortPayload)));
    }

    // --- v0.5 value codec / opcode helpers --------------------------------

    fn round_trip_value(v: &WireValue) {
        let mut buf = Vec::new();
        v.encode(&mut buf).unwrap();
        let (decoded, n) = WireValue::decode(&buf, 0).unwrap();
        assert_eq!(&decoded, v);
        assert_eq!(n, buf.len());
    }

    #[test]
    fn value_codec_round_trip_each_type() {
        round_trip_value(&WireValue::Null);
        round_trip_value(&WireValue::Int(-42));
        round_trip_value(&WireValue::BigInt(i64::MIN));
        // Pick a finite f64 that the codec must round-trip bitwise. Avoid
        // π (clippy::approx_constant) — any non-special value works.
        round_trip_value(&WireValue::Float(1.234_567_891_234_5));
        round_trip_value(&WireValue::Text("hello — UTF-8 ✓".into()));
        round_trip_value(&WireValue::Bool(true));
        round_trip_value(&WireValue::Bool(false));
    }

    #[test]
    fn value_decode_truncated_text_errors() {
        let mut buf = Vec::new();
        // Claim a 10-byte text but only provide 3.
        buf.push(WireType::Text as u8);
        buf.extend_from_slice(&10u32.to_le_bytes());
        buf.extend_from_slice(b"abc");
        assert!(matches!(
            WireValue::decode(&buf, 0),
            Err(FrameError::TruncatedPayload)
        ));
    }

    #[test]
    fn value_decode_unknown_type_tag_errors() {
        let buf = [0xEE_u8];
        assert!(matches!(
            WireValue::decode(&buf, 0),
            Err(FrameError::UnknownWireType(0xEE))
        ));
    }

    #[test]
    fn query_frame_round_trip() {
        let f = build_query("SELECT 1");
        assert_eq!(f.op, Op::Query);
        assert_eq!(parse_query(&f).unwrap(), "SELECT 1");
    }

    #[test]
    fn row_description_round_trip() {
        let cols = vec![
            ColumnDesc {
                name: "id".into(),
                ty: WireType::BigInt,
                nullable: false,
            },
            ColumnDesc {
                name: "score".into(),
                ty: WireType::Float,
                nullable: true,
            },
        ];
        let f = build_row_description(&cols).unwrap();
        assert_eq!(f.op, Op::RowDescription);
        assert_eq!(parse_row_description(&f).unwrap(), cols);
    }

    #[test]
    fn row_description_empty_column_list() {
        let f = build_row_description(&[]).unwrap();
        assert!(parse_row_description(&f).unwrap().is_empty());
    }

    #[test]
    fn data_row_round_trip_mixed_types() {
        let row = vec![
            WireValue::BigInt(1),
            WireValue::Text("alice".into()),
            WireValue::Null,
            WireValue::Float(99.5),
            WireValue::Bool(true),
        ];
        let f = build_data_row(&row).unwrap();
        assert_eq!(f.op, Op::DataRow);
        assert_eq!(parse_data_row(&f).unwrap(), row);
    }

    #[test]
    fn command_complete_round_trip() {
        let f = build_command_complete(7);
        assert_eq!(f.op, Op::CommandComplete);
        assert_eq!(parse_command_complete(&f).unwrap(), 7);
    }

    #[test]
    fn error_response_round_trip() {
        let f = build_error_response("table not found: ghost");
        assert_eq!(f.op, Op::ErrorResponse);
        assert_eq!(parse_error_response(&f).unwrap(), "table not found: ghost");
    }

    #[test]
    fn stats_request_and_response_round_trip() {
        let req = build_stats_request();
        assert_eq!(req.op, Op::Stats);
        assert!(req.payload.is_empty());

        let resp = build_stats_response("tables=2 rows=42");
        assert_eq!(resp.op, Op::StatsResponse);
        assert_eq!(parse_stats_response(&resp).unwrap(), "tables=2 rows=42");
    }

    #[test]
    fn frame_decode_recognises_new_opcodes() {
        for op in [
            Op::Query,
            Op::RowDescription,
            Op::DataRow,
            Op::CommandComplete,
            Op::ErrorResponse,
            Op::Stats,
            Op::StatsResponse,
        ] {
            let mut buf = Vec::new();
            encode(&Frame::new(op, vec![]), &mut buf).unwrap();
            let (decoded, _) = decode(&buf).unwrap();
            assert_eq!(decoded.op, op);
        }
    }

    #[test]
    fn two_frames_back_to_back_decode_independently() {
        let mut wire = Vec::new();
        encode(&Frame::ping(), &mut wire).unwrap();
        encode(&Frame::error("nope"), &mut wire).unwrap();

        let (first, n1) = decode(&wire).unwrap();
        assert_eq!(first, Frame::ping());
        let (second, n2) = decode(&wire[n1..]).unwrap();
        assert_eq!(second.op, Op::Error);
        assert_eq!(&second.payload, b"nope");
        assert_eq!(n1 + n2, wire.len());
    }
}
