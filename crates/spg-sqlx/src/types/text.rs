//! v7.16.0 — `Type` / `Encode` / `Decode` for `String` / `&str`.

use sqlx_core::decode::Decode;
use sqlx_core::encode::{Encode, IsNull};
use sqlx_core::error::BoxDynError;
use sqlx_core::types::Type;

use spg_embedded::Value as EngineValue;

use crate::arguments::SpgArgumentValue;
use crate::database::Spg;
use crate::type_info::{Kind, SpgTypeInfo};
use crate::value::SpgValueRef;

impl Type<Spg> for str {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::Text)
    }

    fn compatible(ty: &SpgTypeInfo) -> bool {
        // v7.17.0 Phase 3.P0-67 — NUMERIC cells decode to
        // canonical PG decimal text, so `String` / `&str`
        // accept them as compatible. Keeps the
        // `query_as::<_, (String,)>("SELECT amount …")` shape
        // working without forcing every caller to enable the
        // bigdecimal feature.
        matches!(ty.kind(), Kind::Text | Kind::Numeric)
    }
}

impl Type<Spg> for String {
    fn type_info() -> SpgTypeInfo {
        <str as Type<Spg>>::type_info()
    }

    fn compatible(ty: &SpgTypeInfo) -> bool {
        <str as Type<Spg>>::compatible(ty)
    }
}

impl<'q> Encode<'q, Spg> for &'q str {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        buf.push(SpgArgumentValue {
            value: EngineValue::Text((*self).to_string()),
            type_info: Some(<str as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'q> Encode<'q, Spg> for String {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        buf.push(SpgArgumentValue {
            value: EngineValue::Text(self.clone()),
            type_info: Some(<String as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Spg> for String {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        // v7.17.0 Phase 3.P0-67 — NUMERIC cells decode to their
        // canonical PG text form so the dominant
        // `query_as::<_, (String,)>` pattern works without the
        // bigdecimal feature.
        if let Some(s) = crate::types::decimal::try_numeric_as_string(value.engine()) {
            return Ok(s);
        }
        match value.engine() {
            EngineValue::Text(s) | EngineValue::Json(s) => Ok(s.clone()),
            other => Err(format!("cannot decode {other:?} as String / TEXT").into()),
        }
    }
}

impl<'r> Decode<'r, Spg> for &'r str {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::Text(s) | EngineValue::Json(s) => Ok(s.as_str()),
            other => Err(format!("cannot decode {other:?} as &str / TEXT").into()),
        }
    }
}
