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

    /// v7.38 (read01, T3.C3) — expose the on-disk parts (sign, base-10^9 limbs
    /// little-endian, scale) for the codec.
    #[must_use]
    pub fn parts(&self) -> (bool, &[u32], u8) {
        (self.neg, &self.limbs, self.scale)
    }

    /// Rebuild from codec parts. Normalizes (a canonical big value never has a
    /// mantissa that fits `i128` — the caller collapses those to `Numeric`).
    #[must_use]
    pub fn from_parts(neg: bool, limbs: Vec<u32>, scale: u8) -> Self {
        let mut out = BigNumeric { neg, limbs, scale };
        out.normalize();
        out
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

    /// Multiply a magnitude by a small scalar `< BASE`, little-endian.
    fn mul_scalar(limbs: &[u32], factor: u64) -> Vec<u32> {
        if factor == 0 || limbs.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(limbs.len() + 1);
        let mut carry: u64 = 0;
        for &l in limbs {
            let v = u64::from(l) * factor + carry;
            out.push((v % BASE) as u32);
            carry = v / BASE;
        }
        while carry != 0 {
            out.push((carry % BASE) as u32);
            carry /= BASE;
        }
        out
    }

    /// Integer magnitude division: `u / v` → `(quotient, remainder)`, both
    /// little-endian, via Knuth's Algorithm D (TAOCP 4.3.1) over base-10^9 limbs.
    /// `v` must be non-zero and normalized. Learned from the classic algorithm;
    /// re-implemented on `u32` limbs with `u64`/`i64` intermediates.
    fn div_rem_mag(u: &[u32], v: &[u32]) -> (Vec<u32>, Vec<u32>) {
        use core::cmp::Ordering;
        // u < v → quotient 0, remainder u.
        if Self::cmp_mag(u, v) == Ordering::Less {
            let mut r = u.to_vec();
            while r.last() == Some(&0) {
                r.pop();
            }
            return (Vec::new(), r);
        }
        let n = v.len();
        // Short division by a single limb.
        if n == 1 {
            let d = u64::from(v[0]);
            let mut rem: u64 = 0;
            let mut q = alloc::vec![0u32; u.len()];
            for i in (0..u.len()).rev() {
                let cur = rem * BASE + u64::from(u[i]);
                q[i] = (cur / d) as u32;
                rem = cur % d;
            }
            while q.last() == Some(&0) {
                q.pop();
            }
            let r = if rem == 0 { Vec::new() } else { alloc::vec![rem as u32] };
            return (q, r);
        }
        // D1. Normalize so the divisor's top limb is >= BASE/2.
        let d = BASE / (u64::from(v[n - 1]) + 1);
        let vn = Self::mul_scalar(v, d);
        let vn = {
            let mut vn = vn;
            vn.resize(n, 0); // exactly n limbs (d keeps v the same length)
            vn
        };
        let mut un = Self::mul_scalar(u, d);
        let m = u.len() - n; // quotient has m+1 limbs
        un.resize(u.len() + 1, 0); // room for a leading limb
        let mut q = alloc::vec![0u32; m + 1];
        // D2..D7. Loop over quotient limbs from most significant.
        for j in (0..=m).rev() {
            // D3. Estimate qhat.
            let num = u128::from(un[j + n]) * u128::from(BASE) + u128::from(un[j + n - 1]);
            let mut qhat = num / u128::from(vn[n - 1]);
            let mut rhat = num % u128::from(vn[n - 1]);
            while qhat >= u128::from(BASE)
                || qhat * u128::from(vn[n - 2]) > rhat * u128::from(BASE) + u128::from(un[j + n - 2])
            {
                qhat -= 1;
                rhat += u128::from(vn[n - 1]);
                if rhat >= u128::from(BASE) {
                    break;
                }
            }
            // D4. Multiply and subtract qhat*vn from un[j..j+n+1].
            let mut borrow: i64 = 0;
            let mut carry: u64 = 0;
            for i in 0..n {
                let p = qhat * u128::from(vn[i]) + u128::from(carry);
                carry = (p / u128::from(BASE)) as u64;
                let sub = (p % u128::from(BASE)) as i64;
                let mut t = i64::from(un[j + i]) - sub - borrow;
                if t < 0 {
                    t += BASE as i64;
                    borrow = 1;
                } else {
                    borrow = 0;
                }
                un[j + i] = t as u32;
            }
            let mut t = i64::from(un[j + n]) - carry as i64 - borrow;
            // D5/D6. If we subtracted too much, add back one multiple of vn.
            if t < 0 {
                qhat -= 1;
                let mut c: u64 = 0;
                for i in 0..n {
                    let s = u64::from(un[j + i]) + u64::from(vn[i]) + c;
                    un[j + i] = (s % BASE) as u32;
                    c = s / BASE;
                }
                t += (BASE as i64) + c as i64;
            }
            un[j + n] = t as u32;
            q[j] = qhat as u32;
        }
        // D8. Unnormalize the remainder: un[0..n] / d.
        let mut rem = un[..n].to_vec();
        while rem.last() == Some(&0) {
            rem.pop();
        }
        let (rem, _) = Self::div_rem_mag(&rem, &[d as u32]);
        let mut q = q;
        while q.last() == Some(&0) {
            q.pop();
        }
        (q, rem)
    }

    /// Signed integer division truncating toward zero (like `i128 / i128`),
    /// ignoring scale. Returns `(quotient, remainder)`.
    #[must_use]
    pub fn div_rem_int(&self, other: &Self) -> (Self, Self) {
        let (q, r) = Self::div_rem_mag(&self.limbs, &other.limbs);
        let mut quo = BigNumeric { neg: self.neg != other.neg, limbs: q, scale: 0 };
        let mut rem = BigNumeric { neg: self.neg, limbs: r, scale: 0 };
        quo.normalize();
        rem.normalize();
        (quo, rem)
    }

    /// Fixed-point division to a target `result_scale`, rounded half-away-from-
    /// zero — the shape PG's `numeric / numeric` uses. Errors are the caller's:
    /// dividing by zero returns `None`.
    #[must_use]
    pub fn div(&self, other: &Self, result_scale: u8) -> Option<Self> {
        if other.is_zero() {
            return None;
        }
        // Scale the dividend so the integer quotient carries result_scale + 1
        // guard digit, relative to the operands' own scales.
        let want = i32::from(result_scale) + 1 + i32::from(other.scale) - i32::from(self.scale);
        let num = if want > 0 {
            Self::mul_pow10(&self.limbs, want as u32)
        } else {
            self.limbs.clone()
        };
        let den = if want < 0 {
            Self::mul_pow10(&other.limbs, (-want) as u32)
        } else {
            other.limbs.clone()
        };
        let (mut q, _rem) = Self::div_rem_mag(&num, &den);
        // The quotient carries one guard digit past result_scale. Round
        // half-away-from-zero (PG numeric): guard >= 5 bumps, then drop it.
        let guard = if q.is_empty() { 0 } else { q[0] % 10 };
        q = Self::div_rem_mag(&q, &[10]).0; // drop the guard digit
        if guard >= 5 {
            q = Self::add_mag(&q, &[1]);
        }
        let mut out = BigNumeric { neg: self.neg != other.neg, limbs: q, scale: result_scale };
        out.normalize();
        Some(out)
    }

    /// v7.38 (read01, C4) — floor of the integer square root of this value's
    /// magnitude taken as an integer (scale ignored). Newton's method on the
    /// base-10^9 limbs, starting from a decimal-digit overestimate and
    /// descending to the floor. Zero → zero.
    fn isqrt_mag(&self) -> Self {
        use core::cmp::Ordering;
        let n = BigNumeric { neg: false, limbs: self.limbs.clone(), scale: 0 };
        if n.is_zero() {
            return BigNumeric { neg: false, limbs: Vec::new(), scale: 0 };
        }
        // Decimal digit count of the magnitude.
        let top = *n.limbs.last().unwrap();
        let ndigits = (n.limbs.len() - 1) * BASE_DIGITS + top.to_string().len();
        // Overestimate x0 = 10^ceil(ndigits/2) >= sqrt(n).
        let half = ndigits.div_ceil(2);
        let one = BigNumeric::from_i128(1, 0);
        let two = BigNumeric::from_i128(2, 0);
        let mut x = BigNumeric {
            neg: false,
            limbs: Self::mul_pow10(&one.limbs, half as u32),
            scale: 0,
        };
        // Newton: x_{k+1} = (x + n/x) / 2, monotonically descending to the floor.
        loop {
            let (div, _) = n.div_rem_int(&x);
            let sum = x.add(&div);
            let (next, _) = sum.div_rem_int(&two);
            if next.cmp(&x) != Ordering::Less {
                break;
            }
            x = next;
        }
        // Descend any residual overshoot so x*x <= n exactly.
        while x.mul(&x).cmp(&n) == Ordering::Greater {
            x = x.sub(&one);
        }
        x
    }

    /// v7.38 (read01, C4) — square root at a target display scale, rounded
    /// half-away-from-zero, the shape PG's numeric `sqrt` uses. `None` for a
    /// negative value (the caller raises the domain error). The caller picks
    /// `result_scale` (PG's ~16-significant-digit rule) and guarantees it is at
    /// least the argument's own scale.
    #[must_use]
    pub fn sqrt(&self, result_scale: u8) -> Option<Self> {
        use core::cmp::Ordering;
        if self.neg && !self.is_zero() {
            return None;
        }
        if self.is_zero() {
            return Some(BigNumeric { neg: false, limbs: Vec::new(), scale: result_scale });
        }
        // Compute one guard digit past result_scale, then round it off.
        // radicand = mantissa * 10^(2*(result_scale+1) - scale); isqrt of it is
        // floor(sqrt(value) * 10^(result_scale+1)).
        let shift = 2 * (i32::from(result_scale) + 1) - i32::from(self.scale);
        let mant = BigNumeric { neg: false, limbs: self.limbs.clone(), scale: 0 };
        let radicand = if shift >= 0 {
            BigNumeric { neg: false, limbs: Self::mul_pow10(&mant.limbs, shift as u32), scale: 0 }
        } else {
            let (q, _) = Self::div_rem_mag(&mant.limbs, &Self::mul_pow10(&[1], (-shift) as u32));
            BigNumeric { neg: false, limbs: q, scale: 0 }
        };
        let root = radicand.isqrt_mag();
        // Round the guard digit half-away-from-zero, drop it.
        let ten = BigNumeric::from_i128(10, 0);
        let (q, r) = root.div_rem_int(&ten);
        let five = BigNumeric::from_i128(5, 0);
        let mut rounded = if r.cmp(&five) != Ordering::Less {
            q.add(&BigNumeric::from_i128(1, 0))
        } else {
            q
        };
        rounded.scale = result_scale;
        rounded.normalize();
        Some(rounded)
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

    #[test]
    fn fuzz_div_int_vs_i128() {
        let mut rng = Lcg(0xdead_beef_cafe_babe);
        for _ in 0..20_000 {
            let a = rng.i128_small();
            let mut b = rng.i128_small();
            if b == 0 {
                b = 1;
            }
            let (q, r) = BigNumeric::from_i128(a, 0).div_rem_int(&BigNumeric::from_i128(b, 0));
            assert_eq!(q.to_i128(), Some(a / b), "quot {a}/{b}");
            assert_eq!(r.to_i128(), Some(a % b), "rem {a}%{b}");
        }
    }

    #[test]
    fn div_multi_limb() {
        // A quotient that exercises the full Knuth D loop (multi-limb divisor).
        let a = BigNumeric::from_decimal_str("123456789012345678901234567890").unwrap();
        let b = BigNumeric::from_decimal_str("987654321987654321").unwrap();
        let (q, r) = a.div_rem_int(&b);
        // reconstruct: q*b + r == a
        let recon = q.mul(&b).add(&r);
        assert_eq!(recon.to_decimal_str(), a.to_decimal_str());
        assert_eq!(b.cmp(&r), core::cmp::Ordering::Greater); // r < b
    }

    #[test]
    fn div_fixed_point() {
        let ten = BigNumeric::from_decimal_str("10").unwrap();
        let three = BigNumeric::from_decimal_str("3").unwrap();
        assert_eq!(ten.div(&three, 4).unwrap().to_decimal_str(), "3.3333");
        let one = BigNumeric::from_decimal_str("1").unwrap();
        let seven = BigNumeric::from_decimal_str("7").unwrap();
        assert_eq!(one.div(&seven, 6).unwrap().to_decimal_str(), "0.142857");
        // half-away rounding: 1/8 = 0.125 → scale 2 rounds to 0.13.
        let eight = BigNumeric::from_decimal_str("8").unwrap();
        assert_eq!(one.div(&eight, 2).unwrap().to_decimal_str(), "0.13");
        // division by zero → None.
        assert!(one.div(&BigNumeric::from_decimal_str("0").unwrap(), 4).is_none());
    }

    #[test]
    fn isqrt_exact_and_floor() {
        // Perfect square far beyond i128: (12345678901234567890)^2.
        let n = BigNumeric::from_decimal_str("152415787532388367501905199875019052100").unwrap();
        assert_eq!(n.isqrt_mag().to_decimal_str(), "12345678901234567890");
        // Floor for a non-square: isqrt(10) = 3, isqrt(15) = 3, isqrt(16) = 4.
        for (v, want) in [("0", "0"), ("1", "1"), ("2", "1"), ("10", "3"), ("15", "3"), ("16", "4"), ("99", "9"), ("100", "10")] {
            let b = BigNumeric::from_decimal_str(v).unwrap();
            assert_eq!(b.isqrt_mag().to_decimal_str(), want, "isqrt({v})");
        }
    }

    #[test]
    fn sqrt_scale_and_rounding() {
        // sqrt(2) at scale 15 rounds to PG's value.
        let two = BigNumeric::from_decimal_str("2").unwrap();
        assert_eq!(two.sqrt(15).unwrap().to_decimal_str(), "1.414213562373095");
        // sqrt(10) rounds down (16th digit 3).
        let ten = BigNumeric::from_decimal_str("10").unwrap();
        assert_eq!(ten.sqrt(15).unwrap().to_decimal_str(), "3.162277660168379");
        // Perfect squares are exact at any scale.
        let nine = BigNumeric::from_decimal_str("9").unwrap();
        assert_eq!(nine.sqrt(15).unwrap().to_decimal_str(), "3.000000000000000");
        // A big perfect square, scale 0.
        let big = BigNumeric::from_decimal_str("152415787532388367501905199875019052100").unwrap();
        assert_eq!(big.sqrt(0).unwrap().to_decimal_str(), "12345678901234567890");
        // Negative → None (caller raises the domain error); zero is fine.
        assert!(BigNumeric::from_decimal_str("-4").unwrap().sqrt(2).is_none());
        assert_eq!(BigNumeric::from_decimal_str("0").unwrap().sqrt(3).unwrap().to_decimal_str(), "0.000");
    }

    #[test]
    fn fuzz_isqrt_vs_i128() {
        // Deterministic LCG: isqrt of fit-i128 values matches the property
        // x^2 <= n < (x+1)^2.
        let mut state: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            state
        };
        for _ in 0..20_000 {
            let n = u128::from(next()) | (u128::from(next()) << 64);
            let n = n % (1u128 << 100); // keep products in range
            let b = BigNumeric::from_i128(n as i128, 0);
            let root = b.isqrt_mag();
            let rl = root.to_i128().unwrap() as u128;
            assert!(rl * rl <= n, "root^2 > n for n={n}");
            assert!((rl + 1) * (rl + 1) > n, "(root+1)^2 <= n for n={n}");
        }
    }
}
