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
    /// v7.17.0 — `UUID` (128-bit identifier, RFC 4122 byte order).
    /// Bridges decode into `uuid::Uuid` or `String` (canonical
    /// hyphenated lowercase form).
    Uuid,
    /// v7.17.0 Phase 3.P0-32 — `TIME` (without time zone). i64
    /// microseconds since 00:00:00. Bridges decode into `String`
    /// (canonical `HH:MM:SS[.ffffff]` form).
    Time,
    /// v7.17.0 Phase 3.P0-33 — MySQL `YEAR`. u16 in 1901..=2155
    /// plus zero-year sentinel. Bridges decode into `i32` (wire
    /// shape collapses to INT4) or `String` (4-digit zero-pad).
    Year,
    /// v7.17.0 Phase 3.P0-34 — PG `TIMETZ` (TIME WITH TIME
    /// ZONE). i64 us since 00:00:00 local + i32 offset_secs.
    /// Bridges decode into `String` (canonical
    /// `HH:MM:SS[.ffffff]±HH[:MM]`).
    TimeTz,
    /// v7.17.0 Phase 3.P0-35 — PG `MONEY` — i64 cents.
    /// Bridges decode into `String` (canonical en_US
    /// `$N,NNN.CC`) or `i64` (raw cents).
    Money,
    /// v7.17.0 Phase 3.P0-38 — PG range types (int4range /
    /// int8range / numrange / tsrange / tstzrange / daterange).
    /// Bridges decode into `String` (canonical `[a,b)`).
    Range,
    /// v7.17.0 Phase 3.P0-39 — PG `hstore` extension type.
    /// Bridges decode into `String` (canonical `"k"=>"v"`)
    /// or `HashMap<String, Option<String>>` (in the language
    /// dialect that ships the hstore feature).
    Hstore,
    /// v7.17.0 Phase 3.P0-67 — PG `NUMERIC(p, s)` / `DECIMAL(p, s)`
    /// — exact-decimal fixed-point. Stored engine-side as
    /// `(scaled: i128, scale: u8)`. Bridges decode into
    /// `bigdecimal::BigDecimal` (under the `bigdecimal`
    /// feature) or `String` (canonical PG decimal text).
    Numeric,
    /// v7.17.0 Phase 3.P0-68 — pgvector `VECTOR(N)` (any of the
    /// three storage encodings: default f32, `USING SQ8`,
    /// `USING HALF`). Bridges decode into `Vec<f32>` or
    /// `String` (canonical pgvector external form
    /// `'[1, 2.5, -3]'`). Quantised storage variants
    /// (`Sq8Vector` / `HalfVector`) dequantise to f32 at the
    /// adapter boundary.
    Vector,
    /// v7.17.0 Phase 3.P0-68 — PG `TSVECTOR` (full-text search
    /// document representation). Bridges decode into `String`
    /// (canonical PG external form
    /// `'word1':1 'word2':2,3A'`). Encode is intentionally not
    /// supported — clients build `tsvector` via the `to_tsvector`
    /// SQL function, not by binding raw lexeme lists.
    TsVector,
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

    /// v7.16.0 — translate from the engine's [`DataType`] to
    /// the adapter's typed `Kind`. Used by the fetch path to
    /// build column metadata for `SpgRow`. Any DataType the
    /// adapter hasn't bridged yet maps to `Kind::Null` —
    /// downstream Decode calls then fail with a clear
    /// "cannot decode" message identifying the column type.
    #[must_use]
    pub fn from_data_type(ty: spg_embedded::DataType) -> Self {
        use spg_embedded::DataType;
        let kind = match ty {
            DataType::Int => Kind::Int,
            DataType::BigInt => Kind::BigInt,
            DataType::SmallInt => Kind::SmallInt,
            DataType::Bool => Kind::Bool,
            DataType::Text => Kind::Text,
            DataType::Bytes => Kind::Bytes,
            DataType::Float => Kind::Float,
            DataType::Date => Kind::Date,
            DataType::Timestamp => Kind::Timestamp,
            DataType::Timestamptz => Kind::Timestamptz,
            DataType::Json => Kind::Json,
            // v7.17.0 — UUID bridges to `uuid::Uuid` (when the
            // `uuid` feature is enabled on sqlx) or to `String`.
            DataType::Uuid => Kind::Uuid,
            DataType::Time => Kind::Time,
            DataType::Year => Kind::Year,
            DataType::TimeTz => Kind::TimeTz,
            DataType::Money => Kind::Money,
            DataType::Range(_) => Kind::Range,
            DataType::Hstore => Kind::Hstore,
            // v7.17.0 Phase 3.P0-67 — NUMERIC(p, s) → exact-decimal.
            DataType::Numeric { .. } => Kind::Numeric,
            // v7.17.0 Phase 3.P0-68 — pgvector + tsvector.
            DataType::Vector { .. } => Kind::Vector,
            DataType::TsVector => Kind::TsVector,
            // v7.17.0 Phase 3.P0-40 — 2D arrays decode as TEXT
            // on the sqlx side (canonical PG nested external form).
            DataType::IntArray2D | DataType::BigIntArray2D | DataType::TextArray2D => Kind::Text,
            // v7.16.0 — DataType is #[non_exhaustive]; any
            // variant we haven't bridged yet decodes to Null
            // (so Decode impls see "compatible? no" instead of
            // a panic).
            _ => Kind::Null,
        };
        Self { kind }
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
            Kind::Uuid => "UUID",
            Kind::Time => "TIME",
            Kind::Year => "YEAR",
            Kind::TimeTz => "TIMETZ",
            Kind::Money => "MONEY",
            Kind::Range => "RANGE",
            Kind::Hstore => "HSTORE",
            Kind::Numeric => "NUMERIC",
            Kind::Vector => "VECTOR",
            Kind::TsVector => "TSVECTOR",
            Kind::Null => "NULL",
        }
    }
}

impl fmt::Display for SpgTypeInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
