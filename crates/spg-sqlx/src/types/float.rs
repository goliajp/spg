//! v7.16.0 — `Type` / `Encode` / `Decode` for f64 / f32 / FLOAT.

use sqlx_core::decode::Decode;
use sqlx_core::encode::{Encode, IsNull};
use sqlx_core::error::BoxDynError;
use sqlx_core::types::Type;

use spg_embedded::Value as EngineValue;

use crate::arguments::SpgArgumentValue;
use crate::database::Spg;
use crate::type_info::{Kind, SpgTypeInfo};
use crate::value::SpgValueRef;

impl Type<Spg> for f64 {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::Float)
    }
    fn compatible(ty: &SpgTypeInfo) -> bool {
        matches!(ty.kind(), Kind::Float)
    }
}

impl<'q> Encode<'q, Spg> for f64 {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        buf.push(SpgArgumentValue {
            value: EngineValue::Float(*self),
            type_info: Some(<f64 as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Spg> for f64 {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::Float(x) => Ok(*x),
            other => Err(format!("cannot decode {other:?} as f64 / FLOAT").into()),
        }
    }
}

impl Type<Spg> for f32 {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::Float)
    }
    fn compatible(ty: &SpgTypeInfo) -> bool {
        matches!(ty.kind(), Kind::Float)
    }
}

impl<'q> Encode<'q, Spg> for f32 {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        buf.push(SpgArgumentValue {
            value: EngineValue::Float(f64::from(*self)),
            type_info: Some(<f32 as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Spg> for f32 {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            #[allow(clippy::cast_possible_truncation)]
            EngineValue::Float(x) => Ok(*x as f32),
            other => Err(format!("cannot decode {other:?} as f32 / FLOAT").into()),
        }
    }
}
