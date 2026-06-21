//! v7.16.0 — `Type` / `Encode` / `Decode` for i16 / i32 / i64.

use sqlx_core::decode::Decode;
use sqlx_core::encode::{Encode, IsNull};
use sqlx_core::error::BoxDynError;
use sqlx_core::types::Type;

use spg_embedded::ValueOwned as EngineValue;

use crate::arguments::SpgArgumentValue;
use crate::database::Spg;
use crate::type_info::{Kind, SpgTypeInfo};
use crate::value::SpgValueRef;

// ---- i32 (INT) ----

impl Type<Spg> for i32 {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::Int)
    }

    fn compatible(ty: &SpgTypeInfo) -> bool {
        matches!(ty.kind(), Kind::Int | Kind::SmallInt | Kind::BigInt)
    }
}

impl<'q> Encode<'q, Spg> for i32 {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        buf.push(SpgArgumentValue {
            value: EngineValue::Int(*self),
            type_info: Some(<i32 as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Spg> for i32 {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::Int(n) => Ok(*n),
            EngineValue::SmallInt(n) => Ok(i32::from(*n)),
            EngineValue::BigInt(n) => i32::try_from(*n).map_err(|e| Box::new(e) as BoxDynError),
            other => Err(format!("cannot decode {other:?} as i32 / INT").into()),
        }
    }
}

// ---- i64 (BIGINT) ----

impl Type<Spg> for i64 {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::BigInt)
    }

    fn compatible(ty: &SpgTypeInfo) -> bool {
        matches!(ty.kind(), Kind::BigInt | Kind::Int | Kind::SmallInt)
    }
}

impl<'q> Encode<'q, Spg> for i64 {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        buf.push(SpgArgumentValue {
            value: EngineValue::BigInt(*self),
            type_info: Some(<i64 as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Spg> for i64 {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::BigInt(n) => Ok(*n),
            EngineValue::Int(n) => Ok(i64::from(*n)),
            EngineValue::SmallInt(n) => Ok(i64::from(*n)),
            other => Err(format!("cannot decode {other:?} as i64 / BIGINT").into()),
        }
    }
}

// ---- i16 (SMALLINT) ----

impl Type<Spg> for i16 {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::SmallInt)
    }

    fn compatible(ty: &SpgTypeInfo) -> bool {
        matches!(ty.kind(), Kind::SmallInt | Kind::Int | Kind::BigInt)
    }
}

impl<'q> Encode<'q, Spg> for i16 {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        buf.push(SpgArgumentValue {
            value: EngineValue::SmallInt(*self),
            type_info: Some(<i16 as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Spg> for i16 {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::SmallInt(n) => Ok(*n),
            EngineValue::Int(n) => i16::try_from(*n).map_err(|e| Box::new(e) as BoxDynError),
            EngineValue::BigInt(n) => i16::try_from(*n).map_err(|e| Box::new(e) as BoxDynError),
            other => Err(format!("cannot decode {other:?} as i16 / SMALLINT").into()),
        }
    }
}
