//! v7.16.0 — `sqlx::TypeInfo` for SPG column types.

use std::fmt;

use sqlx_core::type_info::TypeInfo;

/// SPG column type info. Stores the concrete [`Kind`] so the
/// adapter can drive PG-shape column metadata that
/// `#[derive(FromRow)]` expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpgTypeInfo {
    kind: Kind,
}

/// Identity tag for each column type the adapter currently
/// understands. Matches the subset of `spg_storage::DataType`
/// the adapter Encode/Decode coverage extends to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Kind {
    /// `INT` / 4-byte signed integer.
    Int,
    /// `BIGINT` / 8-byte signed integer.
    BigInt,
    /// `SMALLINT` / 2-byte signed integer.
    SmallInt,
    /// `BOOLEAN`.
    Bool,
    /// `TEXT` / `VARCHAR` (text body — encoding agnostic).
    Text,
    /// `BYTEA` (raw bytes).
    Bytes,
    /// `FLOAT` (IEEE-754 double).
    Float,
    /// `DATE`.
    Date,
    /// `TIMESTAMP`.
    Timestamp,
    /// `TIMESTAMPTZ`.
    Timestamptz,
    /// `JSON` / `JSONB` (text-backed JSON).
    Json,
    /// Unknown / type-erased — used for parameters that the
    /// adapter binds without a fixed column-side type yet (e.g.
    /// the first bind of a fresh parameter index).
    Null,
}

impl SpgTypeInfo {
    /// Construct a TypeInfo for a known kind.
    #[must_use]
    pub const fn of(kind: Kind) -> Self {
        Self { kind }
    }

    /// The concrete kind tag.
    #[must_use]
    pub const fn kind(&self) -> Kind {
        self.kind
    }
}

impl TypeInfo for SpgTypeInfo {
    fn is_null(&self) -> bool {
        matches!(self.kind, Kind::Null)
    }

    fn name(&self) -> &str {
        match self.kind {
            Kind::Int => "INT",
            Kind::BigInt => "BIGINT",
            Kind::SmallInt => "SMALLINT",
            Kind::Bool => "BOOLEAN",
            Kind::Text => "TEXT",
            Kind::Bytes => "BYTEA",
            Kind::Float => "FLOAT",
            Kind::Date => "DATE",
            Kind::Timestamp => "TIMESTAMP",
            Kind::Timestamptz => "TIMESTAMPTZ",
            Kind::Json => "JSON",
            Kind::Null => "NULL",
        }
    }
}

impl fmt::Display for SpgTypeInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
