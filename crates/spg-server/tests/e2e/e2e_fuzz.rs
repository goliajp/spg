//! v4.31 wire-frame fuzz harness — randomized payloads against
//! the wire decoder. Smaller scale than `cargo fuzz` (which
//! requires nightly + libFuzzer) but enough to keep adversarial
//! inputs from slipping past CI.
//!
//! SQL parser fuzz lives in `crates/spg-sql/tests/fuzz.rs` (no
//! dep cycle there). This file covers the wire layer only.
//!
//! By default each harness runs `DEFAULT_ITERS` inputs (~10K).
//! Set `SPG_FUZZ_ITERS=N` to run more — e.g. an operator can do
//! `SPG_FUZZ_ITERS=10000000` overnight without recompiling.
//!
//! Determinism: seeded SplitMix64 PRNG. The bar: **no panics**.

use std::time::Instant;

const DEFAULT_ITERS: u64 = 10_000;

/// Deterministic SplitMix64 PRNG. Public-domain algorithm; small
/// enough to inline so the test stays free of external deps.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn next_byte(&mut self) -> u8 {
        (self.next_u64() & 0xFF) as u8
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + ((self.next_u64() as usize) % (hi - lo))
    }
    #[allow(dead_code)] // kept for parity with spg-sql/tests/fuzz.rs
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.range(0, xs.len())]
    }
}

fn iters() -> u64 {
    std::env::var("SPG_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_ITERS)
}

/// Fuzz the wire frame decoder with random payloads and random
/// opcodes. Encode/decode round-trip on well-formed payloads;
/// no panic on adversarial ones.
#[test]
fn fuzz_wire_frame_does_not_panic() {
    use spg_wire::{Frame, Op, decode};
    let mut rng = SplitMix64::new(0xDEAD_BEEF);
    let n = iters();
    let started = Instant::now();
    let mut decoded = 0u64;
    let mut rejected = 0u64;
    let mut buf = Vec::with_capacity(8192);
    for _ in 0..n {
        buf.clear();
        let mode = rng.next_byte() & 0b11;
        if mode == 0 {
            // pure random byte soup
            let len = rng.range(0, 256);
            for _ in 0..len {
                buf.push(rng.next_byte());
            }
        } else {
            // mostly-well-formed frame
            let payload_len = rng.range(0, 256) as u32;
            buf.extend_from_slice(&payload_len.to_le_bytes());
            buf.push(rng.next_byte());
            for _ in 0..payload_len {
                buf.push(rng.next_byte());
            }
        }
        match decode(&buf) {
            Ok(_) => decoded += 1,
            Err(_) => rejected += 1,
        }
    }
    eprintln!(
        "wire fuzz: {n} iters in {:?}, decoded={decoded} rejected={rejected}",
        started.elapsed()
    );
    // Round-trip sanity: every documented opcode encodes + decodes.
    for op_byte in [
        0x00_u8, 0x01, 0x02, 0x03, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0xFF,
    ] {
        let op = Op::from_byte(op_byte).unwrap();
        let f = Frame {
            op,
            payload: alloc_payload(b"abc"),
        };
        let mut out = Vec::new();
        spg_wire::encode(&f, &mut out).unwrap();
        let (decoded, consumed) = decode(&out).unwrap();
        assert_eq!(decoded.op, f.op);
        assert_eq!(decoded.payload, f.payload);
        assert_eq!(consumed, out.len());
    }
}

fn alloc_payload(b: &[u8]) -> Vec<u8> {
    b.to_vec()
}
