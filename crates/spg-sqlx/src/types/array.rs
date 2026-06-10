//! v7.16.0 — `Type` / `Encode` / `Decode` for the basic array
//! shapes mailrs uses: `Vec<i32>` (INT[]), `Vec<i64>` (BIGINT[]),
//! `Vec<String>` (TEXT[]). NULL elements are NOT supported on
//! Encode (mailrs's existing PG path stores Vec<T>, not
//! Vec<Option<T>>); Decode tolerates NULLs by skipping the slot.
//! Round-trip through SPG's text-form `{a,b,c}` external array
//! shape.

use sqlx_core::decode::Decode;
use sqlx_core::encode::{Encode, IsNull};
use sqlx_core::error::BoxDynError;
use sqlx_core::types::Type;

use spg_embedded::Value as EngineValue;

use crate::arguments::SpgArgumentValue;
use crate::database::Spg;
use crate::type_info::{Kind, SpgTypeInfo};
use crate::value::SpgValueRef;

// ---- [i32] + Vec<i32> / INT[] ----
//
// Encode lives on the unsized slice `[T]`; `Vec<T>` delegates and
// `&[T]` comes for free through sqlx-core's blanket `Encode for &T` +
// `Type for &T` impls (mailrs binds `= ANY($1)` params as `&[i64]`).

impl Type<Spg> for [i32] {
    fn type_info() -> SpgTypeInfo {
        // No dedicated array kind in the v7.16.0 type_info — the
        // engine stores arrays as IntArray/BigIntArray/TextArray
        // variants, dispatch happens at coerce time. Surface as
        // Text so sqlx's type-compatibility check passes for the
        // PG `INT[]` column type.
        SpgTypeInfo::of(Kind::Text)
    }
    fn compatible(_ty: &SpgTypeInfo) -> bool {
        true
    }
}

impl Type<Spg> for Vec<i32> {
    fn type_info() -> SpgTypeInfo {
        <[i32] as Type<Spg>>::type_info()
    }
    fn compatible(_ty: &SpgTypeInfo) -> bool {
        true
    }
}

impl<'q> Encode<'q, Spg> for [i32] {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        // Native engine array — `= ANY($1)` and INT[] column coerce
        // both take it directly (the prior `{1,2,3}` text form only
        // worked through column-typed coercion; ANY() has no column
        // context — mailrs embed round-12).
        buf.push(SpgArgumentValue {
            value: EngineValue::IntArray(self.iter().map(|v| Some(*v)).collect()),
            type_info: Some(<[i32] as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'q> Encode<'q, Spg> for Vec<i32> {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        self.as_slice().encode_by_ref(buf)
    }
}

// sqlx-core's blanket `Encode for &T` requires `T: Sized`, so the
// borrowed-slice form needs its own impl (delegates to `[i32]`).
impl<'q> Encode<'q, Spg> for &'q [i32] {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        (**self).encode_by_ref(buf)
    }
}

impl<'r> Decode<'r, Spg> for Vec<i32> {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::IntArray(items) => Ok(items.iter().filter_map(|o| *o).collect()),
            // BIGINT[] narrows lossily.
            EngineValue::BigIntArray(items) => Ok(items
                .iter()
                .filter_map(|o| (*o).and_then(|n| i32::try_from(n).ok()))
                .collect()),
            other => Err(format!("cannot decode {other:?} as Vec<i32>").into()),
        }
    }
}

// ---- Vec<i64> / BIGINT[] ----

impl Type<Spg> for [i64] {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::Text)
    }
    fn compatible(_ty: &SpgTypeInfo) -> bool {
        true
    }
}

impl Type<Spg> for Vec<i64> {
    fn type_info() -> SpgTypeInfo {
        <[i64] as Type<Spg>>::type_info()
    }
    fn compatible(_ty: &SpgTypeInfo) -> bool {
        true
    }
}

