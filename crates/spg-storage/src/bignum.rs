//! v7.38 (read01, T3) — arbitrary-precision decimal for NUMERIC values that
//! overflow `i128` (PG's NUMERIC is unbounded; SPG's `i128` fast path tops out
//! near 38 digits). Clean-room schoolbook arithmetic on base-10^9 limbs (each
//! limb holds 9 decimal digits, little-endian), a sign, and a decimal `scale`.
//! This is phase C1: representation + add / sub / mul / cmp + the `i128` bridge
//! + decimal-string conversion. Division (Knuth D) and sqrt (Newton) follow in
//! later phases; this module is not yet wired into the engine.
//!
//! Learned from PG's NUMERIC design (base-10000 `NBASE` digits, the same
//! schoolbook shape) but re-implemented over SPG's own `u32` base-10^9 limbs.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Base of a limb: 10^9, so a limb is 9 decimal digits and fits in `u32`
/// (10^9 < 2^32). A product of two limbs (< 10^18) fits in `u64`.
const BASE: u64 = 1_000_000_000;
const BASE_DIGITS: usize = 9;

/// An arbitrary-precision decimal: `(-1)^neg · (Σ limbs[i]·BASE^i) · 10^-scale`.
/// `limbs` is little-endian with no trailing (most-significant) zero limbs; the
/// value zero is the empty limb vector with `neg == false`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BigNumeric {
    neg: bool,
    limbs: Vec<u32>,
    scale: u8,
}

impl BigNumeric {
    /// True when the magnitude is zero (regardless of sign / scale).
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    #[must_use]
    pub fn scale(&self) -> u8 {
        self.scale
    }

    /// Drop most-significant zero limbs and canonicalize a zero to `+0`.
    fn normalize(&mut self) {
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
        if self.limbs.is_empty() {
            self.neg = false;
        }
    }

    /// Build from an `i128` mantissa at a given scale.
    #[must_use]
    pub fn from_i128(mut v: i128, scale: u8) -> Self {
        let neg = v < 0;
        let mut limbs = Vec::new();
        // Use the unsigned magnitude; i128::MIN's magnitude still fits in u128.
        let mut mag = v.unsigned_abs();
        let _ = &mut v;
        while mag != 0 {
            limbs.push((mag % u128::from(BASE)) as u32);
            mag /= u128::from(BASE);
        }
        let mut out = BigNumeric { neg, limbs, scale };
        out.normalize();
        out
    }

    /// Convert back to `(i128 mantissa, scale)` when the mantissa fits; `None`
    /// on overflow (the caller keeps the big form). Scale is preserved.
    #[must_use]
    pub fn to_i128(&self) -> Option<i128> {
        let mut mag: u128 = 0;
        for &limb in self.limbs.iter().rev() {
            mag = mag.checked_mul(u128::from(BASE))?.checked_add(u128::from(limb))?;
        }
        if self.neg {
            // magnitude up to i128::MIN's magnitude (2^127) is representable.
            if mag <= (i128::MAX as u128) + 1 {
                Some((mag as i128).wrapping_neg())
            } else {
                None
            }
        } else {
            i128::try_from(mag).ok()
        }
    }

    /// Compare magnitudes only (ignores sign + scale alignment).
    fn cmp_mag(a: &[u32], b: &[u32]) -> core::cmp::Ordering {
        use core::cmp::Ordering;
        match a.len().cmp(&b.len()) {
            Ordering::Equal => {
                for i in (0..a.len()).rev() {
                    match a[i].cmp(&b[i]) {
                        Ordering::Equal => {}
                        other => return other,
                    }
                }
                Ordering::Equal
            }
            other => other,
        }
    }

