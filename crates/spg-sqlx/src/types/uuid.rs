//! v7.17.0 Phase 3.P0-69 — sqlx bridge for `UUID`.
//!
//! Two surfaces land here:
//!   * `try_uuid_as_string` — unconditional fallthrough used by
//!     `types/text.rs::Decode<String>` so any sqlx caller can
//!     read UUID columns as canonical hyphenated text without
//!     enabling the `uuid` feature.
//!   * `uuid::Uuid` Encode/Decode (gated by the `uuid` feature) —
//!     full typed round-trip matching sqlx-postgres's UUID
//!     surface, so mailrs and other sqlx apps can drop in
//!     without code change.

use spg_embedded::Value as EngineValue;

#[cfg(feature = "uuid")]
use sqlx_core::decode::Decode;
#[cfg(feature = "uuid")]
use sqlx_core::error::BoxDynError;
#[cfg(feature = "uuid")]
use sqlx_core::types::Type;

#[cfg(feature = "uuid")]
use crate::database::Spg;
#[cfg(feature = "uuid")]
use crate::type_info::{Kind, SpgTypeInfo};
#[cfg(feature = "uuid")]
use crate::value::SpgValueRef;

/// Try to render a UUID cell as PG's canonical hyphenated
/// lowercase text. Returns `None` for non-UUID cells so the
/// `Decode<String>` falls through to the next attempt.
pub(crate) fn try_uuid_as_string(value: &EngineValue) -> Option<String> {
    match value {
        EngineValue::Uuid(b) => Some(spg_storage::format_uuid(b)),
        _ => None,
    }
}

#[cfg(feature = "uuid")]
mod u {
    use super::*;
    use sqlx_core::encode::{Encode, IsNull};
    use uuid::Uuid;

    use crate::arguments::SpgArgumentValue;

    impl Type<Spg> for Uuid {
        fn type_info() -> SpgTypeInfo {
            SpgTypeInfo::of(Kind::Uuid)
        }

        fn compatible(ty: &SpgTypeInfo) -> bool {
            matches!(ty.kind(), Kind::Uuid)
        }
    }

    impl<'q> Encode<'q, Spg> for Uuid {
        fn encode_by_ref(
            &self,
            buf: &mut Vec<SpgArgumentValue<'q>>,
        ) -> Result<IsNull, BoxDynError> {
            buf.push(SpgArgumentValue {
                value: EngineValue::Uuid(*self.as_bytes()),
                type_info: Some(SpgTypeInfo::of(Kind::Uuid)),
                _phantom: core::marker::PhantomData,
            });
            Ok(IsNull::No)
        }
    }

    impl<'r> Decode<'r, Spg> for Uuid {
        fn decode(value: SpgValueRef<'r>) -> Result<Self, BoxDynError> {
            match value.engine() {
                EngineValue::Uuid(b) => Ok(Uuid::from_bytes(*b)),
                // Generous decode from canonical text — covers
                // the case where a `SELECT id::text FROM t` path
                // landed at a UUID column site.
                EngineValue::Text(s) => {
                    spg_storage::parse_uuid_str(s)
                        .map(Uuid::from_bytes)
                        .ok_or_else(|| {
                            format!("cannot parse text {s:?} as uuid::Uuid").into()
                        })
                }
                other => Err(format!("cannot decode {other:?} as uuid::Uuid / UUID").into()),
            }
        }
    }
}
