//! v7.16.0 — `sqlx::Value` + `sqlx::ValueRef` for SPG cells.

use std::borrow::Cow;

use sqlx_core::error::BoxDynError;
use sqlx_core::value::{Value, ValueRef};

use spg_embedded::ValueOwned as EngineValue;

use crate::database::Spg;
use crate::type_info::{Kind, SpgTypeInfo};

/// Owned form of an SPG cell as it comes back from a query.
/// Wraps [`spg_embedded::Value`] + the column's static
/// [`SpgTypeInfo`] so the decode path can drive sqlx's type-
/// compatibility check (`Decode::compatible`).
#[derive(Debug, Clone)]
pub struct SpgValue {
    inner: EngineValue,
    type_info: SpgTypeInfo,
}

impl SpgValue {
    /// Wrap an engine value with its column type info.
    #[must_use]
    pub fn new(inner: EngineValue, type_info: SpgTypeInfo) -> Self {
        Self { inner, type_info }
    }

    /// Borrow the underlying engine value.
    #[must_use]
    pub const fn engine(&self) -> &EngineValue {
        &self.inner
    }
}

impl Value for SpgValue {
    type Database = Spg;

    fn as_ref(&self) -> SpgValueRef<'_> {
        SpgValueRef::new(&self.inner, self.type_info.clone())
    }

    fn type_info(&self) -> Cow<'_, SpgTypeInfo> {
        Cow::Borrowed(&self.type_info)
    }

    fn is_null(&self) -> bool {
        matches!(self.inner, EngineValue::Null)
    }
}

/// Borrowed form of an SPG cell. Returned by [`SpgRow::try_get_raw`][crate::row::SpgRow]
/// to let Decode implementations read the value without taking
/// ownership.
#[derive(Debug, Clone)]
pub struct SpgValueRef<'r> {
    inner: &'r EngineValue,
    type_info: SpgTypeInfo,
}

impl<'r> SpgValueRef<'r> {
    /// Build a borrowed cell + its column type info.
    #[must_use]
    pub fn new(inner: &'r EngineValue, type_info: SpgTypeInfo) -> Self {
        Self { inner, type_info }
    }

    /// Borrow the underlying engine value.
    #[must_use]
    pub const fn engine(&self) -> &'r EngineValue {
        self.inner
    }

    /// The column kind tag for this value.
    #[must_use]
    pub const fn kind(&self) -> Kind {
        self.type_info.kind()
    }
}

impl<'r> ValueRef<'r> for SpgValueRef<'r> {
    type Database = Spg;

    fn to_owned(&self) -> SpgValue {
        SpgValue {
            inner: self.inner.clone(),
            type_info: self.type_info.clone(),
        }
    }

    fn type_info(&self) -> Cow<'_, SpgTypeInfo> {
        Cow::Borrowed(&self.type_info)
    }

    fn is_null(&self) -> bool {
        matches!(self.inner, EngineValue::Null)
    }
}

/// Map an engine value's runtime variant to the corresponding
/// [`Kind`] tag — used when a fetched row carries cells whose
/// column metadata is not yet known to the adapter.
#[must_use]
#[allow(dead_code)]
pub fn engine_value_kind(v: &EngineValue) -> Kind {
    match v {
        EngineValue::Null => Kind::Null,
        EngineValue::SmallInt(_) => Kind::SmallInt,
        EngineValue::Int(_) => Kind::Int,
        EngineValue::BigInt(_) => Kind::BigInt,
        EngineValue::Bool(_) => Kind::Bool,
        EngineValue::Text(_) => Kind::Text,
        EngineValue::Bytes(_) => Kind::Bytes,
        EngineValue::Float(_) => Kind::Float,
        // v7.39 (round 269) — REAL rides the same Kind as FLOAT so
        // f32 / f64 stay `compatible` with a real column; the width
        // difference lives in Decode, not in the kind. Without this it
        // fell to the non-exhaustive catch-all and reported Kind::Null,
        // which fails every type check.
        EngineValue::Real(_) => Kind::Float,
        EngineValue::Date(_) => Kind::Date,
        EngineValue::Timestamp(_) => Kind::Timestamp,
        EngineValue::Json(_) => Kind::Json,
        // v7.17.0 Phase 3.P0-69 — UUID bridges to uuid::Uuid /
        // String.
        EngineValue::Uuid(_) => Kind::Uuid,
        // v7.17.0 Phase 3.P0-67 — NUMERIC bridges to bigdecimal /
        // String.
        EngineValue::Numeric { .. } => Kind::Numeric,
        // v7.17.0 Phase 3.P0-68 — pgvector (all encodings) /
        // tsvector. Sq8 + Half storage variants dequantise to
        // f32 inside Decode, so the column-side kind is still
        // Vector.
        EngineValue::Vector(_) | EngineValue::Sq8Vector(_) | EngineValue::HalfVector(_) => {
            Kind::Vector
        }
        EngineValue::TsVector(_) => Kind::TsVector,
        // v7.16.0 — non-exhaustive enum; future types
        // (Interval, arrays beyond text/int/bigint, hstore) decode
        // to Kind::Null until the corresponding Decode lands.
        _ => Kind::Null,
    }
}

/// Force a no-op use of [`BoxDynError`] so future Decode
/// impls can return an unboxed error without losing the
/// import path.
#[allow(dead_code)]
fn _box_dyn_marker() -> Option<BoxDynError> {
    None
}
