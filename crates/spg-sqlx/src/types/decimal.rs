//! v7.17.0 Phase 3.P0-67 — `Type` / `Encode` / `Decode` for the
//! NUMERIC family.
//!
//! Engine-side, NUMERIC carries `(scaled: i128, scale: u8)`.
//! `(scaled, scale)` ↔ BigDecimal mantissa/exponent is a
//! straight reshape — both encode "digits × 10^-scale". The
//! `i128` ceiling caps SPG NUMERIC at precision 38; values
//! beyond that range surface as Encode errors instead of
//! silently truncating.
//!
//! The text path (`Decode<String>` for NUMERIC) lives in
//! `types/text.rs` and uses [`numeric_to_text`] below — every
//! sqlx user gets it for free.

use spg_embedded::ValueOwned as EngineValue;

#[cfg(feature = "bigdecimal")]
use sqlx_core::decode::Decode;
#[cfg(feature = "bigdecimal")]
use sqlx_core::error::BoxDynError;
#[cfg(feature = "bigdecimal")]
use sqlx_core::types::Type;

#[cfg(feature = "bigdecimal")]
use crate::database::Spg;
#[cfg(feature = "bigdecimal")]
use crate::type_info::{Kind, SpgTypeInfo};
#[cfg(feature = "bigdecimal")]
use crate::value::SpgValueRef;

/// Render a NUMERIC cell into PG's canonical decimal text
/// (`"123.45"` / `"-0.001"` / `"42"`). Mirrors what
/// `spg_engine::eval::format_numeric` produces — kept in lock-
/// step so the sqlx adapter and pgwire show identical strings
/// for the same cell. Inlined here so spg-sqlx doesn't grow a
/// direct dep on spg-engine.
pub(crate) fn numeric_to_text(scaled: i128, scale: u8) -> String {
    if scale == 0 {
        return format!("{scaled}");
    }
    let negative = scaled < 0;
    let mag_str = scaled.unsigned_abs().to_string();
    let mag_bytes = mag_str.as_bytes();
    let scale_u = scale as usize;
    let mut out = String::with_capacity(mag_str.len() + 3);
    if negative {
        out.push('-');
    }
    if mag_bytes.len() <= scale_u {
        out.push('0');
        out.push('.');
        for _ in mag_bytes.len()..scale_u {
            out.push('0');
        }
        out.push_str(&mag_str);
    } else {
        let split = mag_bytes.len() - scale_u;
        out.push_str(&mag_str[..split]);
        out.push('.');
        out.push_str(&mag_str[split..]);
    }
    out
}

/// Internal — `Decode<String>` for NUMERIC cells in
/// `types/text.rs` falls through to this helper.
pub(crate) fn try_numeric_as_string(value: &EngineValue) -> Option<String> {
    match value {
        EngineValue::Numeric {
            scaled,
            scale,
            kind: spg_storage::NumericKind::Finite,
        } => Some(numeric_to_text(*scaled, *scale)),
        _ => None,
    }
}

// ---- bigdecimal::BigDecimal bridge ---------------------------

#[cfg(feature = "bigdecimal")]
mod bd {
    use super::*;
    use bigdecimal::BigDecimal;
    use num_bigint::{BigInt, Sign};
    use num_traits::ToPrimitive;
    use sqlx_core::encode::{Encode, IsNull};

    use crate::arguments::SpgArgumentValue;

    impl Type<Spg> for BigDecimal {
        fn type_info() -> SpgTypeInfo {
            SpgTypeInfo::of(Kind::Numeric)
        }

        fn compatible(ty: &SpgTypeInfo) -> bool {
            // Mirrors sqlx-postgres's BigDecimal::compatible:
            // integer columns are valid NUMERIC inputs on the
            // wire, so the bridge accepts them too.
            matches!(
                ty.kind(),
                Kind::Numeric | Kind::Int | Kind::BigInt | Kind::SmallInt
            )
        }
    }

    impl<'q> Encode<'q, Spg> for BigDecimal {
        fn encode_by_ref(
            &self,
            buf: &mut Vec<SpgArgumentValue<'q>>,
        ) -> Result<IsNull, BoxDynError> {
            let (scaled, scale) = bigdecimal_to_scaled(self)?;
            buf.push(SpgArgumentValue {
                value: EngineValue::Numeric {
                    scaled,
                    scale,
                    kind: spg_storage::NumericKind::Finite,
                },
                type_info: Some(SpgTypeInfo::of(Kind::Numeric)),
                _phantom: core::marker::PhantomData,
            });
            Ok(IsNull::No)
        }
    }

    impl<'r> Decode<'r, Spg> for BigDecimal {
        fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
            match value.engine() {
                EngineValue::Numeric {
                    scaled,
                    scale,
                    kind: spg_storage::NumericKind::Finite,
                } => Ok(scaled_to_bigdecimal(*scaled, *scale)),
                // Generous coerce: small ints are valid NUMERICs
                // too. Mirrors what PG would do on the wire when
                // a column is widened.
                EngineValue::Int(n) => Ok(BigDecimal::from(*n)),
                EngineValue::BigInt(n) => Ok(BigDecimal::from(*n)),
                EngineValue::SmallInt(n) => Ok(BigDecimal::from(i32::from(*n))),
                other => Err(format!("cannot decode {other:?} as BigDecimal / NUMERIC").into()),
            }
        }
    }

    /// Reshape a BigDecimal into SPG's `(i128 scaled, u8 scale)`
    /// pair. Errors out if the mantissa overflows i128 (precision
    /// `> 38`) or the exponent is outside `0..=u8::MAX` — both
    /// states fall outside what SPG NUMERIC can represent.
    fn bigdecimal_to_scaled(d: &BigDecimal) -> Result<(i128, u8), BoxDynError> {
        // BigDecimal::as_bigint_and_exponent gives (mantissa,
        // exponent) where the value = mantissa * 10^(-exponent).
        // Non-negative exponent → `scale = exponent`; negative
        // exponent → fold the magnitude into the mantissa, with
        // `scale = 0`.
        let (mantissa, exp) = d.as_bigint_and_exponent();
        let (scaled, scale) = if exp < 0 {
            let factor = BigInt::from(10u8).pow((-exp) as u32);
            let folded = mantissa * factor;
            (folded, 0u8)
        } else if exp > i64::from(u8::MAX) {
            return Err(format!(
                "BigDecimal scale {exp} exceeds SPG NUMERIC ceiling ({})",
                u8::MAX
            )
            .into());
        } else {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let s = exp as u8;
            (mantissa, s)
        };
        let scaled_i128 = scaled.to_i128().ok_or_else(|| {
            format!(
                "BigDecimal mantissa {scaled} overflows i128 — SPG NUMERIC tops out at precision 38"
            )
        })?;
        Ok((scaled_i128, scale))
    }

    fn scaled_to_bigdecimal(scaled: i128, scale: u8) -> BigDecimal {
        let sign = if scaled < 0 { Sign::Minus } else { Sign::Plus };
        let magnitude: u128 = scaled.unsigned_abs();
        let mantissa = BigInt::from_biguint(sign, num_bigint::BigUint::from(magnitude));
        BigDecimal::new(mantissa, i64::from(scale))
    }
}
