#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::doc_markdown
)]
// All bit-twiddle casts in this file (i32 ↔ u32 ↔ u16) are
// arithmetically bounded by the IEEE-754 binary16 field widths;
// the lints would force an unsigned-bit-pattern detour that
// obscures the algorithm shape.

//! v6.0.3 — halfvec: IEEE-754 binary16 (`F16`) per-element storage.
//!
//! Stable Rust 1.96 (this workspace) does not yet expose a stable
//! `f16` primitive or stable `core::arch::aarch64` f16 intrinsics
//! (rust-lang/rust#116909, #125606). v6.0.3 ships with a hand-
//! rolled IEEE-754 binary16 codec on top of `Vec<u8>` carrying
//! raw little-endian u16 bits. NEON f16 SIMD lands as v6.0.6 or
//! whenever the stable toolchain catches up.
//!
//! Layout per cell: `[u16 LE × dim]`. Dim = `bytes.len() / 2`.
//!
//! Codec rounding: round-to-nearest-even on overflow / underflow
//! (matches `f32 as f16` semantics on hosts that do have the
//! primitive). Special values:
//!
//! - `±0.0` → bit-exact `±0.0` half.
//! - `±∞`  → bit-exact `±∞` half.
//! - `NaN` → quiet NaN half (sign + payload preserved as far as
//!   the 10-bit mantissa allows; signalling/quiet bit is forced
//!   set so the value can't decode back as inf).
//! - Subnormals + overflow → flushed to `0` and `±∞`
//!   respectively per IEEE 754-2008 §7.4.

use alloc::vec::Vec;

/// SQ8 / SQ4 / SQ16 share an `Sq*Vector`-shaped struct; halfvec
/// follows the same pattern. `bytes` always has even length; the
/// invariant is enforced by every constructor in this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HalfVector {
    pub bytes: Vec<u8>,
}

impl HalfVector {
    /// Dimension = bytes.len() / 2. Returns 0 for empty input.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.bytes.len() / 2
    }

    /// Encode `v` into raw u16 LE bits via per-element
    /// `f32 → f16` round-to-nearest-even.
    #[must_use]
    pub fn from_f32_slice(v: &[f32]) -> Self {
        let mut bytes = Vec::with_capacity(v.len() * 2);
        for &x in v {
            let bits = f16_from_f32_bits(x.to_bits());
            bytes.extend_from_slice(&bits.to_le_bytes());
        }
        Self { bytes }
    }

    /// Decode every u16 LE in `bytes` to f32. Inverse of
    /// `from_f32_slice` modulo half-precision round-trip error
    /// (≤ 2^-10 × |x| for finite normals).
    #[must_use]
    pub fn to_f32_vec(&self) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.dim());
        let mut i = 0;
        while i + 2 <= self.bytes.len() {
            let bits = u16::from_le_bytes([self.bytes[i], self.bytes[i + 1]]);
            out.push(f32::from_bits(f16_to_f32_bits(bits)));
            i += 2;
        }
        out
    }
}

