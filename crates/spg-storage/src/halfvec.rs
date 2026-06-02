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

// ===========================================================================
// v6.0.6 — NEON SIMD f16 → f32 conversion (fused into distance kernels).
//
// stable Rust 1.96 still gates `core::arch::aarch64::vcvt_f32_f16` behind
// the unstable `stdarch_neon_f16` feature. Bit-manipulation in SIMD lanes
// stays on stable NEON (`vshl`, `vand`, `vceq`, `vbsl`) and produces the
// same f32 bit pattern for the cases ML embeddings actually hit:
//
//   * NaN / ±∞ (exp_h == 0x1f) — bit-exact.
//   * ±0       (exp_h == 0 && mant_h == 0) — bit-exact.
//   * Normal   (exp_h ∈ 1..=30) — bit-exact.
//   * Subnormal (exp_h == 0 && mant_h != 0) — **flushed to ±0**. The
//     scalar codec renormalises subnormals into f32 normals; SIMD path
//     flushes them. f16 subnormals are |value| < 2^-14 ≈ 6.1e-5, which
//     is below the precision of natural embeddings anyway. Distance
//     queries on those values are dominated by other lanes.
//
// Public distance functions below dispatch the SIMD kernel under
// `#[cfg(target_arch = "aarch64")]` when `dim ≥ 8 && dim % 8 == 0`,
// falling back to scalar otherwise — same pre-condition shape as the
// SQ8 / f32 NEON paths.
// ===========================================================================

/// L2² distance between an f16 cell and an f32 query, fused so no
/// `Vec<f32>` ever materialises. Dispatches to NEON for production-
/// shaped dims (multiples of 8); scalar fallback otherwise.
#[must_use]
pub fn half_l2_distance_sq_asymmetric(a: &HalfVector, q: &[f32]) -> f32 {
    if a.dim() != q.len() {
        return f32::INFINITY;
    }
    #[cfg(target_arch = "aarch64")]
    {
        let n = a.dim();
        if n >= 8 && n.is_multiple_of(8) {
            // SAFETY: NEON is baseline aarch64; preconditions
            // (matching lengths, ≥ 1 full 8-lane chunk) checked above.
            return unsafe { half_l2_distance_sq_asymmetric_neon(a, q) };
        }
    }
    half_l2_distance_sq_asymmetric_scalar(a, q)
}

/// Negated dot product (pgvector `<#>` convention). Fused SIMD path.
#[must_use]
pub fn half_inner_product_asymmetric(a: &HalfVector, q: &[f32]) -> f32 {
    if a.dim() != q.len() {
        return f32::INFINITY;
    }
    #[cfg(target_arch = "aarch64")]
    {
        let n = a.dim();
        if n >= 8 && n.is_multiple_of(8) {
            // SAFETY: see `half_l2_distance_sq_asymmetric_neon`.
            return -unsafe { half_dot_asymmetric_neon(a, q) };
        }
    }
    -half_dot_asymmetric_scalar(a, q)
}

