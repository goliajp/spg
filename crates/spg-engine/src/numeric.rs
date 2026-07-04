//! NUMERIC value construction, parsing, rescaling, and precision checks.
//! Split out of `lib.rs` (v7.32 engine modularisation): pure functions
//! with no Engine state — integer/float/text in, `Result<Value,
//! EngineError>` out.

use spg_storage::Value;

use crate::EngineError;

/// Promote an integer to a NUMERIC value at the requested scale.
/// Rejects values that, after scaling, would overflow the column's
/// precision budget.
pub(crate) fn numeric_from_integer(
    n: i128,
    precision: u8,
    scale: u8,
    col_name: &str,
) -> Result<Value<'static>, EngineError> {
    let factor = pow10_i128(scale);
    let scaled = n.checked_mul(factor).ok_or_else(|| {
        EngineError::Unsupported(alloc::format!(
            "integer overflow scaling value for column `{col_name}` to scale {scale}"
        ))
    })?;
    check_precision(scaled, precision, col_name)?;
    Ok(Value::Numeric { scaled, scale })
}

/// Float → NUMERIC. Uses round-half-away-from-zero on `x * 10^scale`,
/// then verifies the result fits the column's precision.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub(crate) fn numeric_from_float(
    x: f64,
    precision: u8,
    scale: u8,
    col_name: &str,
) -> Result<Value<'static>, EngineError> {
    if !x.is_finite() {
        return Err(EngineError::Unsupported(alloc::format!(
            "cannot store non-finite float in NUMERIC column `{col_name}`"
        )));
    }
    let mut factor = 1.0_f64;
    for _ in 0..scale {
        factor *= 10.0;
    }
    // Round half-away-from-zero by biasing then casting (`as i128`
    // truncates toward zero, so the bias + truncation gives the
    // desired rounding). `f64::floor` / `ceil` live in std; we don't
    // need them — the cast handles the truncation step.
    let shifted = x * factor;
    let biased = if shifted >= 0.0 {
        shifted + 0.5
    } else {
        shifted - 0.5
    };
    // Range-check before casting back to i128 — the cast itself is
    // saturating in Rust, which would silently truncate huge inputs.
    if !(-1e38..=1e38).contains(&biased) {
        return Err(EngineError::Unsupported(alloc::format!(
            "value {x} overflows NUMERIC range for column `{col_name}`"
        )));
    }
    let scaled = biased as i128;
    check_precision(scaled, precision, col_name)?;
    Ok(Value::Numeric { scaled, scale })
}

/// v7.17.0 Phase 3.P0-67 — parse PG-canonical decimal text into
/// `(mantissa: i128, source_scale: u8)`. Accepts optional sign,
/// optional integer part, optional fractional part. Rejects
/// scientific notation, embedded spaces, locale-specific
/// thousand separators. Returns None on bad input — coerce_value
/// turns that into a TypeMismatch error.
pub(crate) fn parse_numeric_text(s: &str) -> Option<(i128, u8)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Scientific notation (`1e3`, `1.5e2`, `3E-4`): split off the exponent
    // and fold it into the decimal scale. PG accepts this in numeric input.
    if let Some(idx) = s.find(['e', 'E']) {
        let exp: i32 = s[idx + 1..].parse().ok()?;
        let (mantissa, base_scale) = parse_plain_numeric(&s[..idx])?;
        // Effective scale = base fractional digits minus the exponent.
        let eff = i32::from(base_scale) - exp;
        return if eff >= 0 {
            Some((mantissa, u8::try_from(eff).ok()?))
        } else {
            // Negative scale → shift the mantissa up, land at scale 0.
            let shift = u8::try_from(-eff).ok()?;
            if shift > 38 {
                return None;
            }
            Some((mantissa.checked_mul(pow10_i128(shift))?, 0))
        };
    }
    parse_plain_numeric(s)
}