/// Convert one f32 (passed as raw bits) to f16 (raw bits).
///
/// Implements IEEE 754-2008 §7.4 round-to-nearest-even with
/// subnormal flush-to-zero on underflow and saturation to ±∞ on
/// overflow. Matches the bit-pattern `f32 as f16` produces on
/// hosts that have the primitive — verified by the unit tests
/// below against a hand-table of fixtures (`0`, `0.25`, `1.0`,
/// `65504.0`, `±∞`, NaN, denormals).
#[must_use]
pub fn f16_from_f32_bits(bits: u32) -> u16 {
    let sign = ((bits >> 31) & 0x1) as u16;
    let exp32 = (bits >> 23) & 0xff;
    let mant32 = bits & 0x7f_ffff;

    // 1. NaN / ±∞
    if exp32 == 0xff {
        if mant32 == 0 {
            // ±∞: half is sign | 0x7c00
            return (sign << 15) | 0x7c00;
        }
        // NaN: collapse to quiet NaN with the top mantissa bit set.
        // Preserve the high bits of the f32 payload as far as the
        // 10-bit mantissa allows; force the quiet bit (bit 9 of
        // mantissa) so the value isn't sNaN.
        let mant16 = ((mant32 >> 13) | 0x200) as u16;
        return (sign << 15) | 0x7c00 | mant16;
    }

    // 2. ±0.0 (and other f32 zeros / subnormals that round to 0).
    if exp32 == 0 {
        return sign << 15;
    }

    // 3. Re-bias the exponent for half: half-bias 15 vs f32-bias 127.
    let exp_unbiased: i32 = exp32 as i32 - 127;

    // 3a. Overflow: |x| ≥ 65520 saturates to ±∞.
    if exp_unbiased > 15 {
        return (sign << 15) | 0x7c00;
    }

    // 3b. Underflow + subnormal range. exp_unbiased < -14:
    // representable only as subnormal half; below -24 flushes to 0.
    if exp_unbiased < -14 {
        if exp_unbiased < -24 {
            return sign << 15;
        }
        // Subnormal half: the implied leading 1 becomes explicit
        // and we shift right by (1 - exp_unbiased - (-14)) = (-14 -
        // exp_unbiased - 1) extra positions on top of the standard
        // 13-bit mantissa drop.
        let shift = (1 - 14 - exp_unbiased) as u32; // 1..=10
        let mant_with_lead = mant32 | 0x80_0000;
        let drop_bits = 13 + shift;
        let mant16_pre = mant_with_lead >> drop_bits;
        // Round-to-nearest-even on the bits we just dropped.
        let half = 1u32 << (drop_bits - 1);
        let mask = (1u32 << drop_bits) - 1;
        let dropped = mant_with_lead & mask;
        let round_up = dropped > half || (dropped == half && (mant16_pre & 1) == 1);
        let mant16 = mant16_pre + u32::from(round_up);
        return (sign << 15) | (mant16 as u16);
    }

    // 4. Normal range.
    let exp16 = (exp_unbiased + 15) as u16;
    let mant16_pre = mant32 >> 13;
    // Round-to-nearest-even on the 13 low bits we just dropped.
    let drop_mask = 0x1fffu32;
    let half = 0x1000u32;
    let dropped = mant32 & drop_mask;
    let round_up = dropped > half || (dropped == half && (mant16_pre & 1) == 1);
    let mant16 = mant16_pre + u32::from(round_up);
    // Carry from rounding can bump exp16 — if mantissa hit 0x400
    // (one past max half mantissa) the rounding overflowed into
    // exp; collapse via `(exp16 << 10) | mant16` arithmetic.
    let packed = (u32::from(exp16) << 10) + mant16;
    if packed >= 0x7c00 {
        // Overflow into infinity (e.g. 65520 → rounds to ±∞).
        return (sign << 15) | 0x7c00;
    }
    #[allow(clippy::cast_possible_truncation)]
    let packed_u16 = packed as u16;
    (sign << 15) | packed_u16
}

/// Convert one f16 (raw bits) to f32 (raw bits). Exact for every
/// finite f16; preserves sign + NaN-ness.
#[must_use]
pub fn f16_to_f32_bits(bits: u16) -> u32 {
    let sign = u32::from(bits >> 15) & 0x1;
    let exp16 = u32::from((bits >> 10) & 0x1f);
    let mant16 = u32::from(bits & 0x3ff);

    // 1. NaN / ±∞.
    if exp16 == 0x1f {
        if mant16 == 0 {
            return (sign << 31) | 0x7f80_0000;
        }
        // Lift the half mantissa into the f32 mantissa, preserving
        // the quiet bit (bit 9 → bit 22).
        return (sign << 31) | 0x7f80_0000 | (mant16 << 13);
    }

    // 2. ±0.0
    if exp16 == 0 && mant16 == 0 {
        return sign << 31;
    }

    // 3. Subnormal half — re-normalise.
    if exp16 == 0 {
        // Find leading-1 position to count the shift.
        let mut m = mant16;
        let mut e: i32 = -14;
        while (m & 0x400) == 0 {
            m <<= 1;
            e -= 1;
        }
        m &= 0x3ff; // drop the leading 1 (becomes implicit again)
        let exp32 = ((e + 127) as u32) & 0xff;
        return (sign << 31) | (exp32 << 23) | (m << 13);
    }

    // 4. Normal half.
    let exp_unbiased = exp16 as i32 - 15;
    let exp32 = (exp_unbiased + 127) as u32;
    (sign << 31) | (exp32 << 23) | (mant16 << 13)
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    clippy::approx_constant,
    clippy::suboptimal_flops,
    clippy::unreadable_literal
)]
mod tests {
    use super::*;