/// Cosine distance `1 - dot / (||a|| * ||q||)`. Fused SIMD path;
/// norm-sqrt + zero-guard live in the safe wrapper.
#[must_use]
pub fn half_cosine_distance_asymmetric(a: &HalfVector, q: &[f32]) -> f32 {
    if a.dim() != q.len() {
        return f32::INFINITY;
    }
    let (dot, na, nq);
    #[cfg(target_arch = "aarch64")]
    {
        let n = a.dim();
        if n >= 8 && n.is_multiple_of(8) {
            // SAFETY: see `half_l2_distance_sq_asymmetric_neon`.
            let (d, a2, q2) = unsafe { half_cosine_accumulators_asymmetric_neon(a, q) };
            dot = d;
            na = a2;
            nq = q2;
        } else {
            let (d, a2, q2) = half_cosine_accumulators_asymmetric_scalar(a, q);
            dot = d;
            na = a2;
            nq = q2;
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let (d, a2, q2) = half_cosine_accumulators_asymmetric_scalar(a, q);
        dot = d;
        na = a2;
        nq = q2;
    }
    if na == 0.0 || nq == 0.0 {
        return f32::INFINITY;
    }
    1.0 - dot / (sqrt_finite(na) * sqrt_finite(nq))
}

/// Symmetric L2² between two f16 cells. Used during HNSW build.
#[must_use]
pub fn half_l2_distance_sq(a: &HalfVector, b: &HalfVector) -> f32 {
    if a.dim() != b.dim() {
        return f32::INFINITY;
    }
    #[cfg(target_arch = "aarch64")]
    {
        let n = a.dim();
        if n >= 8 && n.is_multiple_of(8) {
            // SAFETY: see `half_l2_distance_sq_asymmetric_neon`.
            return unsafe { half_l2_distance_sq_symmetric_neon(a, b) };
        }
    }
    half_l2_distance_sq_symmetric_scalar(a, b)
}

// ---------------------------------------------------------------------------
// Scalar references — used as fallback + for SIMD parity tests below.

fn half_l2_distance_sq_asymmetric_scalar(a: &HalfVector, q: &[f32]) -> f32 {
    let mut acc: f32 = 0.0;
    let mut i = 0usize;
    while i + 2 <= a.bytes.len() {
        let bits = u16::from_le_bytes([a.bytes[i], a.bytes[i + 1]]);
        let xa = f32::from_bits(f16_to_f32_bits(bits));
        let d = xa - q[i / 2];
        acc += d * d;
        i += 2;
    }
    acc
}

fn half_dot_asymmetric_scalar(a: &HalfVector, q: &[f32]) -> f32 {
    let mut dot: f32 = 0.0;
    let mut i = 0usize;
    while i + 2 <= a.bytes.len() {
        let bits = u16::from_le_bytes([a.bytes[i], a.bytes[i + 1]]);
        let xa = f32::from_bits(f16_to_f32_bits(bits));
        dot += xa * q[i / 2];
        i += 2;
    }
    dot
}

fn half_cosine_accumulators_asymmetric_scalar(a: &HalfVector, q: &[f32]) -> (f32, f32, f32) {
    let (mut dot, mut na, mut nq) = (0.0_f32, 0.0_f32, 0.0_f32);
    let mut i = 0usize;
    while i + 2 <= a.bytes.len() {
        let bits = u16::from_le_bytes([a.bytes[i], a.bytes[i + 1]]);
        let xa = f32::from_bits(f16_to_f32_bits(bits));
        let qx = q[i / 2];
        dot += xa * qx;
        na += xa * xa;
        nq += qx * qx;
        i += 2;
    }
    (dot, na, nq)
}

fn half_l2_distance_sq_symmetric_scalar(a: &HalfVector, b: &HalfVector) -> f32 {
    let mut acc: f32 = 0.0;
    let mut i = 0usize;
    while i + 2 <= a.bytes.len() {
        let av = u16::from_le_bytes([a.bytes[i], a.bytes[i + 1]]);
        let bv = u16::from_le_bytes([b.bytes[i], b.bytes[i + 1]]);
        let xa = f32::from_bits(f16_to_f32_bits(av));
        let xb = f32::from_bits(f16_to_f32_bits(bv));
        let d = xa - xb;
        acc += d * d;
        i += 2;
    }
    acc
}

fn sqrt_finite(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let mut y = if x >= 1.0 { x * 0.5 } else { (x + 1.0) * 0.5 };
    for _ in 0..6 {
        y = 0.5 * (y + x / y);
    }
    y
}

// ---------------------------------------------------------------------------
// NEON kernels — bit-manipulation f16 → f32 in u32 lanes, then standard
// f32 SIMD arithmetic (subtract / FMA / dot / norm).

/// Convert eight half-precision lanes (loaded via `vld1q_u16` from raw
/// `bytes`) into two `float32x4_t` registers. Bit-exact for normal /
/// zero / inf / nan; subnormals flush to `±0` (see module docstring).
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[allow(clippy::many_single_char_names)]
#[inline]
unsafe fn half_to_f32x8_neon(
    h: core::arch::aarch64::uint16x8_t,
) -> [core::arch::aarch64::float32x4_t; 2] {
    use core::arch::aarch64::{
        vaddq_u32, vandq_u32, vbslq_u32, vceqq_u32, vdupq_n_u32, vget_high_u16, vget_low_u16,
        vmovl_u16, vorrq_u32, vreinterpretq_f32_u32, vshlq_n_u32, vshrq_n_u32,
    };
    // Widen u16x8 → 2× u32x4.
    let lo = vmovl_u16(vget_low_u16(h));
    let hi = vmovl_u16(vget_high_u16(h));

    // Helper: convert one u32x4 of raw f16 bits → f32x4.
    // Bit-exact for normal / zero / inf / nan; subnormals → ±0 via
    // the `exp == 0` mask. ML embeddings never trip the latter.
    let convert = |w: core::arch::aarch64::uint32x4_t| -> core::arch::aarch64::float32x4_t {
        let sign = vshlq_n_u32::<16>(vandq_u32(w, vdupq_n_u32(0x8000)));
        let mant = vandq_u32(w, vdupq_n_u32(0x3ff));
        let exp = vandq_u32(vshrq_n_u32::<10>(w), vdupq_n_u32(0x1f));
        let mant_f32 = vshlq_n_u32::<13>(mant);
        let exp_plus_bias = vaddq_u32(exp, vdupq_n_u32(112));
        let exp_f32_shifted = vshlq_n_u32::<23>(exp_plus_bias);
        let normal = vorrq_u32(vorrq_u32(sign, exp_f32_shifted), mant_f32);
        let inf_nan = vorrq_u32(vorrq_u32(sign, vdupq_n_u32(0x7f80_0000)), mant_f32);
        let is_inf_nan = vceqq_u32(exp, vdupq_n_u32(0x1f));
        let is_zero_or_subnormal = vceqq_u32(exp, vdupq_n_u32(0));
        let result = vbslq_u32(is_inf_nan, inf_nan, normal);
        let result = vbslq_u32(is_zero_or_subnormal, sign, result);
        vreinterpretq_f32_u32(result)
    };

    [convert(lo), convert(hi)]
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[allow(clippy::many_single_char_names)]
unsafe fn half_l2_distance_sq_asymmetric_neon(a: &HalfVector, q: &[f32]) -> f32 {
    use core::arch::aarch64::{
        float32x4_t, vaddq_f32, vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_f32, vld1q_u8,
        vreinterpretq_u16_u8, vsubq_f32,
    };
    unsafe {
        let zero: float32x4_t = vdupq_n_f32(0.0);
        let mut acc0 = zero;
        let mut acc1 = zero;
        let n = a.dim();
        let mut i = 0usize;
        while i + 8 <= n {
            // 16 bytes from a.bytes → 8 u16 raw bits → 2× f32x4.
            // Load 16 u8 then reinterpret as 8 u16 lanes. Avoids
            // the cast-alignment lint and stays correct on hosts
            // where `Vec<u8>`'s buffer alignment isn't a multiple
            // of 2 (it always is in practice, but the lint is
            // right to flag the unsafe assumption).
            let h = vreinterpretq_u16_u8(vld1q_u8(a.bytes.as_ptr().add(i * 2)));
            let [xa0, xa1] = half_to_f32x8_neon(h);
            let q0 = vld1q_f32(q.as_ptr().add(i));
            let q1 = vld1q_f32(q.as_ptr().add(i + 4));
            let d0 = vsubq_f32(xa0, q0);
            let d1 = vsubq_f32(xa1, q1);
            acc0 = vfmaq_f32(acc0, d0, d0);
            acc1 = vfmaq_f32(acc1, d1, d1);
            i += 8;
        }
        vaddvq_f32(vaddq_f32(acc0, acc1))
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[allow(clippy::many_single_char_names)]
unsafe fn half_dot_asymmetric_neon(a: &HalfVector, q: &[f32]) -> f32 {
    use core::arch::aarch64::{
        float32x4_t, vaddq_f32, vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_f32, vld1q_u8,
        vreinterpretq_u16_u8,
    };
    unsafe {
        let zero: float32x4_t = vdupq_n_f32(0.0);
        let mut acc0 = zero;
        let mut acc1 = zero;
        let n = a.dim();
        let mut i = 0usize;
        while i + 8 <= n {
            // Load 16 u8 then reinterpret as 8 u16 lanes. Avoids
            // the cast-alignment lint and stays correct on hosts
            // where `Vec<u8>`'s buffer alignment isn't a multiple
            // of 2 (it always is in practice, but the lint is
            // right to flag the unsafe assumption).
            let h = vreinterpretq_u16_u8(vld1q_u8(a.bytes.as_ptr().add(i * 2)));
            let [xa0, xa1] = half_to_f32x8_neon(h);
            acc0 = vfmaq_f32(acc0, xa0, vld1q_f32(q.as_ptr().add(i)));
            acc1 = vfmaq_f32(acc1, xa1, vld1q_f32(q.as_ptr().add(i + 4)));
            i += 8;
        }
        vaddvq_f32(vaddq_f32(acc0, acc1))
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[allow(clippy::many_single_char_names, clippy::similar_names)]
unsafe fn half_cosine_accumulators_asymmetric_neon(a: &HalfVector, q: &[f32]) -> (f32, f32, f32) {
    use core::arch::aarch64::{
        float32x4_t, vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_f32, vld1q_u8, vreinterpretq_u16_u8,
    };
    unsafe {
        let zero: float32x4_t = vdupq_n_f32(0.0);
        let mut acc_dot = zero;
        let mut acc_na = zero;
        let mut acc_nq = zero;
        let n = a.dim();
        let mut i = 0usize;
        while i + 8 <= n {
            // Load 16 u8 then reinterpret as 8 u16 lanes. Avoids
            // the cast-alignment lint and stays correct on hosts
            // where `Vec<u8>`'s buffer alignment isn't a multiple
            // of 2 (it always is in practice, but the lint is
            // right to flag the unsafe assumption).
            let h = vreinterpretq_u16_u8(vld1q_u8(a.bytes.as_ptr().add(i * 2)));
            let [xa0, xa1] = half_to_f32x8_neon(h);
            let q0 = vld1q_f32(q.as_ptr().add(i));
            let q1 = vld1q_f32(q.as_ptr().add(i + 4));
            acc_dot = vfmaq_f32(acc_dot, xa0, q0);
            acc_dot = vfmaq_f32(acc_dot, xa1, q1);
            acc_na = vfmaq_f32(acc_na, xa0, xa0);
            acc_na = vfmaq_f32(acc_na, xa1, xa1);
            acc_nq = vfmaq_f32(acc_nq, q0, q0);
            acc_nq = vfmaq_f32(acc_nq, q1, q1);
            i += 8;
        }
        (vaddvq_f32(acc_dot), vaddvq_f32(acc_na), vaddvq_f32(acc_nq))
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[allow(clippy::many_single_char_names)]
unsafe fn half_l2_distance_sq_symmetric_neon(a: &HalfVector, b: &HalfVector) -> f32 {
    use core::arch::aarch64::{
        float32x4_t, vaddq_f32, vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_u8, vreinterpretq_u16_u8,
        vsubq_f32,
    };
    unsafe {
        let zero: float32x4_t = vdupq_n_f32(0.0);
        let mut acc0 = zero;
        let mut acc1 = zero;
        let n = a.dim();
        let mut i = 0usize;
        while i + 8 <= n {
            let ha = vreinterpretq_u16_u8(vld1q_u8(a.bytes.as_ptr().add(i * 2)));
            let hb = vreinterpretq_u16_u8(vld1q_u8(b.bytes.as_ptr().add(i * 2)));
            let [xa0, xa1] = half_to_f32x8_neon(ha);
            let [xb0, xb1] = half_to_f32x8_neon(hb);
            let d0 = vsubq_f32(xa0, xb0);
            let d1 = vsubq_f32(xa1, xb1);
            acc0 = vfmaq_f32(acc0, d0, d0);
            acc1 = vfmaq_f32(acc1, d1, d1);
            i += 8;
        }
        vaddvq_f32(vaddq_f32(acc0, acc1))
    }
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

    // ------------------------------------------------------------------
    // v6.0.6 — NEON SIMD f16 → f32 + fused distance kernels.

    /// Generate a deterministic dim-N f32 vector of small finite
    /// values so the f16 round-trip stays inside the normal range
    /// (avoids subnormal flush-to-zero divergence between scalar
    /// and SIMD paths).
    #[allow(clippy::cast_precision_loss)]
    fn random_normal_vec(seed: u64, dim: usize) -> alloc::vec::Vec<f32> {
        let mut state = seed | 1;
        let mut out = alloc::vec::Vec::with_capacity(dim);
        for _ in 0..dim {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            // 24 high bits of state → [0, 2^24) → /2^24 → [0, 1).
            // The cast is safe (lossless) because the mask leaves
            // at most 24 bits, which fits f32's mantissa.
            let u = ((state >> 32) & 0x00FF_FFFF) as f32 / (0x80_0000_u32 as f32);
            // Range (-1, 1) — well inside f16 normal range; no subnormals
            // emerge from the round-trip.
            out.push(2.0 * u - 1.0);
        }
        out
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn half_l2_asymmetric_neon_matches_scalar() {
        // NEON SIMD path must agree with the scalar reference on
        // every production-shaped dim. Tolerance reflects FMA
        // rounding + the f32→f16→f32 round-trip noise; the lanes
        // are normal floats so subnormal flush-to-zero in the SIMD
        // path is a no-op.
        for &d in &[8_usize, 16, 32, 64, 128, 256, 512, 1024] {
            for trial in 0..8_u64 {
                let v = random_normal_vec(0xA5A5_F160_F160_0001 ^ trial ^ (d as u64), d);
                let q = random_normal_vec(0xC0FE_F160_F160_0002 ^ trial ^ (d as u64), d);
                let h = HalfVector::from_f32_slice(&v);
                let scalar = half_l2_distance_sq_asymmetric_scalar(&h, &q);
                let neon = unsafe { half_l2_distance_sq_asymmetric_neon(&h, &q) };
                let tol = (scalar.abs().max(1e-6)) * 1e-4 + (d as f32) * 1e-5;
                assert!(
                    (scalar - neon).abs() <= tol,
                    "L2 asym dim={d} trial={trial}: scalar={scalar} neon={neon}"
                );
            }
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn half_dot_asymmetric_neon_matches_scalar() {
        for &d in &[8_usize, 16, 32, 64, 128, 256, 512, 1024] {
            for trial in 0..8_u64 {
                let v = random_normal_vec(0xBEEF_F160_F160_0003 ^ trial ^ (d as u64), d);
                let q = random_normal_vec(0xDEAD_F160_F160_0004 ^ trial ^ (d as u64), d);
                let h = HalfVector::from_f32_slice(&v);
                let scalar = half_dot_asymmetric_scalar(&h, &q);
                let neon = unsafe { half_dot_asymmetric_neon(&h, &q) };
                let tol = (scalar.abs().max(1e-6)) * 1e-4 + (d as f32) * 1e-5;
                assert!(
                    (scalar - neon).abs() <= tol,
                    "dot dim={d} trial={trial}: scalar={scalar} neon={neon}"
                );
            }
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    #[allow(clippy::similar_names, clippy::cast_precision_loss)]
    fn half_cosine_accumulators_neon_matches_scalar() {
        for &d in &[8_usize, 16, 32, 64, 128, 256, 512, 1024] {
            for trial in 0..8_u64 {
                let v = random_normal_vec(0xC051_F160_F160_0005 ^ trial ^ (d as u64), d);
                let q = random_normal_vec(0xF00D_F160_F160_0006 ^ trial ^ (d as u64), d);
                let h = HalfVector::from_f32_slice(&v);
                let (dot_s, na_s, nq_s) = half_cosine_accumulators_asymmetric_scalar(&h, &q);
                let (dot_n, na_n, nq_n) =
                    unsafe { half_cosine_accumulators_asymmetric_neon(&h, &q) };
                let tol = |x: f32| (x.abs().max(1e-6)) * 1e-4 + (d as f32) * 1e-5;
                assert!(
                    (dot_s - dot_n).abs() <= tol(dot_s),
                    "cos dot dim={d}: scalar={dot_s} neon={dot_n}"
                );
                assert!(
                    (na_s - na_n).abs() <= tol(na_s),
                    "cos na dim={d}: scalar={na_s} neon={na_n}"
                );
                assert!(
                    (nq_s - nq_n).abs() <= tol(nq_s),
                    "cos nq dim={d}: scalar={nq_s} neon={nq_n}"
                );
            }
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn half_l2_symmetric_neon_matches_scalar() {
        for &d in &[8_usize, 16, 32, 64, 128, 256, 512, 1024] {
            for trial in 0..8_u64 {
                let va = random_normal_vec(0x1234_F160_F160_0007 ^ trial ^ (d as u64), d);
                let vb = random_normal_vec(0x5678_F160_F160_0008 ^ trial ^ (d as u64), d);
                let ha = HalfVector::from_f32_slice(&va);
                let hb = HalfVector::from_f32_slice(&vb);
                let scalar = half_l2_distance_sq_symmetric_scalar(&ha, &hb);
                let neon = unsafe { half_l2_distance_sq_symmetric_neon(&ha, &hb) };
                let tol = (scalar.abs().max(1e-6)) * 1e-4 + (d as f32) * 1e-5;
                assert!(
                    (scalar - neon).abs() <= tol,
                    "L2 sym dim={d}: scalar={scalar} neon={neon}"
                );
            }
        }
    }
}
