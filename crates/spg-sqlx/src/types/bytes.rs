//! v7.16.0 — `Type` / `Encode` / `Decode` for `Vec<u8>` / BYTEA.

use sqlx_core::decode::Decode;
use sqlx_core::encode::{Encode, IsNull};
use sqlx_core::error::BoxDynError;
use sqlx_core::types::Type;

use spg_embedded::Value as EngineValue;

use crate::arguments::SpgArgumentValue;
use crate::database::Spg;
use crate::type_info::{Kind, SpgTypeInfo};
use crate::value::SpgValueRef;

impl Type<Spg> for [u8] {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::Bytes)
    }

    fn compatible(ty: &SpgTypeInfo) -> bool {
        matches!(ty.kind(), Kind::Bytes)
    }
}

impl Type<Spg> for Vec<u8> {
    fn type_info() -> SpgTypeInfo {
        <[u8] as Type<Spg>>::type_info()
    }

    fn compatible(ty: &SpgTypeInfo) -> bool {
        <[u8] as Type<Spg>>::compatible(ty)
    }
}

impl<'q> Encode<'q, Spg> for &'q [u8] {
    fn encode_by_ref(
        &self,
        buf: &mut Vec<SpgArgumentValue<'q>>,
    ) -> Result<IsNull, BoxDynError> {
        buf.push(SpgArgumentValue {
            value: EngineValue::Bytes((*self).to_vec()),
            type_info: Some(<[u8] as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'q> Encode<'q, Spg> for Vec<u8> {
    fn encode_by_ref(
        &self,
        buf: &mut Vec<SpgArgumentValue<'q>>,
    ) -> Result<IsNull, BoxDynError> {
        buf.push(SpgArgumentValue {
            value: EngineValue::Bytes(self.clone()),
            type_info: Some(<Vec<u8> as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Spg> for Vec<u8> {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::Bytes(b) => Ok(b.clone()),
            other => Err(format!("cannot decode {other:?} as Vec<u8> / BYTEA").into()),
        }
    }
}
