//! v7.16.0 — `Type` / `Encode` / `Decode` for serde_json::Value
//! against SPG's JSON / JSONB columns. Both PG types share the
//! same text-backed storage in SPG (text round-trip on the wire).

#![cfg(feature = "json")]

use sqlx_core::decode::Decode;
use sqlx_core::encode::{Encode, IsNull};
use sqlx_core::error::BoxDynError;
use sqlx_core::types::Type;

use spg_embedded::Value as EngineValue;

use crate::arguments::SpgArgumentValue;
use crate::database::Spg;
use crate::type_info::{Kind, SpgTypeInfo};
use crate::value::SpgValueRef;

impl Type<Spg> for serde_json::Value {
    fn type_info() -> SpgTypeInfo {
        SpgTypeInfo::of(Kind::Json)
    }
    fn compatible(ty: &SpgTypeInfo) -> bool {
        // Accept TEXT-shaped columns too — mailrs has a few
        // pre-D-pre-3 cells where JSON lives in TEXT.
        matches!(ty.kind(), Kind::Json | Kind::Text)
    }
}

impl<'q> Encode<'q, Spg> for serde_json::Value {
    fn encode_by_ref(&self, buf: &mut Vec<SpgArgumentValue<'q>>) -> Result<IsNull, BoxDynError> {
        let s = serde_json::to_string(self)?;
        buf.push(SpgArgumentValue {
            value: EngineValue::Json(s),
            type_info: Some(<serde_json::Value as Type<Spg>>::type_info()),
            _phantom: core::marker::PhantomData,
        });
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Spg> for serde_json::Value {
    fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.engine() {
            EngineValue::Json(s) | EngineValue::Text(s) => {
                serde_json::from_str(s).map_err(|e| Box::new(e) as BoxDynError)
            }
            other => Err(format!("cannot decode {other:?} as serde_json::Value").into()),
        }
    }
}
