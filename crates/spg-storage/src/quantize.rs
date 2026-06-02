//! v6.0.0 — SQ8 scalar quantization for vector columns.
//!
//! Per-vector affine f32 → u8 quantization. Each `Sq8Vector` carries its
//! own `(min, max)` so quantization is purely streaming — no two-pass
//! corpus scan needed to learn global parameters. Trade-off: 8 bytes
//! overhead per vector (negligible at dim ≥ 64) in exchange for
//! INSERT-time simplicity.
//!
//! This file is the v6.0.0 standalone module: types + quantize /
//! dequantize + ADC distance + serde + recall oracle. Integration with
//! `DataType::Vector` + HNSW write path lands in v6.0.1.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

/// SQ8-quantized vector: every dimension stored as one byte plus a
/// shared (min, max) reconstruction frame.
///
/// Reconstruction: `x_i ≈ min + (byte_i / 255) * (max - min)`.
/// Quantization error bound (per element): `(max - min) / 510`
/// — half the quantization step `(max - min) / 255` thanks to
/// round-to-nearest.
#[derive(Debug, Clone, PartialEq)]
pub struct Sq8Vector {
    pub min: f32,
    pub max: f32,
    pub bytes: Vec<u8>,
}

impl Sq8Vector {
    /// Dimension of the original f32 vector (= byte count).
    #[must_use]
    pub fn dim(&self) -> usize {
        self.bytes.len()
    }
}

/// Minimum positive denominator used when `max == min`. Avoids
/// division-by-zero in `quantize`; reconstruction still returns
/// `min` for every element in that degenerate case.
const RANGE_FLOOR: f32 = 1e-12;

/// Quantize an f32 vector to SQ8 using per-vector affine mapping.
///
/// Empty input is allowed (returns an empty `Sq8Vector` with min=max=0).
/// Non-finite components (`NaN` / `±∞`) participate in the min/max scan
/// but are then clamped at quantize time — recall is undefined when the
/// caller passes them.
#[must_use]
pub fn quantize(v: &[f32]) -> Sq8Vector {
    if v.is_empty() {
        return Sq8Vector {
            min: 0.0,
            max: 0.0,
            bytes: Vec::new(),
        };
    }
    let mut min = v[0];
    let mut max = v[0];
    for &x in &v[1..] {
        if x < min {
            min = x;
        }
        if x > max {
            max = x;
        }
    }
    let range = max - min;
    let bytes: Vec<u8> = if range <= RANGE_FLOOR {
        vec![0u8; v.len()]
    } else {
        let scale = 255.0 / range;
        v.iter()
            .map(|&x| {
                let mapped = ((x - min) * scale) + 0.5;
                clamp_to_u8(mapped)
            })
            .collect()
    };
    Sq8Vector { min, max, bytes }
}

/// Reconstruct the approximate f32 vector. For a vector with
/// `max == min` (constant or single-element input) every component
/// reconstructs as `min` exactly.
#[must_use]
pub fn dequantize(q: &Sq8Vector) -> Vec<f32> {
    if q.bytes.is_empty() {
        return Vec::new();
    }
    let range = q.max - q.min;
    if range <= RANGE_FLOOR {
        return vec![q.min; q.bytes.len()];
    }
    let inv = range / 255.0;
    q.bytes
        .iter()
        .map(|&b| q.min + f32::from(b) * inv)
        .collect()
}

/// Saturating cast f32 → u8 with NaN-safe clamp. The mapped value
/// is in `[0, 255]` for well-formed input; clamping guards against
/// rounding edges and stray NaN.
#[inline]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "guarded by NaN check + (0.0, 255.0) range bracket above"
)]
fn clamp_to_u8(x: f32) -> u8 {
    if x.is_nan() {
        return 0;
    }
    if x <= 0.0 {
        0
    } else if x >= 255.0 {
        255
    } else {
        x as u8
    }
}

// ===========================================================================
// ADC (Asymmetric Distance Computation) over SQ8 vectors.
//
// "Symmetric" here means both operands are quantized; "asymmetric" means
// one operand is the un-quantized query vector. The asymmetric path is
// what kNN scans use: the query is parsed once into f32 then compared
// against many stored Sq8Vectors, so we save the quantization cost on
// the query side and gain a tiny precision bump.
//
// All four functions return values on the same scale as their f32 cousins
// in lib.rs (l2_distance_sq / cosine_distance / inner_product) so a planner
// can swap them in place.
// ===========================================================================

/// Symmetric L2² distance between two SQ8 vectors of equal dim.
/// Returns `f32::INFINITY` on dim mismatch (mirrors `vec_l2_sq`'s
/// behaviour in `lib.rs`).
#[must_use]
pub fn sq8_l2_distance_sq(a: &Sq8Vector, b: &Sq8Vector) -> f32 {
    if a.bytes.len() != b.bytes.len() {
        return f32::INFINITY;
    }
    let inv_a = sq8_step(a);
    let inv_b = sq8_step(b);
    let mut acc: f32 = 0.0;
    for (&ba, &bb) in a.bytes.iter().zip(b.bytes.iter()) {
        let xa = a.min + f32::from(ba) * inv_a;
        let xb = b.min + f32::from(bb) * inv_b;
        let d = xa - xb;
        acc += d * d;
    }
    acc
}