impl<'q> Encode<'q, Spg> for [i64] {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        buf.push(SpgArgumentValue {
            value: EngineValue::BigIntArray(self.iter().map(|v| Some(*v)).collect()),
            type_info: Some(<[i64] as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'q> Encode<'q, Spg> for Vec<i64> {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        self.as_slice().encode_by_ref(buf)
    }
}

// sqlx-core's blanket `Encode for &T` requires `T: Sized`, so the
// borrowed-slice form needs its own impl (delegates to `[i64]`).
impl<'q> Encode<'q, Spg> for &'q [i64] {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        (**self).encode_by_ref(buf)
    }
}

impl<'r> Decode<'r, Spg> for Vec<i64> {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::BigIntArray(items) => Ok(items.iter().filter_map(|o| *o).collect()),
            EngineValue::IntArray(items) => {
                Ok(items.iter().filter_map(|o| (*o).map(i64::from)).collect())
            }
            other => Err(format!("cannot decode {other:?} as Vec<i64>").into()),
        }
    }
}

// ---- Vec<String> / TEXT[] ----

impl Type<Spg> for [String] {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::Text)
    }
    fn compatible(_ty: &SpgTypeInfo) -> bool {
        true
    }
}

impl Type<Spg> for Vec<String> {
    fn type_info() -> SpgTypeInfo {
        <[String] as Type<Spg>>::type_info()
    }
    fn compatible(_ty: &SpgTypeInfo) -> bool {
        true
    }
}

/// Native engine text array (see the [i32] note — ANY() needs the
/// real array value, not the `{"a","b"}` external text form).
fn encode_text_array<'q, S: AsRef<str>>(
    items: impl Iterator<Item = S>,
    buf: &mut Vec<SpgArgumentValue<'q>>,
) -> Result<IsNull, BoxDynError> {
    buf.push(SpgArgumentValue {
        value: EngineValue::TextArray(items.map(|v| Some(v.as_ref().to_string())).collect()),
        type_info: Some(<[String] as Type<Spg>>::type_info()),
        _phantom: core::marker::PhantomData,
    });
    Ok(IsNull::No)
}

impl<'q> Encode<'q, Spg> for [String] {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        encode_text_array(self.iter(), buf)
    }
}

impl<'q> Encode<'q, Spg> for Vec<String> {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        self.as_slice().encode_by_ref(buf)
    }
}

// sqlx-core's blanket `Encode for &T` requires `T: Sized`, so the
// borrowed-slice form needs its own impl (delegates to `[String]`).
impl<'q> Encode<'q, Spg> for &'q [String] {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        (**self).encode_by_ref(buf)
    }
}

// `&[&str]` / `Vec<&str>` — borrowed text arrays bind too.

impl Type<Spg> for [&str] {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::Text)
    }
    fn compatible(_ty: &SpgTypeInfo) -> bool {
        true
    }
}

impl Type<Spg> for Vec<&str> {
    fn type_info() -> SpgTypeInfo {
        <[&str] as Type<Spg>>::type_info()
    }
    fn compatible(_ty: &SpgTypeInfo) -> bool {
        true
    }
}

impl<'q> Encode<'q, Spg> for [&str] {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        encode_text_array(self.iter(), buf)
    }
}

impl<'q> Encode<'q, Spg> for Vec<&str> {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        self.as_slice().encode_by_ref(buf)
    }
}

// sqlx-core's blanket `Encode for &T` requires `T: Sized`, so the
// borrowed-slice form needs its own impl (delegates to `[&str]`).
impl<'q> Encode<'q, Spg> for &'q [&str] {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        (**self).encode_by_ref(buf)
    }
}

impl<'r> Decode<'r, Spg> for Vec<String> {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::TextArray(items) => Ok(items.iter().filter_map(|o| o.clone()).collect()),
            other => Err(format!("cannot decode {other:?} as Vec<String>").into()),
        }
    }
}
