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
    let (negative, rest) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    if rest.is_empty() {
        return None;
    }
    // Reject scientific notation — bigdecimal collapses it before
    // hitting the wire, and we want a clear error if a stray `e`
    // sneaks in.
    if rest.bytes().any(|b| b == b'e' || b == b'E') {
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