    /// `a + b` on magnitudes (limb vectors), little-endian.
    fn add_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
        let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
        let mut carry: u64 = 0;
        for i in 0..a.len().max(b.len()) {
            let av = u64::from(a.get(i).copied().unwrap_or(0));
            let bv = u64::from(b.get(i).copied().unwrap_or(0));
            let s = av + bv + carry;
            out.push((s % BASE) as u32);
            carry = s / BASE;
        }
        if carry != 0 {
            out.push(carry as u32);
        }
        out
    }

    /// `a - b` on magnitudes, requires `a >= b`; little-endian.
    fn sub_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
        let mut out = Vec::with_capacity(a.len());
        let mut borrow: i64 = 0;
        for i in 0..a.len() {
            let av = i64::from(a[i]);
            let bv = i64::from(b.get(i).copied().unwrap_or(0));
            let mut d = av - bv - borrow;
            if d < 0 {
                d += BASE as i64;
                borrow = 1;
            } else {
                borrow = 0;
            }
            out.push(d as u32);
        }
        while out.last() == Some(&0) {
            out.pop();
        }
        out
    }

    /// Multiply the magnitude by 10^k (used to align scales), little-endian.
    fn mul_pow10(limbs: &[u32], k: u32) -> Vec<u32> {
        if limbs.is_empty() {
            return Vec::new();
        }
        let whole = (k as usize) / BASE_DIGITS;
        let rem = (k as usize) % BASE_DIGITS;
        // shift by `rem` decimal digits = multiply by 10^rem within the base
        let mut cur: Vec<u32> = if rem == 0 {
            limbs.to_vec()
        } else {
            let factor = 10u64.pow(rem as u32);
            let mut out = Vec::with_capacity(limbs.len() + 1);
            let mut carry: u64 = 0;
            for &l in limbs {
                let v = u64::from(l) * factor + carry;
                out.push((v % BASE) as u32);
                carry = v / BASE;
            }
            if carry != 0 {
                out.push(carry as u32);
            }
            out
        };
        // then shift by `whole` full limbs
        if whole > 0 {
            let mut shifted = Vec::with_capacity(cur.len() + whole);
            shifted.resize(whole, 0);
            shifted.append(&mut cur);
            cur = shifted;
        }
        cur
    }

    /// Align two values to a common scale (the larger of the two), returning the
    /// scaled magnitude limb vectors and the shared scale.
    fn align(&self, other: &Self) -> (Vec<u32>, Vec<u32>, u8) {
        use core::cmp::Ordering;
        match self.scale.cmp(&other.scale) {
            Ordering::Equal => (self.limbs.clone(), other.limbs.clone(), self.scale),
            Ordering::Less => {
                let k = u32::from(other.scale - self.scale);
                (Self::mul_pow10(&self.limbs, k), other.limbs.clone(), other.scale)
            }
            Ordering::Greater => {
                let k = u32::from(self.scale - other.scale);
                (self.limbs.clone(), Self::mul_pow10(&other.limbs, k), self.scale)
            }
        }
    }

    /// Signed comparison honoring sign + scale.
    #[must_use]
    pub fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        use core::cmp::Ordering;
        match (self.is_zero(), other.is_zero()) {
            (true, true) => return Ordering::Equal,
            _ => {}
        }
        match (self.neg, other.neg) {
            (false, true) => return Ordering::Greater,
            (true, false) => return Ordering::Less,
            _ => {}
        }
        let (a, b, _) = self.align(other);
        let mag = Self::cmp_mag(&a, &b);
        if self.neg { mag.reverse() } else { mag }
    }

    #[must_use]
    pub fn add(&self, other: &Self) -> Self {
        let (a, b, scale) = self.align(other);
        let out = if self.neg == other.neg {
            BigNumeric { neg: self.neg, limbs: Self::add_mag(&a, &b), scale }
        } else {
            // opposite signs → subtract the smaller magnitude from the larger.
            match Self::cmp_mag(&a, &b) {
                core::cmp::Ordering::Less => {
                    BigNumeric { neg: other.neg, limbs: Self::sub_mag(&b, &a), scale }
                }
                _ => BigNumeric { neg: self.neg, limbs: Self::sub_mag(&a, &b), scale },
            }
        };
        let mut out = out;
        out.normalize();
        out
    }

    #[must_use]
    pub fn neg(&self) -> Self {
        let mut out = self.clone();
        if !out.is_zero() {
            out.neg = !out.neg;
        }
        out
    }

    #[must_use]
    pub fn sub(&self, other: &Self) -> Self {
        self.add(&other.neg())
    }

    #[must_use]
    pub fn mul(&self, other: &Self) -> Self {
        if self.is_zero() || other.is_zero() {
            return BigNumeric { neg: false, limbs: Vec::new(), scale: self.scale + other.scale };
        }
        let mut acc = alloc::vec![0u64; self.limbs.len() + other.limbs.len()];
        for (i, &a) in self.limbs.iter().enumerate() {
            let mut carry: u64 = 0;
            for (j, &b) in other.limbs.iter().enumerate() {
                let cur = acc[i + j] + u64::from(a) * u64::from(b) + carry;
                acc[i + j] = cur % BASE;
                carry = cur / BASE;
            }
            acc[i + other.limbs.len()] += carry;
        }
        // propagate any residual carries and narrow to u32 limbs
        let mut limbs = Vec::with_capacity(acc.len());
        let mut carry: u64 = 0;
        for v in acc {
            let cur = v + carry;
            limbs.push((cur % BASE) as u32);
            carry = cur / BASE;
        }
        while carry != 0 {
            limbs.push((carry % BASE) as u32);
            carry /= BASE;
        }
        let mut out = BigNumeric {
            neg: self.neg != other.neg,
            limbs,
            scale: self.scale + other.scale,
        };
        out.normalize();
        out
    }

    /// Render as a decimal string (`-123.4500` style), inserting the scale point.
    #[must_use]
    pub fn to_decimal_str(&self) -> String {
        if self.is_zero() {
            if self.scale == 0 {
                return String::from("0");
            }
            return alloc::format!("0.{}", "0".repeat(self.scale as usize));
        }
        // most-significant limb without leading zeros, the rest zero-padded to 9.
        let mut digits = String::new();
        for (idx, &limb) in self.limbs.iter().rev().enumerate() {
            if idx == 0 {
                digits.push_str(&limb.to_string());
            } else {
                digits.push_str(&alloc::format!("{limb:0width$}", width = BASE_DIGITS));
            }
        }
        let scale = self.scale as usize;
        let body = if scale == 0 {
            digits
        } else {
            // ensure at least scale+1 digits so the point has an integer side.
            if digits.len() <= scale {
                let pad = scale + 1 - digits.len();
                let padded = alloc::format!("{}{}", "0".repeat(pad), digits);
                let point = padded.len() - scale;
                alloc::format!("{}.{}", &padded[..point], &padded[point..])
            } else {
                let point = digits.len() - scale;
                alloc::format!("{}.{}", &digits[..point], &digits[point..])
            }
        };
        if self.neg {
            alloc::format!("-{body}")
        } else {
            body
        }
    }

    /// Parse a plain decimal string (`[-]digits[.digits]`, no exponent) into a
    /// `BigNumeric`. Returns `None` on malformed input.
    #[must_use]
    pub fn from_decimal_str(s: &str) -> Option<Self> {
        let s = s.trim();
        let (neg, rest) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        let (int_part, frac_part) = match rest.split_once('.') {
            Some((i, f)) => (i, f),
            None => (rest, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return None;
        }
        if !int_part.bytes().all(|b| b.is_ascii_digit())
            || !frac_part.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let scale = u8::try_from(frac_part.len()).ok()?;
        let mut all: String = String::with_capacity(int_part.len() + frac_part.len());
        all.push_str(int_part);
        all.push_str(frac_part);
        // strip leading zeros of the combined digit string (keep at least one).
        let trimmed = all.trim_start_matches('0');
        let digits = if trimmed.is_empty() { "0" } else { trimmed };
        // group into base-10^9 limbs from the least-significant end.
        let bytes = digits.as_bytes();
        let mut limbs = Vec::new();
        let mut i = bytes.len();
        while i > 0 {
            let start = i.saturating_sub(BASE_DIGITS);
            let chunk = core::str::from_utf8(&bytes[start..i]).ok()?;
            limbs.push(chunk.parse::<u32>().ok()?);
            i = start;
        }
        let mut out = BigNumeric { neg, limbs, scale };
        out.normalize();
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A small deterministic LCG so the fuzz is reproducible without std rand.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0
        }
        fn i128_small(&mut self) -> i128 {
            // values with magnitude up to ~1e18 so products/sums stay in i128.
            let m = (self.next() % 2_000_000_000_000_000_000) as i128;
            if self.next() & 1 == 0 { m } else { -m }
        }
    }

    #[test]
    fn i128_bridge_round_trips() {
        for v in [0i128, 1, -1, 123, -456, i64::MAX as i128, i128::MAX, i128::MIN, 10i128.pow(30)] {
            assert_eq!(BigNumeric::from_i128(v, 0).to_i128(), Some(v), "v={v}");
        }
    }

    #[test]
    fn decimal_str_round_trips() {
        for s in ["0", "123", "-123", "1.50", "-0.001", "1000000000", "999999999999999999999999999999"] {
            let b = BigNumeric::from_decimal_str(s).unwrap();
            assert_eq!(b.to_decimal_str(), s, "s={s}");
        }
    }

    #[test]
    fn fuzz_vs_i128() {
        let mut rng = Lcg(0x1234_5678_9abc_def0);
        for _ in 0..20_000 {
            let a = rng.i128_small();
            let b = rng.i128_small();
            let ba = BigNumeric::from_i128(a, 0);
            let bb = BigNumeric::from_i128(b, 0);
            assert_eq!(ba.add(&bb).to_i128(), Some(a + b), "add {a}+{b}");
            assert_eq!(ba.sub(&bb).to_i128(), Some(a - b), "sub {a}-{b}");
            assert_eq!(ba.mul(&bb).to_i128(), Some(a * b), "mul {a}*{b}");
            assert_eq!(ba.cmp(&bb), a.cmp(&b), "cmp {a} vs {b}");
        }
    }

    #[test]
    fn overflow_stays_big() {
        // 10^30 * 10^30 = 10^60 overflows i128 → to_i128 None, but decimal exact.
        let a = BigNumeric::from_i128(10i128.pow(30), 0);
        let p = a.mul(&a);
        assert_eq!(p.to_i128(), None);
        let mut expect = String::from("1");
        expect.push_str(&"0".repeat(60));
        assert_eq!(p.to_decimal_str(), expect);
    }

    #[test]
    fn scale_align_add() {
        // 1.5 + 0.25 = 1.75
        let a = BigNumeric::from_decimal_str("1.5").unwrap();
        let b = BigNumeric::from_decimal_str("0.25").unwrap();
        assert_eq!(a.add(&b).to_decimal_str(), "1.75");
    }
}
