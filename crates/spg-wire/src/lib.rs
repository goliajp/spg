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
    Error = 0xFF,
}

impl Op {
    pub const fn from_byte(b: u8) -> Result<Self, FrameError> {
        match b {
            0x00 => Ok(Self::Ping),
            0x01 => Ok(Self::Pong),
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

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