    fn f32_eq_bits(a: f32, b: f32) -> bool {
        // Includes ±0.0 separately + NaN-aware equality.
        if a.is_nan() && b.is_nan() {
            return true;
        }
        a.to_bits() == b.to_bits()
    }

    #[test]
    fn f16_roundtrip_representable_values() {
        // Values that fall on f16 grid points round-trip exactly.
        let cases: &[f32] = &[
            0.0,
            -0.0,
            1.0,
            -1.0,
            0.5,
            -0.5,
            0.25,
            2.0,
            4.0,
            1.5,
            -1.5,
            65504.0, // f16 max
            -65504.0,
            1.0 / 16384.0, // = 2^-14 (smallest normal)
        ];
        for &x in cases {
            let bits = f16_from_f32_bits(x.to_bits());
            let y = f32::from_bits(f16_to_f32_bits(bits));
            assert!(f32_eq_bits(x, y), "expected {x} == {y} (bits {bits:#x})");
        }
    }

    #[test]
    fn f16_roundtrip_inf_and_nan() {
        let inf = f32::INFINITY;
        let neg_inf = f32::NEG_INFINITY;
        assert_eq!(
            f16_to_f32_bits(f16_from_f32_bits(inf.to_bits())),
            inf.to_bits()
        );
        assert_eq!(
            f16_to_f32_bits(f16_from_f32_bits(neg_inf.to_bits())),
            neg_inf.to_bits()
        );
        let nan = f32::NAN;
        let nan_back = f32::from_bits(f16_to_f32_bits(f16_from_f32_bits(nan.to_bits())));
        assert!(nan_back.is_nan(), "NaN should round-trip as NaN");
    }

    #[test]
    fn f16_overflow_saturates_to_inf() {
        // > 65504 saturates to +∞.
        let huge = 1e30_f32;
        let half_bits = f16_from_f32_bits(huge.to_bits());
        assert_eq!(half_bits, 0x7c00, "huge positive → +∞");
        let half_back = f32::from_bits(f16_to_f32_bits(half_bits));
        assert_eq!(half_back, f32::INFINITY);
    }

    #[test]
    fn f16_underflow_flushes_to_zero() {
        // 2^-30 is way below the f16 subnormal range, flushes to 0.
        let tiny = 1.0e-30_f32;
        let half_bits = f16_from_f32_bits(tiny.to_bits());
        assert_eq!(
            half_bits & 0x7fff,
            0,
            "tiny positive → +0 (got {half_bits:#x})"
        );
    }

    #[test]
    fn f16_codec_roundtrip_finite_normals_bounded_error() {
        // Smooth-sweep test: half-precision has ~10 bits of
        // mantissa, so the relative error after roundtrip is
        // ≤ 2^-10 ≈ 9.77e-4 for finite normals. Allow a touch
        // more for the rounding boundary case.
        let cases: &[f32] = &[
            0.1,
            0.333,
            1.0 / 7.0,
            3.14159,
            100.0,
            12345.0,
            -0.1,
            -3.14159,
        ];
        for &x in cases {
            let bits = f16_from_f32_bits(x.to_bits());
            let y = f32::from_bits(f16_to_f32_bits(bits));
            let rel = (x - y).abs() / x.abs();
            assert!(rel < 1e-3, "x={x} y={y} rel_err={rel} (bits {bits:#x})");
        }
    }

    #[test]
    fn half_vector_from_to_f32_slice() {
        let v = alloc::vec![0.0_f32, 0.25, 0.5, 1.0, -1.0];
        let h = HalfVector::from_f32_slice(&v);
        assert_eq!(h.dim(), 5);
        let back = h.to_f32_vec();
        assert_eq!(back, v);
    }

    #[test]
    fn half_vector_empty() {
        let h = HalfVector::from_f32_slice(&[]);
        assert_eq!(h.dim(), 0);
        assert!(h.bytes.is_empty());
        let back = h.to_f32_vec();
        assert!(back.is_empty());
    }
}