/// Parse a plain (no-exponent) decimal `[+-]int[.frac]` into `(mantissa, scale)`.
fn parse_plain_numeric(s: &str) -> Option<(i128, u8)> {
    if s.is_empty() {
        return None;
    }
    let (negative, rest) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    if rest.is_empty() {
        return None;
    }
    let (int_part, frac_part) = match rest.find('.') {
        Some(idx) => (&rest[..idx], &rest[idx + 1..]),
        None => (rest, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if int_part.bytes().any(|b| !b.is_ascii_digit()) {
        return None;
    }
    if frac_part.bytes().any(|b| !b.is_ascii_digit()) {
        return None;
    }
    let scale_u32 = u32::try_from(frac_part.len()).ok()?;
    if scale_u32 > u32::from(u8::MAX) {
        return None;
    }
    let scale = scale_u32 as u8;
    let mut digits = alloc::string::String::with_capacity(int_part.len() + frac_part.len() + 1);
    if negative {
        digits.push('-');
    }
    digits.push_str(int_part);
    digits.push_str(frac_part);
    // Strip a leading "+0..0" so parse doesn't choke on "00" etc.
    let digits = if digits == "-" {
        return None;
    } else if digits.is_empty() {
        "0"
    } else {
        digits.as_str()
    };
    let mantissa: i128 = digits.parse().ok()?;
    Some((mantissa, scale))
}

/// Move a Numeric value from `src_scale` to `dst_scale`. Going up
/// multiplies by 10; going down rounds half-away-from-zero.
pub(crate) fn numeric_rescale(
    scaled: i128,
    src_scale: u8,
    precision: u8,
    dst_scale: u8,
    col_name: &str,
) -> Result<Value<'static>, EngineError> {
    let new_scaled = if dst_scale >= src_scale {
        let bump = pow10_i128(dst_scale - src_scale);
        scaled.checked_mul(bump).ok_or_else(|| {
            EngineError::Unsupported(alloc::format!(
                "overflow rescaling NUMERIC for column `{col_name}`"
            ))
        })?
    } else {
        let drop = pow10_i128(src_scale - dst_scale);
        let half = drop / 2;
        if scaled >= 0 {
            (scaled + half) / drop
        } else {
            (scaled - half) / drop
        }
    };
    check_precision(new_scaled, precision, col_name)?;
    Ok(Value::Numeric {
        scaled: new_scaled,
        scale: dst_scale,
    })
}

/// Drop the fractional part of a scaled integer, returning the integer
/// portion (toward zero). Used for NUMERIC → INT casts.
pub(crate) const fn numeric_truncate_to_integer(scaled: i128, scale: u8) -> i128 {
    if scale == 0 {
        return scaled;
    }
    let factor = pow10_i128_const(scale);
    scaled / factor
}

/// Verify a scaled NUMERIC value fits the column's declared precision.
/// `precision == 0` is the "unconstrained" form (bare `NUMERIC`); we
/// skip the check there.
fn check_precision(scaled: i128, precision: u8, col_name: &str) -> Result<(), EngineError> {
    if precision == 0 {
        return Ok(());
    }
    let limit = pow10_i128(precision);
    if scaled.unsigned_abs() >= limit.unsigned_abs() {
        return Err(EngineError::Unsupported(alloc::format!(
            "NUMERIC value exceeds precision {precision} for column `{col_name}`"
        )));
    }
    Ok(())
}

/// Exact NUMERIC addition, aligning the two operands on the larger of
/// their scales (scaling *up* never rounds, so the sum stays exact).
/// Used by `sum(numeric)` / `avg(numeric)` accumulators — reuses the
/// same integral-mantissa arithmetic the `+` binop does, no f64.
/// Saturates on i128 overflow rather than panicking (extreme
/// magnitudes are out of scope; representable-range sums are exact).
pub(crate) fn numeric_add(a: i128, a_scale: u8, b: i128, b_scale: u8) -> (i128, u8) {
    if a_scale == b_scale {
        (a.saturating_add(b), a_scale)
    } else if a_scale > b_scale {
        let f = pow10_sat(a_scale - b_scale);
        (a.saturating_add(b.saturating_mul(f)), a_scale)
    } else {
        let f = pow10_sat(b_scale - a_scale);
        (a.saturating_mul(f).saturating_add(b), b_scale)
    }
}

/// PG-compatible `avg(numeric)` = `numeric_div(sum, count)`. Picks the
/// division result scale via a faithful port of PostgreSQL's
/// `select_div_scale` (base-10000 digit weights, `NUMERIC_MIN_SIG_DIGITS
/// = 16`, `DEC_DIGITS = 4`), then rounds half-away-from-zero — matching
/// PG18's exact avg output text including trailing digits. `count` must
/// be > 0 (callers gate on `count == 0 → NULL`).
pub(crate) fn numeric_avg(sum_scaled: i128, sum_scale: u8, count: i128) -> (i128, u8) {
    let rscale = select_div_scale(sum_scaled, sum_scale, count);
    let (num, den) = if i32::from(rscale) >= i32::from(sum_scale) {
        let k = rscale - sum_scale;
        (sum_scaled.saturating_mul(pow10_sat(k)), count)
    } else {
        let k = sum_scale - rscale;
        (sum_scaled, count.saturating_mul(pow10_sat(k)))
    };
    (div_round_half_away(num, den), rscale)
}

/// Port of PG `numeric.c:select_div_scale`. Returns the display scale
/// PG would give `sum / count`, clamped to SPG's `u8` scale field
/// (PG allows up to `NUMERIC_MAX_DISPLAY_SCALE = 1000`, but every
/// `Value::Numeric` scale in SPG is `u8`; scales beyond 255 are a
/// pre-existing SPG representation limit, not introduced here).
fn select_div_scale(sum_scaled: i128, sum_scale: u8, count: i128) -> u8 {
    let (w1, fd1) = base10000_weight_firstdigit(sum_scaled, sum_scale);
    let (w2, fd2) = base10000_weight_firstdigit(count, 0);
    // Estimate the weight of the quotient; if the two leading base-10000
    // digits are equal we can't be sure, so PG assumes var1 < var2.
    let mut qweight = w1 - w2;
    if fd1 <= fd2 {
        qweight -= 1;
    }
    // NUMERIC_MIN_SIG_DIGITS (16) - qweight * DEC_DIGITS (4), floored by
    // both inputs' display scales and NUMERIC_MIN_DISPLAY_SCALE (0),
    // capped at NUMERIC_MAX_DISPLAY_SCALE (1000).
    let mut rscale = 16 - qweight * 4;
    rscale = rscale.max(i32::from(sum_scale));
    rscale = rscale.max(0);
    rscale = rscale.min(1000);
    // SPG stores scale as u8; clamp (see doc comment).
    rscale.min(255) as u8
}

/// Base-10000 weight + leading digit of `|scaled / 10^scale|`, matching
/// how PG normalizes a `NumericVar` (digits grouped in 4s anchored on
/// the decimal point). Returns `(weight, first_digit)` where `weight`
/// is in units of 10000 and `first_digit` is the most-significant
/// non-zero base-10000 digit. Zero → `(0, 0)`.
fn base10000_weight_firstdigit(scaled: i128, scale: u8) -> (i32, i32) {
    let a = scaled.unsigned_abs();
    if a == 0 {
        return (0, 0);
    }
    let s = alloc::string::ToString::to_string(&a);
    let ndigits = s.len() as i32;
    let scale = i32::from(scale);
    let int_digits = ndigits - scale;
    if int_digits > 0 {
        // Integer part present: the most-significant group sits at
        // weight floor((int_digits - 1) / 4); its width is the leftover
        // 1..=4 leading decimal digits.
        let weight = (int_digits - 1) / 4;
        let top_len = (int_digits - weight * 4) as usize;
        let firstdigit: i32 = s[..top_len].parse().unwrap_or(0);
        (weight, firstdigit)
    } else {
        // |value| < 1: count leading fractional zeros, group by 4 from
        // the decimal point; the first non-zero group's index g gives
        // weight = -(g + 1).
        let lead_zeros = (-int_digits) as usize;
        let g = (lead_zeros as i32) / 4;
        let weight = -(g + 1);
        let mut frac = alloc::string::String::with_capacity(lead_zeros + s.len());
        for _ in 0..lead_zeros {
            frac.push('0');
        }
        frac.push_str(&s);
        let start = (4 * g) as usize;
        let mut group: alloc::string::String = frac[start..].chars().take(4).collect();
        while group.len() < 4 {
            group.push('0');
        }
        let firstdigit: i32 = group.parse().unwrap_or(0);
        (weight, firstdigit)
    }
}

/// Divide `num / den` (den > 0) rounding half away from zero, matching
/// PG's `round_var`.
fn div_round_half_away(num: i128, den: i128) -> i128 {
    let q = num / den;
    let r = num % den;
    if r.unsigned_abs() * 2 >= den.unsigned_abs() {
        if num >= 0 { q + 1 } else { q - 1 }
    } else {
        q
    }
}

/// `10^p` as i128, saturating at `i128::MAX` instead of panicking on
/// overflow (guards the avg-scale multiply for pathological scales).
fn pow10_sat(p: u8) -> i128 {
    let mut acc: i128 = 1;
    for _ in 0..p {
        match acc.checked_mul(10) {
            Some(v) => acc = v,
            None => return i128::MAX,
        }
    }
    acc
}

const fn pow10_i128_const(p: u8) -> i128 {
    let mut acc: i128 = 1;
    let mut i = 0;
    while i < p {
        acc *= 10;
        i += 1;
    }
    acc
}

fn pow10_i128(p: u8) -> i128 {
    pow10_i128_const(p)
}
