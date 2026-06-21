//! v7.16.0 — `Type` / `Encode` / `Decode` for `bool` / BOOLEAN.

use sqlx_core::decode::Decode;
use sqlx_core::encode::{Encode, IsNull};
use sqlx_core::error::BoxDynError;
use sqlx_core::types::Type;

use spg_embedded::ValueOwned as EngineValue;

use crate::arguments::SpgArgumentValue;
use crate::database::Spg;
use crate::type_info::{Kind, SpgTypeInfo};
use crate::value::SpgValueRef;

impl Type<Spg> for bool {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::Bool)
    }

    fn compatible(ty: &SpgTypeInfo) -> bool {
        matches!(ty.kind(), Kind::Bool)
    }
}

impl<'q> Encode<'q, Spg> for bool {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        buf.push(SpgArgumentValue {
            value: EngineValue::Bool(*self),
            type_info: Some(<bool as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Spg> for bool {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::Bool(b) => Ok(*b),
            other => Err(format!("cannot decode {other:?} as bool / BOOLEAN").into()),
        }
    }
}