/// Asymmetric L2² between a stored SQ8 vector and an un-quantized
/// query vector. Same semantics as `vec_l2_sq` for the kNN scan
/// case (one query, many vectors).
///
/// v6.0.2: aarch64 NEON path for `dim >= 16 && dim % 16 == 0` —
/// covers every production-shaped embedding (64, 128, 256, ...).
/// Other shapes fall back to the scalar loop.
#[must_use]
pub fn sq8_l2_distance_sq_asymmetric(a: &Sq8Vector, q: &[f32]) -> f32 {
    if a.bytes.len() != q.len() {
        return f32::INFINITY;
    }
    #[cfg(target_arch = "aarch64")]
    {
        let n = a.bytes.len();
        if n >= 16 && n.is_multiple_of(16) {
            // SAFETY: NEON is baseline aarch64; preconditions (matching
            // lengths, ≥ 1 full 16-byte lane group) checked above.
            return unsafe { sq8_l2_distance_sq_asymmetric_neon(a, q) };
        }
    }
    sq8_l2_distance_sq_asymmetric_scalar(a, q)
}

fn sq8_l2_distance_sq_asymmetric_scalar(a: &Sq8Vector, q: &[f32]) -> f32 {
    let inv_a = sq8_step(a);
    let mut acc: f32 = 0.0;
    for (&ba, &qx) in a.bytes.iter().zip(q.iter()) {
        let xa = a.min + f32::from(ba) * inv_a;
        let d = xa - qx;
        acc += d * d;
    }
    acc
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[allow(clippy::many_single_char_names)] // NEON intrinsics work in single-letter regs by convention
unsafe fn sq8_l2_distance_sq_asymmetric_neon(a: &Sq8Vector, q: &[f32]) -> f32 {
    use core::arch::aarch64::{
        float32x4_t, vaddq_f32, vaddvq_f32, vcvtq_f32_u32, vdupq_n_f32, vfmaq_f32, vget_high_u16,
        vget_low_u16, vld1_u8, vld1q_f32, vmovl_u8, vmovl_u16, vsubq_f32,
    };
    unsafe {
        let step = vdupq_n_f32(sq8_step(a));
        let bias = vdupq_n_f32(a.min);
        let zero: float32x4_t = vdupq_n_f32(0.0);
        let mut acc0 = zero;
        let mut acc1 = zero;
        let n = a.bytes.len();
        let mut i = 0usize;
        while i + 16 <= n {
            // Two 8-byte loads cover one 16-byte chunk of a.bytes.
            // Widening u8 → u16 → u32 → f32 stays portable to every
            // ARMv8.0+ NEON host (no FEAT_DotProd dependency).
            let lo8 = vld1_u8(a.bytes.as_ptr().add(i));
            let hi8 = vld1_u8(a.bytes.as_ptr().add(i + 8));
            let lo16 = vmovl_u8(lo8); // u8x8 → u16x8
            let hi16 = vmovl_u8(hi8);
            let xa0 = vfmaq_f32(bias, step, vcvtq_f32_u32(vmovl_u16(vget_low_u16(lo16))));
            let xa1 = vfmaq_f32(bias, step, vcvtq_f32_u32(vmovl_u16(vget_high_u16(lo16))));
            let xa2 = vfmaq_f32(bias, step, vcvtq_f32_u32(vmovl_u16(vget_low_u16(hi16))));
            let xa3 = vfmaq_f32(bias, step, vcvtq_f32_u32(vmovl_u16(vget_high_u16(hi16))));
            let q0 = vld1q_f32(q.as_ptr().add(i));
            let q1 = vld1q_f32(q.as_ptr().add(i + 4));
            let q2 = vld1q_f32(q.as_ptr().add(i + 8));
            let q3 = vld1q_f32(q.as_ptr().add(i + 12));
            let d0 = vsubq_f32(xa0, q0);
            let d1 = vsubq_f32(xa1, q1);
            let d2 = vsubq_f32(xa2, q2);
            let d3 = vsubq_f32(xa3, q3);
            acc0 = vfmaq_f32(acc0, d0, d0);
            acc1 = vfmaq_f32(acc1, d1, d1);
            acc0 = vfmaq_f32(acc0, d2, d2);
            acc1 = vfmaq_f32(acc1, d3, d3);
            i += 16;
        }
        vaddvq_f32(vaddq_f32(acc0, acc1))
    }
}

/// Symmetric inner product, returned **negated** so smaller = closer
/// (matches pgvector `<#>` and SPG's `NswMetric::InnerProduct`).
#[must_use]
pub fn sq8_inner_product(a: &Sq8Vector, b: &Sq8Vector) -> f32 {
    if a.bytes.len() != b.bytes.len() {
        return f32::INFINITY;
    }
    let inv_a = sq8_step(a);
    let inv_b = sq8_step(b);
    let mut dot: f32 = 0.0;
    for (&ba, &bb) in a.bytes.iter().zip(b.bytes.iter()) {
        let xa = a.min + f32::from(ba) * inv_a;
        let xb = b.min + f32::from(bb) * inv_b;
        dot += xa * xb;
    }
    -dot
}

/// Asymmetric inner product (negated). v6.0.2: aarch64 NEON path
/// under the same `dim >= 16 && dim % 16 == 0` pre-condition as the
/// L2 asymmetric variant.
#[must_use]
pub fn sq8_inner_product_asymmetric(a: &Sq8Vector, q: &[f32]) -> f32 {
    if a.bytes.len() != q.len() {
        return f32::INFINITY;
    }
    #[cfg(target_arch = "aarch64")]
    {
        let n = a.bytes.len();
        if n >= 16 && n.is_multiple_of(16) {
            // SAFETY: see `sq8_l2_distance_sq_asymmetric_neon`.
            return -unsafe { sq8_dot_asymmetric_neon(a, q) };
        }
    }
    -sq8_dot_asymmetric_scalar(a, q)
}

fn sq8_dot_asymmetric_scalar(a: &Sq8Vector, q: &[f32]) -> f32 {
    let inv_a = sq8_step(a);
    let mut dot: f32 = 0.0;
    for (&ba, &qx) in a.bytes.iter().zip(q.iter()) {
        let xa = a.min + f32::from(ba) * inv_a;
        dot += xa * qx;
    }
    dot
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[allow(clippy::many_single_char_names)]
unsafe fn sq8_dot_asymmetric_neon(a: &Sq8Vector, q: &[f32]) -> f32 {
    use core::arch::aarch64::{
        float32x4_t, vaddq_f32, vaddvq_f32, vcvtq_f32_u32, vdupq_n_f32, vfmaq_f32, vget_high_u16,
        vget_low_u16, vld1_u8, vld1q_f32, vmovl_u8, vmovl_u16,
    };
    unsafe {
        let step = vdupq_n_f32(sq8_step(a));
        let bias = vdupq_n_f32(a.min);
        let zero: float32x4_t = vdupq_n_f32(0.0);
        let mut acc0 = zero;
        let mut acc1 = zero;
        let n = a.bytes.len();
        let mut i = 0usize;
        while i + 16 <= n {
            let lo8 = vld1_u8(a.bytes.as_ptr().add(i));
            let hi8 = vld1_u8(a.bytes.as_ptr().add(i + 8));
            let lo16 = vmovl_u8(lo8);
            let hi16 = vmovl_u8(hi8);
            let xa0 = vfmaq_f32(bias, step, vcvtq_f32_u32(vmovl_u16(vget_low_u16(lo16))));
            let xa1 = vfmaq_f32(bias, step, vcvtq_f32_u32(vmovl_u16(vget_high_u16(lo16))));
            let xa2 = vfmaq_f32(bias, step, vcvtq_f32_u32(vmovl_u16(vget_low_u16(hi16))));
            let xa3 = vfmaq_f32(bias, step, vcvtq_f32_u32(vmovl_u16(vget_high_u16(hi16))));
            acc0 = vfmaq_f32(acc0, xa0, vld1q_f32(q.as_ptr().add(i)));
            acc1 = vfmaq_f32(acc1, xa1, vld1q_f32(q.as_ptr().add(i + 4)));
            acc0 = vfmaq_f32(acc0, xa2, vld1q_f32(q.as_ptr().add(i + 8)));
            acc1 = vfmaq_f32(acc1, xa3, vld1q_f32(q.as_ptr().add(i + 12)));
            i += 16;
        }
        vaddvq_f32(vaddq_f32(acc0, acc1))
    }
}

/// Symmetric cosine distance `1 - dot / (||a|| ||b||)`. Zero-norm
/// operand yields `f32::INFINITY` so it sorts last (matches the
/// f32 `cosine_distance` in `eval.rs`).
#[must_use]
pub fn sq8_cosine_distance(a: &Sq8Vector, b: &Sq8Vector) -> f32 {
    if a.bytes.len() != b.bytes.len() {
        return f32::INFINITY;
    }
    let inv_a = sq8_step(a);
    let inv_b = sq8_step(b);
    let (mut dot, mut na, mut nb) = (0.0_f32, 0.0_f32, 0.0_f32);
    for (&ba, &bb) in a.bytes.iter().zip(b.bytes.iter()) {
        let xa = a.min + f32::from(ba) * inv_a;
        let xb = b.min + f32::from(bb) * inv_b;
        dot += xa * xb;
        na += xa * xa;
        nb += xb * xb;
    }
    if na == 0.0 || nb == 0.0 {
        return f32::INFINITY;
    }
    1.0 - dot / (sqrt_finite(na) * sqrt_finite(nb))
}

/// Asymmetric cosine distance against an un-quantized query. v6.0.2:
/// aarch64 NEON path for the three accumulators; norm-sqrt + zero-
/// guard stays in this safe wrapper.
#[must_use]
pub fn sq8_cosine_distance_asymmetric(a: &Sq8Vector, q: &[f32]) -> f32 {
    if a.bytes.len() != q.len() {
        return f32::INFINITY;
    }
    let (dot, na, nq);
    #[cfg(target_arch = "aarch64")]
    {
        let n = a.bytes.len();
        if n >= 16 && n.is_multiple_of(16) {
            // SAFETY: see `sq8_l2_distance_sq_asymmetric_neon`.
            let (d, a2, q2) = unsafe { sq8_cosine_accumulators_asymmetric_neon(a, q) };
            dot = d;
            na = a2;
            nq = q2;
        } else {
            let (d, a2, q2) = sq8_cosine_accumulators_asymmetric_scalar(a, q);
            dot = d;
            na = a2;
            nq = q2;
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let (d, a2, q2) = sq8_cosine_accumulators_asymmetric_scalar(a, q);
        dot = d;
        na = a2;
        nq = q2;
    }
    if na == 0.0 || nq == 0.0 {
        return f32::INFINITY;
    }
    1.0 - dot / (sqrt_finite(na) * sqrt_finite(nq))
}

fn sq8_cosine_accumulators_asymmetric_scalar(a: &Sq8Vector, q: &[f32]) -> (f32, f32, f32) {
    let inv_a = sq8_step(a);
    let (mut dot, mut na, mut nq) = (0.0_f32, 0.0_f32, 0.0_f32);
    for (&ba, &qx) in a.bytes.iter().zip(q.iter()) {
        let xa = a.min + f32::from(ba) * inv_a;
        dot += xa * qx;
        na += xa * xa;
        nq += qx * qx;
    }
    (dot, na, nq)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[allow(clippy::many_single_char_names, clippy::similar_names)]
unsafe fn sq8_cosine_accumulators_asymmetric_neon(a: &Sq8Vector, q: &[f32]) -> (f32, f32, f32) {
    use core::arch::aarch64::{
        float32x4_t, vaddvq_f32, vcvtq_f32_u32, vdupq_n_f32, vfmaq_f32, vget_high_u16,
        vget_low_u16, vld1_u8, vld1q_f32, vmovl_u8, vmovl_u16,
    };
    unsafe {
        let step = vdupq_n_f32(sq8_step(a));
        let bias = vdupq_n_f32(a.min);
        let zero: float32x4_t = vdupq_n_f32(0.0);
        let mut acc_dot = zero;
        let mut acc_na = zero;
        let mut acc_nq = zero;
        let n = a.bytes.len();
        let mut i = 0usize;
        while i + 16 <= n {
            let lo8 = vld1_u8(a.bytes.as_ptr().add(i));
            let hi8 = vld1_u8(a.bytes.as_ptr().add(i + 8));
            let lo16 = vmovl_u8(lo8);
            let hi16 = vmovl_u8(hi8);
            let xs = [
                vfmaq_f32(bias, step, vcvtq_f32_u32(vmovl_u16(vget_low_u16(lo16)))),
                vfmaq_f32(bias, step, vcvtq_f32_u32(vmovl_u16(vget_high_u16(lo16)))),
                vfmaq_f32(bias, step, vcvtq_f32_u32(vmovl_u16(vget_low_u16(hi16)))),
                vfmaq_f32(bias, step, vcvtq_f32_u32(vmovl_u16(vget_high_u16(hi16)))),
            ];
            let qs = [
                vld1q_f32(q.as_ptr().add(i)),
                vld1q_f32(q.as_ptr().add(i + 4)),
                vld1q_f32(q.as_ptr().add(i + 8)),
                vld1q_f32(q.as_ptr().add(i + 12)),
            ];
            for k in 0..4 {
                acc_dot = vfmaq_f32(acc_dot, xs[k], qs[k]);
                acc_na = vfmaq_f32(acc_na, xs[k], xs[k]);
                acc_nq = vfmaq_f32(acc_nq, qs[k], qs[k]);
            }
            i += 16;
        }
        (vaddvq_f32(acc_dot), vaddvq_f32(acc_na), vaddvq_f32(acc_nq))
    }
}

/// Reconstruction step `(max - min) / 255`; saturates to 0 on
/// degenerate (constant) vectors so the multiply collapses to
/// `min` for every element.
#[inline]
fn sq8_step(q: &Sq8Vector) -> f32 {
    let range = q.max - q.min;
    if range <= RANGE_FLOOR {
        0.0
    } else {
        range / 255.0
    }
}

/// `f32::sqrt` lives in `std`. `no_std` reimpl via 6 Newton-Raphson
/// iterations from a `(x + 1) / 2` seed — converges to ULP for
/// `x ∈ (0, 1e6)`, matches `eval.rs`'s pattern.
#[inline]
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

#[cfg(test)]
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::useless_conversion,
    clippy::similar_names,
    clippy::unreadable_literal,
    clippy::items_after_statements,
    clippy::too_many_lines,
    clippy::float_cmp,
    clippy::suboptimal_flops,
    clippy::cast_possible_wrap
)]
mod tests {
    use super::*;

    /// Deterministic PRNG so test corpora are reproducible across runs
    /// and platforms. Algorithm: SplitMix64 (Vigna). Pure u64
    /// arithmetic — no `std`, no `rand` crate.
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

        /// Uniform f32 in [0, 1).
        fn next_unit_f32(&mut self) -> f32 {
            // 24 high bits → [0, 2^24) → /2^24 → [0, 1).
            let bits = (self.next_u64() >> 40) as u32;
            (bits as f32) / ((1u32 << 24) as f32)
        }

        /// Box-Muller transform → standard normal f32.
        fn next_gaussian_f32(&mut self) -> f32 {
            // Avoid log(0) by lifting u from [0, 1) to (0, 1].
            let u = 1.0 - self.next_unit_f32();
            let v = self.next_unit_f32();
            let r = sqrt_f32(-2.0 * ln_f32(u));
            let theta = 2.0 * core::f32::consts::PI * v;
            r * cos_f32(theta)
        }
    }

    /// Newton-Raphson square root for f32 (no `std::f32::sqrt` in no_std).
    /// Five iterations from a `(x + 1) / 2` seed converge to ULP for
    /// `x ∈ (0, 1e6)` — ample for unit-test vectors.
    fn sqrt_f32(x: f32) -> f32 {
        if x <= 0.0 {
            return 0.0;
        }
        let mut y = if x >= 1.0 { x * 0.5 } else { (x + 1.0) * 0.5 };
        for _ in 0..6 {
            y = 0.5 * (y + x / y);
        }
        y
    }

    /// Natural log via the identity `ln(x) = 2 atanh((x-1)/(x+1))`
    /// — converges quickly for `x ∈ (0, 4)`, which covers the
    /// Box-Muller `1 - unit` input range.
    fn ln_f32(x: f32) -> f32 {
        if x <= 0.0 {
            return f32::NEG_INFINITY;
        }
        // Range-reduce: x = 2^k * m where m ∈ [0.5, 1.0).
        let mut k: i32 = 0;
        let mut m = x;
        while m >= 1.0 {
            m *= 0.5;
            k += 1;
        }
        while m < 0.5 {
            m *= 2.0;
            k -= 1;
        }
        // atanh series on (m-1)/(m+1).
        let u = (m - 1.0) / (m + 1.0);
        let u2 = u * u;
        let mut term = u;
        let mut sum = 0.0;
        for i in 0..16 {
            sum += term / ((2 * i + 1) as f32);
            term *= u2;
        }
        2.0 * sum + (k as f32) * core::f32::consts::LN_2
    }

    /// Cosine via 5-term Taylor on the reduced argument `theta mod 2π`.
    /// Accurate to ~1e-5 — fine for Box-Muller generating Gaussians
    /// whose tail behaviour we never inspect at sub-ULP precision.
    fn cos_f32(theta: f32) -> f32 {
        let two_pi = 2.0 * core::f32::consts::PI;
        let mut t = theta % two_pi;
        if t > core::f32::consts::PI {
            t -= two_pi;
        } else if t < -core::f32::consts::PI {
            t += two_pi;
        }
        let t2 = t * t;
        // 1 - t²/2! + t⁴/4! - t⁶/6! + t⁸/8! - t¹⁰/10!
        1.0 - t2 / 2.0 + t2 * t2 / 24.0 - t2 * t2 * t2 / 720.0 + t2 * t2 * t2 * t2 / 40_320.0
            - t2 * t2 * t2 * t2 * t2 / 3_628_800.0
    }

    fn random_gaussian_vec(rng: &mut SplitMix64, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| rng.next_gaussian_f32()).collect()
    }

    fn random_unit_vec(rng: &mut SplitMix64, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| rng.next_unit_f32() * 2.0 - 1.0).collect()
    }

    fn linf_error(a: &[f32], b: &[f32]) -> f32 {
        let mut e: f32 = 0.0;
        for (x, y) in a.iter().zip(b.iter()) {
            let d = (x - y).abs();
            if d > e {
                e = d;
            }
        }
        e
    }

    #[test]
    fn quantize_empty_vector_is_zero_dim() {
        let q = quantize(&[]);
        assert_eq!(q.dim(), 0);
        assert_eq!(q.min, 0.0);
        assert_eq!(q.max, 0.0);
        assert!(dequantize(&q).is_empty());
    }

    #[test]
    fn quantize_single_element_roundtrips_exactly() {
        let q = quantize(&[3.25]);
        assert_eq!(q.dim(), 1);
        assert_eq!(q.min, 3.25);
        assert_eq!(q.max, 3.25);
        let d = dequantize(&q);
        assert_eq!(d.len(), 1);
        // Single element → range floor → reconstructs as min exactly.
        assert!((d[0] - 3.25).abs() < 1e-6);
    }

    #[test]
    fn quantize_constant_vector_roundtrips_exactly() {
        let v = vec![7.5_f32; 64];
        let q = quantize(&v);
        assert_eq!(q.min, 7.5);
        assert_eq!(q.max, 7.5);
        let d = dequantize(&q);
        for x in &d {
            assert!((x - 7.5).abs() < 1e-6);
        }
    }

    #[test]
    fn quantize_min_and_max_endpoints_reconstruct_exactly() {
        let v = vec![-2.0_f32, 0.0, 5.0, 3.0, -2.0, 5.0];
        let q = quantize(&v);
        assert_eq!(q.min, -2.0);
        assert_eq!(q.max, 5.0);
        let d = dequantize(&q);
        // -2.0 (min) maps to byte 0; 5.0 (max) maps to byte 255 →
        // both reconstruct exactly.
        assert!((d[0] - (-2.0)).abs() < 1e-5);
        assert!((d[2] - 5.0).abs() < 1e-5);
        assert!((d[4] - (-2.0)).abs() < 1e-5);
        assert!((d[5] - 5.0).abs() < 1e-5);
    }

    #[test]
    fn quantize_dequantize_roundtrip_bounded_error_gaussian() {
        let mut rng = SplitMix64::new(0xDEAD_BEEF_CAFE_F00D);
        for dim in [32_usize, 128, 512, 1024] {
            for _trial in 0..250 {
                let v = random_gaussian_vec(&mut rng, dim);
                let q = quantize(&v);
                let r = dequantize(&q);
                // Theoretical bound: |x - r| ≤ (max - min) / 510
                // (half the step size, round-to-nearest). Add a tiny
                // float slack for the f32 mul+add in reconstruction.
                let step = (q.max - q.min) / 510.0;
                let bound = step + 1e-6_f32.max(step * 1e-3);
                let err = linf_error(&v, &r);
                assert!(
                    err <= bound,
                    "dim={dim} err={err} bound={bound} range={}",
                    q.max - q.min
                );
            }
        }
    }

    // ----- helpers for distance reference comparisons -----

    fn l2_sq_f32(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b.iter()).map(|(x, y)| (x - y).powi(2)).sum()
    }

    fn inner_product_f32(a: &[f32], b: &[f32]) -> f32 {
        -a.iter().zip(b.iter()).map(|(x, y)| x * y).sum::<f32>()
    }

    fn cosine_distance_f32(a: &[f32], b: &[f32]) -> f32 {
        let (mut dot, mut na, mut nb) = (0.0_f32, 0.0_f32, 0.0_f32);
        for (x, y) in a.iter().zip(b.iter()) {
            dot += x * y;
            na += x * x;
            nb += y * y;
        }
        if na == 0.0 || nb == 0.0 {
            return f32::INFINITY;
        }
        1.0 - dot / (sqrt_f32(na) * sqrt_f32(nb))
    }

    // Operational-correctness tests: the SQ8 distance is *defined* as
    // "compute on the dequantized values" — so it must match the f32
    // distance applied to dequantize(q) within float-arithmetic
    // tolerance. Semantic preservation vs the *original* f32 vector
    // is what `sq8_recall_at_10_*` covers (the right metric for
    // quantization drift since ranking is what kNN actually cares
    // about, not absolute distance equality).

    fn float_tolerance_for_dim(dim: usize) -> f32 {
        // Each fused multiply-add contributes ~1 ULP; sum over dim.
        // 1e-4 × dim gives ample headroom over the f32 ε.
        1e-4 * dim as f32
    }

    #[test]
    fn sq8_l2_distance_matches_dequantize_then_f32() {
        let mut rng = SplitMix64::new(0xABCD_0001_2345_6789);
        for dim in [32_usize, 128, 512, 1024] {
            let tol = float_tolerance_for_dim(dim);
            for _ in 0..2500 {
                let a = random_gaussian_vec(&mut rng, dim);
                let b = random_gaussian_vec(&mut rng, dim);
                let qa = quantize(&a);
                let qb = quantize(&b);
                let dqa = dequantize(&qa);
                let dqb = dequantize(&qb);
                let want_sym = l2_sq_f32(&dqa, &dqb);
                let want_asym = l2_sq_f32(&dqa, &b);
                let got_sym = sq8_l2_distance_sq(&qa, &qb);
                let got_asym = sq8_l2_distance_sq_asymmetric(&qa, &b);
                let err_sym = (got_sym - want_sym).abs();
                let err_asym = (got_asym - want_asym).abs();
                let scale = want_sym.abs().max(want_asym.abs()).max(1.0);
                assert!(
                    err_sym <= tol * scale,
                    "dim={dim} sym got={got_sym} want={want_sym} err={err_sym} tol={}",
                    tol * scale
                );
                assert!(
                    err_asym <= tol * scale,
                    "dim={dim} asym got={got_asym} want={want_asym} err={err_asym} tol={}",
                    tol * scale
                );
            }
        }
    }

    #[test]
    fn sq8_inner_product_matches_dequantize_then_f32() {
        let mut rng = SplitMix64::new(0xABCD_0002_2345_6789);
        for dim in [32_usize, 128, 512, 1024] {
            let tol = float_tolerance_for_dim(dim);
            for _ in 0..2500 {
                let a = random_gaussian_vec(&mut rng, dim);
                let b = random_gaussian_vec(&mut rng, dim);
                let qa = quantize(&a);
                let qb = quantize(&b);
                let dqa = dequantize(&qa);
                let dqb = dequantize(&qb);
                let want_sym = inner_product_f32(&dqa, &dqb);
                let want_asym = inner_product_f32(&dqa, &b);
                let got_sym = sq8_inner_product(&qa, &qb);
                let got_asym = sq8_inner_product_asymmetric(&qa, &b);
                let scale = want_sym.abs().max(want_asym.abs()).max(1.0);
                let err_sym = (got_sym - want_sym).abs();
                let err_asym = (got_asym - want_asym).abs();
                assert!(
                    err_sym <= tol * scale,
                    "dim={dim} sym got={got_sym} want={want_sym} err={err_sym}"
                );
                assert!(
                    err_asym <= tol * scale,
                    "dim={dim} asym got={got_asym} want={want_asym} err={err_asym}"
                );
            }
        }
    }

    #[test]
    fn sq8_cosine_distance_matches_dequantize_then_f32() {
        let mut rng = SplitMix64::new(0xABCD_0003_2345_6789);
        for dim in [32_usize, 128, 512, 1024] {
            let tol = float_tolerance_for_dim(dim);
            for _ in 0..2500 {
                let a = random_gaussian_vec(&mut rng, dim);
                let b = random_gaussian_vec(&mut rng, dim);
                let qa = quantize(&a);
                let qb = quantize(&b);
                let dqa = dequantize(&qa);
                let dqb = dequantize(&qb);
                let want_sym = cosine_distance_f32(&dqa, &dqb);
                let want_asym = cosine_distance_f32(&dqa, &b);
                let got_sym = sq8_cosine_distance(&qa, &qb);
                let got_asym = sq8_cosine_distance_asymmetric(&qa, &b);
                // Cosine ∈ [0, 2]; absolute tolerance scaled by dim
                // since norms accumulate dim FMAs.
                let bound = tol;
                assert!(
                    (got_sym - want_sym).abs() <= bound,
                    "dim={dim} sym got={got_sym} want={want_sym}"
                );
                assert!(
                    (got_asym - want_asym).abs() <= bound,
                    "dim={dim} asym got={got_asym} want={want_asym}"
                );
            }
        }
    }

    #[test]
    fn sq8_distance_handles_dim_mismatch_with_infinity() {
        let a = quantize(&[1.0, 2.0, 3.0]);
        let b = quantize(&[1.0, 2.0]);
        assert_eq!(sq8_l2_distance_sq(&a, &b), f32::INFINITY);
        assert_eq!(sq8_inner_product(&a, &b), f32::INFINITY);
        assert_eq!(sq8_cosine_distance(&a, &b), f32::INFINITY);
        assert_eq!(sq8_l2_distance_sq_asymmetric(&a, &[1.0]), f32::INFINITY);
    }

    #[test]
    fn sq8_cosine_handles_zero_norm_with_infinity() {
        let zero = quantize(&[0.0_f32; 8]);
        let nonzero = quantize(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(sq8_cosine_distance(&zero, &nonzero), f32::INFINITY);
        assert_eq!(
            sq8_cosine_distance_asymmetric(&zero, &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            f32::INFINITY
        );
    }

    #[test]
    fn quantize_dequantize_roundtrip_bounded_error_uniform() {
        let mut rng = SplitMix64::new(0xF0F0_F0F0_F0F0_F0F0);
        for dim in [32_usize, 128, 512, 1024] {
            for _trial in 0..250 {
                let v = random_unit_vec(&mut rng, dim);
                let q = quantize(&v);
                let r = dequantize(&q);
                let step = (q.max - q.min) / 510.0;
                let bound = step + 1e-6_f32.max(step * 1e-3);
                let err = linf_error(&v, &r);
                assert!(
                    err <= bound,
                    "dim={dim} err={err} bound={bound} range={}",
                    q.max - q.min
                );
            }
        }
    }

    // ----- recall@10 oracle: SQ8-ranked top-10 must overlap ≥ 95% with
    // the f32 ground truth. This is the ranking-preservation property
    // that kNN actually depends on, vs the distance-magnitude property
    // covered by the dequantize-then-f32 tests above.

    fn topk_indices_l2(corpus: &[Vec<f32>], query: &[f32], k: usize) -> Vec<usize> {
        let mut scored: Vec<(f32, usize)> = corpus
            .iter()
            .enumerate()
            .map(|(i, v)| (l2_sq_f32(v, query), i))
            .collect();
        scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
        scored.into_iter().take(k).map(|(_, i)| i).collect()
    }

    fn topk_indices_l2_sq8_asym(corpus: &[Sq8Vector], query: &[f32], k: usize) -> Vec<usize> {
        let mut scored: Vec<(f32, usize)> = corpus
            .iter()
            .enumerate()
            .map(|(i, qv)| (sq8_l2_distance_sq_asymmetric(qv, query), i))
            .collect();
        scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
        scored.into_iter().take(k).map(|(_, i)| i).collect()
    }

    fn overlap_fraction(a: &[usize], b: &[usize]) -> f32 {
        let mut hits = 0;
        for &x in a {
            if b.contains(&x) {
                hits += 1;
            }
        }
        hits as f32 / a.len() as f32
    }

    #[test]
    fn sq8_recall_at_10_above_0_95_gaussian() {
        const N: usize = 10_000;
        const Q: usize = 100;
        const K: usize = 10;
        const DIM: usize = 128;

        let mut rng = SplitMix64::new(0x5EED_5EED_5EED_5EED);
        let corpus_f32: Vec<Vec<f32>> =
            (0..N).map(|_| random_gaussian_vec(&mut rng, DIM)).collect();
        let corpus_sq8: Vec<Sq8Vector> = corpus_f32.iter().map(|v| quantize(v)).collect();

        let mut total_recall: f32 = 0.0;
        for _ in 0..Q {
            let query = random_gaussian_vec(&mut rng, DIM);
            let truth = topk_indices_l2(&corpus_f32, &query, K);
            let sq8_top = topk_indices_l2_sq8_asym(&corpus_sq8, &query, K);
            total_recall += overlap_fraction(&truth, &sq8_top);
        }
        let avg = total_recall / Q as f32;
        assert!(
            avg >= 0.95,
            "Gaussian recall@10 average = {avg} (need ≥ 0.95)"
        );
    }

    #[test]
    fn sq8_recall_at_10_above_0_93_uniform_unit_sphere() {
        const N: usize = 10_000;
        const Q: usize = 100;
        const K: usize = 10;
        const DIM: usize = 128;

        let mut rng = SplitMix64::new(0xC0DE_C0DE_C0DE_C0DE);
        // Unit-sphere uniform via Gaussian-then-normalise (Müller method).
        let normalise = |mut v: Vec<f32>| -> Vec<f32> {
            let n = sqrt_f32(v.iter().map(|x| x * x).sum::<f32>()).max(1e-12);
            for x in &mut v {
                *x /= n;
            }
            v
        };
        let corpus_f32: Vec<Vec<f32>> = (0..N)
            .map(|_| normalise(random_gaussian_vec(&mut rng, DIM)))
            .collect();
        let corpus_sq8: Vec<Sq8Vector> = corpus_f32.iter().map(|v| quantize(v)).collect();

        let mut total_recall: f32 = 0.0;
        for _ in 0..Q {
            let query = normalise(random_gaussian_vec(&mut rng, DIM));
            let truth = topk_indices_l2(&corpus_f32, &query, K);
            let sq8_top = topk_indices_l2_sq8_asym(&corpus_sq8, &query, K);
            total_recall += overlap_fraction(&truth, &sq8_top);
        }
        let avg = total_recall / Q as f32;
        assert!(
            avg >= 0.93,
            "Unit-sphere recall@10 average = {avg} (need ≥ 0.93)"
        );
    }

    // ----- serde roundtrip -----

    #[test]
    fn sq8_serde_roundtrip_preserves_all_fields() {
        let mut rng = SplitMix64::new(0xBEEF_F00D_DEAD_0123);
        for dim in [0_usize, 1, 7, 32, 128, 1024] {
            for _ in 0..200 {
                let v = random_gaussian_vec(&mut rng, dim);
                let q = quantize(&v);
                let bytes = q.to_bytes();
                assert_eq!(bytes.len(), Sq8Vector::encoded_size_for(dim));
                let back = Sq8Vector::from_bytes(&bytes).expect("from_bytes");
                assert_eq!(back, q, "dim={dim} roundtrip mismatch");
            }
        }
    }

    #[test]
    fn sq8_from_bytes_rejects_truncated_header() {
        for short in [0_usize, 1, 4, 8, 11] {
            let buf = vec![0u8; short];
            assert_eq!(Sq8Vector::from_bytes(&buf), Err(QuantizeError::Truncated));
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn sq8_adc_ip_asymmetric_neon_matches_scalar() {
        // v6.0.2 step 3 verify: NEON inner-product asymmetric ADC.
        // Returned value is `-dot`; we compare against the scalar
        // shape of the same.
        let dims = [16usize, 32, 64, 128, 256, 512, 1024];
        for &d in &dims {
            let mut rng = SplitMix64::new(0xBEEF_DEAD_1234_A5A5u64 ^ d as u64);
            for _ in 0..16 {
                let v = random_gaussian_vec(&mut rng, d);
                let q = random_gaussian_vec(&mut rng, d);
                let sq = quantize(&v);
                let scalar = -sq8_dot_asymmetric_scalar(&sq, &q);
                let neon = -unsafe { sq8_dot_asymmetric_neon(&sq, &q) };
                let tol = (scalar.abs().max(1e-6)) * 1e-4 + (d as f32) * 1e-5;
                assert!(
                    (scalar - neon).abs() <= tol,
                    "IP asym dim={d}: scalar={scalar} neon={neon} diff={}",
                    (scalar - neon).abs()
                );
            }
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn sq8_adc_cosine_asymmetric_neon_matches_scalar() {
        // v6.0.2 step 3 verify: cosine accumulators agree across
        // scalar / NEON; the safe wrapper handles norm-sqrt + zero
        // guard the same way for both paths.
        let dims = [16usize, 32, 64, 128, 256, 512, 1024];
        for &d in &dims {
            let mut rng = SplitMix64::new(0xC0DE_F00D_1234_5678u64 ^ d as u64);
            for _ in 0..16 {
                let v = random_gaussian_vec(&mut rng, d);
                let q = random_gaussian_vec(&mut rng, d);
                let sq = quantize(&v);
                let (dot_s, na_s, nq_s) = sq8_cosine_accumulators_asymmetric_scalar(&sq, &q);
                let (dot_n, na_n, nq_n) =
                    unsafe { sq8_cosine_accumulators_asymmetric_neon(&sq, &q) };
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
    fn sq8_adc_l2_asymmetric_neon_matches_scalar() {
        // v6.0.2 step 2 verify: NEON L2 asymmetric ADC must agree
        // with the scalar reference across every production-shaped
        // dim. Tolerance scales with `dim`: FMA rounding + the
        // dequantisation step's intermediate widening can drift one
        // ulp per term, so a scalar / NEON spread of dim * 1e-6 is
        // expected at dim 1024.
        let dims = [16usize, 32, 48, 64, 128, 256, 512, 1024];
        for &d in &dims {
            let mut rng = SplitMix64::new(0xA5A5_1234_DEAD_BEEFu64 ^ d as u64);
            for _ in 0..16 {
                let v = random_gaussian_vec(&mut rng, d);
                let q = random_gaussian_vec(&mut rng, d);
                let sq = quantize(&v);
                let scalar = sq8_l2_distance_sq_asymmetric_scalar(&sq, &q);
                let neon = unsafe { sq8_l2_distance_sq_asymmetric_neon(&sq, &q) };
                let tol = (scalar.abs().max(1e-6)) * 1e-4 + (d as f32) * 1e-5;
                assert!(
                    (scalar - neon).abs() <= tol,
                    "L2 asym dim={d}: scalar={scalar} neon={neon} diff={}",
                    (scalar - neon).abs()
                );
            }
        }
    }

    #[test]
    fn sq8_from_bytes_rejects_dim_mismatch() {
        // Header declares dim=4 but body only has 2 bytes.
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(&0.0f32.to_le_bytes());
        buf.extend_from_slice(&1.0f32.to_le_bytes());
        buf.extend_from_slice(&[10u8, 200u8]);
        assert_eq!(
            Sq8Vector::from_bytes(&buf),
            Err(QuantizeError::DimMismatch {
                expected: 4,
                got: 2
            })
        );
    }
}

/// Error type for `Sq8Vector` byte-encoding parse failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantizeError {
    /// Input ran out before the declared structure was complete.
    Truncated,
    /// Declared dimension didn't match the byte-payload length.
    DimMismatch { expected: u32, got: u32 },
}

impl fmt::Display for QuantizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "sq8 input truncated"),
            Self::DimMismatch { expected, got } => write!(
                f,
                "sq8 dim mismatch: expected {expected}, payload carries {got}"
            ),
        }
    }
}

// ===========================================================================
// Byte encoding.
//
// Layout (little-endian):
//   [u32 dim][f32 min][f32 max][u8 × dim]
//
// `dim` matches the byte-payload length. Encoded size = 12 + dim bytes.
// This is the standalone format; integration with the v4.37 segment
// envelope happens in v6.0.1 (new envelope sub-tag VECTOR_QUANTIZED).
// ===========================================================================

impl Sq8Vector {
    /// Serialise to the standalone byte format. Always succeeds.
    ///
    /// Panics if the dimension exceeds `u32::MAX` — but `DataType::Vector`
    /// already caps dim at `u32` at the type level, so this is a no-op
    /// invariant on real inputs.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let dim = u32::try_from(self.bytes.len())
            .expect("Sq8Vector dim fits in u32 by DataType::Vector contract");
        let mut out = Vec::with_capacity(12 + self.bytes.len());
        out.extend_from_slice(&dim.to_le_bytes());
        out.extend_from_slice(&self.min.to_le_bytes());
        out.extend_from_slice(&self.max.to_le_bytes());
        out.extend_from_slice(&self.bytes);
        out
    }

    /// Parse the standalone byte format. Strict — body length must
    /// equal the declared dim exactly (no extra trailing bytes).
    pub fn from_bytes(input: &[u8]) -> Result<Self, QuantizeError> {
        if input.len() < 12 {
            return Err(QuantizeError::Truncated);
        }
        let dim = u32::from_le_bytes([input[0], input[1], input[2], input[3]]);
        let min = f32::from_le_bytes([input[4], input[5], input[6], input[7]]);
        let max = f32::from_le_bytes([input[8], input[9], input[10], input[11]]);
        let body = &input[12..];
        if body.len() != dim as usize {
            let got = u32::try_from(body.len()).unwrap_or(u32::MAX);
            return Err(QuantizeError::DimMismatch { expected: dim, got });
        }
        Ok(Self {
            min,
            max,
            bytes: body.to_vec(),
        })
    }

    /// Bytes encoded by `to_bytes` for an instance of `dim` dimensions.
    /// Handy for buffer pre-sizing on the segment writer side.
    #[must_use]
    pub const fn encoded_size_for(dim: usize) -> usize {
        12 + dim
    }
}
