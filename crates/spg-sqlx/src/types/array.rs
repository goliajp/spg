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

// ---- Vec<i32> / INT[] ----

impl Type<Spg> for Vec<i32> {
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

impl<'q> Encode<'q, Spg> for Vec<i32> {
    fn encode_by_ref(
        &self,
        buf: &mut Vec<SpgArgumentValue<'q>>,
    ) -> Result<IsNull, BoxDynError> {
        // PG external form `{1,2,3}`.
        let mut s = String::from("{");
        for (i, v) in self.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&v.to_string());
        }
        s.push('}');
        buf.push(SpgArgumentValue {
            value: EngineValue::Text(s),
            type_info: Some(<Vec<i32> as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Spg> for Vec<i32> {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::IntArray(items) => {
                Ok(items.iter().filter_map(|o| o.clone()).collect())
            }
            // BIGINT[] narrows lossily.
            EngineValue::BigIntArray(items) => Ok(items
                .iter()
                .filter_map(|o| o.clone().and_then(|n| i32::try_from(n).ok()))
                .collect()),
            other => Err(format!("cannot decode {other:?} as Vec<i32>").into()),
        }
    }
}

// ---- Vec<i64> / BIGINT[] ----

impl Type<Spg> for Vec<i64> {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::Text)
    }
    fn compatible(_ty: &SpgTypeInfo) -> bool {
        true
    }
}

impl<'q> Encode<'q, Spg> for Vec<i64> {
    fn encode_by_ref(
        &self,
        buf: &mut Vec<SpgArgumentValue<'q>>,
    ) -> Result<IsNull, BoxDynError> {
        let mut s = String::from("{");
        for (i, v) in self.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&v.to_string());
        }
        s.push('}');
        buf.push(SpgArgumentValue {
            value: EngineValue::Text(s),
            type_info: Some(<Vec<i64> as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Spg> for Vec<i64> {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::BigIntArray(items) => {
                Ok(items.iter().filter_map(|o| o.clone()).collect())
            }
            EngineValue::IntArray(items) => Ok(items
                .iter()
                .filter_map(|o| o.clone().map(i64::from))
                .collect()),
            other => Err(format!("cannot decode {other:?} as Vec<i64>").into()),
        }
    }
}

// ---- Vec<String> / TEXT[] ----

impl Type<Spg> for Vec<String> {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::Text)
    }
    fn compatible(_ty: &SpgTypeInfo) -> bool {
        true
    }
}

impl<'q> Encode<'q, Spg> for Vec<String> {
    fn encode_by_ref(
        &self,
        buf: &mut Vec<SpgArgumentValue<'q>>,
    ) -> Result<IsNull, BoxDynError> {
        // PG external form `{"a","b","c"}` — quote every element
        // so commas and braces inside payloads survive.
        let mut s = String::from("{");
        for (i, v) in self.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push('"');
            for ch in v.chars() {
                if ch == '\\' || ch == '"' {
                    s.push('\\');
                }
                s.push(ch);
            }
            s.push('"');
        }
        s.push('}');
        buf.push(SpgArgumentValue {
            value: EngineValue::Text(s),
            type_info: Some(<Vec<String> as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Spg> for Vec<String> {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::TextArray(items) => {
                Ok(items.iter().filter_map(|o| o.clone()).collect())
            }
            other => Err(format!("cannot decode {other:?} as Vec<String>").into()),
        }
    }
}
