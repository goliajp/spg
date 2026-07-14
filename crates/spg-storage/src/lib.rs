//! In-memory storage primitives.
//!
//! v0.3 is intentionally simple: a flat catalog of tables, each holding rows
//! as `Vec<Value>` (positional, matching the table's `TableSchema`). No MVCC,
//! no on-disk format — those land in later milestones.
#![no_std]
// v3.3.2 NEON path for l2_distance_sq (aarch64 only). Scoped allow:
// `unsafe_code = "deny"` at workspace level stays in force for every
// other crate.
#![cfg_attr(target_arch = "aarch64", allow(unsafe_code))]

extern crate alloc;

pub mod bignum;
pub mod bloom;
mod codec;
pub mod fts_simple;
pub mod halfvec;
pub mod jsonb_gin;
mod nsw;
pub mod persistent;
pub mod persistent_btree;
pub mod quantize;
pub mod row_header;
pub mod row_locator;
pub mod segment;
pub mod snapshot;
mod table;
pub mod trgm;
pub mod vacuum;

pub use self::bloom::{BloomError, BloomFilter};
// v7.31 monster tier-3 cut 3 — on-disk codec moved to `codec`; the
// public dense-row surface keeps its `spg_storage::*` paths, and the
// low-level write/read primitives stay crate-visible for the
// `Catalog::serialize`/`deserialize` methods that remain in this file.
pub(crate) use self::codec::*;
pub use self::codec::{decode_row_body_dense, encode_row_body_dense, row_body_encoded_len};
// v7.31 monster tier-3 cut 2 — HNSW algorithms moved to `nsw`; the
// public vector-search surface keeps its `spg_storage::*` paths via
// these re-exports, and `nsw_insert_at` stays crate-visible for the
// `Table` insert paths in the `table` module.
pub(crate) use self::nsw::nsw_insert_at;
pub use self::nsw::{NswMetric, cosine_dot_norms_f32, inner_product_f32, nsw_index_on, nsw_query};
pub use self::row_locator::{RowLocator, RowLocatorError};
pub use self::segment::{
    BRIN_SIDECAR_MAGIC, BrinSummary, OwnedSegment, SEGMENT_COMPRESS_ALGO_LZSS,
    SEGMENT_COMPRESS_ALGO_NONE, SEGMENT_MAGIC, SEGMENT_MAGIC_V2, SEGMENT_PAGE_BYTES, SegmentError,
    SegmentMeta, SegmentReader, derive_brin_summaries, encode_segment, wrap_v2_envelope,
    wrap_v2_envelope_with_brin,
};

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use self::persistent::PersistentVec;
use self::persistent_btree::PersistentBTreeMap;

/// In-cell encoding for `DataType::Vector`. Mirrors
/// `spg_sql::ast::VecEncoding` — kept here so storage stays
/// dep-free of `spg-sql`. The engine bridges between the two
/// at DDL-execution time.
///
/// `F32` is the pre-v6 default: each cell holds a raw `Vec<f32>`.
/// `Sq8` (v6.0.1) stores `Sq8Vector { min, max, bytes: Vec<u8> }`
/// per cell; 4× compression vs `F32` with recall@10 ≥ 0.95 on
/// natural embeddings (Gaussian / unit-sphere corpora).
/// `F16` (v6.0.3, DDL keyword `HALF`) stores each element as
/// IEEE-754 binary16; 2× compression and bit-exact dequantise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VecEncoding {
    #[default]
    F32,
    Sq8,
    F16,
}

impl fmt::Display for VecEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::F32 => f.write_str("F32"),
            Self::Sq8 => f.write_str("SQ8"),
            Self::F16 => f.write_str("HALF"),
        }
    }
}

/// Runtime type tags. `Vector { dim, encoding }` / `Varchar(max)` /
/// `Char(size)` are parameterised; the parameter travels with both
/// the column schema and the on-wire serialised representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// 16-bit signed. Backed by `Value::SmallInt(i16)`; arithmetic that
    /// would overflow surfaces as a type error at INSERT time.
    SmallInt,
    Int,    // 32-bit signed
    BigInt, // 64-bit signed
    Float,  // f64 (PG double precision)
    /// v7.38 (read01, T-float4) — `real` / `float4`: 32-bit IEEE float (PG
    /// `real`). Backed by `Value::Real(f32)`; behaves like `Float` for most
    /// dispatch but renders / stores at f32 precision.
    Real,
    Text,
    /// `VARCHAR(n)` — same byte representation as `Text`, but INSERT
    /// rejects values longer than `n` Unicode characters.
    Varchar(u32),
    /// `CHAR(n)` — same representation as `Text`, but INSERT right-pads
    /// with U+0020 to exactly `n` Unicode characters (or rejects when
    /// the input is already longer).
    Char(u32),
    Bool,
    /// pgvector-style fixed-dimension vector. `encoding` selects
    /// the in-cell representation (`F32` = pre-v6 raw f32 buffer;
    /// `Sq8` = v6.0.1 8-bit scalar-quantised). The DDL grammar
    /// surfaces encoding via the optional `USING <encoding>`
    /// clause: `VECTOR(128) USING SQ8`.
    Vector {
        dim: u32,
        encoding: VecEncoding,
    },
    /// `NUMERIC(precision, scale)` — exact fixed-point decimal stored as
    /// a scaled `i128`. `precision` caps total decimal digits, `scale`
    /// fixes digits after the decimal point. v1.12 supports up to
    /// precision 38 (the i128-safe ceiling). `NUMERIC` and `NUMERIC(p)`
    /// surface as `Numeric { precision: p, scale: 0 }`.
    Numeric {
        precision: u8,
        scale: u8,
    },
    /// `DATE` — calendar date with day precision, stored as `i32` days
    /// since the Unix epoch (1970-01-01).
    Date,
    /// `TIMESTAMP` (a.k.a. `MySQL` `DATETIME`) — instant with microsecond
    /// precision, stored as `i64` microseconds since the Unix epoch.
    Timestamp,
    /// v7.9.2 `TIMESTAMPTZ` — bit-identical to `Timestamp` on disk
    /// (i64 microseconds, UTC by convention). Carried as a distinct
    /// type tag so the PG-wire layer can advertise OID 1184 (PG's
    /// `timestamp with time zone`) and `sqlx`/`pgx`/JDBC clients
    /// decode into their TZ-aware datetime types. The internal
    /// semantics are unchanged: SPG never stored per-row offsets,
    /// and neither did PG — `TIMESTAMPTZ` in PG is also UTC i64.
    Timestamptz,
    /// `INTERVAL` — calendar-aware span (months + microseconds). v2.11
    /// supports INTERVAL only as a runtime intermediate (literals,
    /// arithmetic results); on-disk encoding is rejected so this branch
    /// can't appear in a `ColumnSchema`.
    Interval,
    /// v4.9: `JSON` — text-backed JSON document. We don't parse
    /// the content (no path operators or jsonb functions yet) —
    /// the column accepts any TEXT-compatible value and round-trips
    /// it verbatim. PG OID 114 on the wire.
    Json,
    /// v7.9.0: `JSONB` — semantically identical to `Json` on
    /// the storage side (same `Value::Json` cells, same
    /// row codec), but advertised as PG OID 3802 on the wire
    /// so `sqlx`-style clients that bind `jsonb` columns
    /// decode correctly. mailrs migration blocker #3.
    Jsonb,
    /// v7.10.4: `BYTES` / `BYTEA` — variable-length raw binary.
    /// Backed by `Value::Bytes(Vec<u8>)`. PG wire OID 17. Literal
    /// forms accepted by parser/engine: PG hex form `'\xDEADBEEF'`
    /// (case-insensitive hex pairs) and escape form
    /// `'foo\\000bar'` (the latter decoded at coercion time when
    /// the target column is BYTEA — TEXT columns leave the
    /// backslash sequence verbatim).
    Bytes,
    /// v7.10.9: `TEXT[]` — single-dimension TEXT array. Elements
    /// may be NULL (PG semantics). PG wire OID 1009. Literal
    /// forms: `ARRAY['a', 'b', NULL]` and the PG external form
    /// `'{a,b,NULL}'::TEXT[]`. Engine implements `= ANY(arr)`,
    /// `<> ALL(arr)`, and 1-based indexing `arr[i]`. Catalog
    /// FILE_VERSION 18+; older snapshots reject this DataType
    /// (forward-only by design — TEXT[] columns aren't readable
    /// on a pre-v7.10 binary).
    TextArray,
    /// v7.11.12: `INT[]` — single-dimension i32 array. PG wire
    /// OID 1007 (_int4). Same `ARRAY[...]` / `'{1,2,3}'::INT[]`
    /// literal surface as TEXT[]. Catalog FILE_VERSION 19+.
    IntArray,
    /// v7.11.12: `BIGINT[]` — single-dimension i64 array. PG
    /// wire OID 1016 (_int8). Catalog FILE_VERSION 19+.
    BigIntArray,
    /// v7.37.5 β-P4 — `INTERVAL[]` — single-dimension array of
    /// `IntervalSpan { months, days, micros }`. PG wire OID 1187
    /// (`_interval`). Catalog tag 35 + per-cell body
    /// `[u16 count][per elem: u8 null + (if non-null) 16-byte
    /// interval body in LE PG-byte-equal field order]`.
    /// FILE_VERSION 48+.
    IntervalArray,
    /// v7.37.5 γ — full PG array-of-scalar family. Catalog tags
    /// 36..48; wire OIDs from PG `pg_type.dat`. Per-element body
    /// uses the scalar's existing `write_value_body` shape.
    /// FILE_VERSION 48+ (same window as β; no separate bump).
    BoolArray, // PG `_bool`        OID 1000, tag 36
    SmallIntArray,    // PG `_int2`        OID 1005, tag 37
    FloatArray,       // PG `_float8`      OID 1022, tag 38
    NumericArray,     // PG `_numeric`     OID 1231, tag 39
    DateArray,        // PG `_date`        OID 1182, tag 40
    TimestampArray,   // PG `_timestamp`   OID 1115, tag 41
    TimestamptzArray, // PG `_timestamptz` OID 1185, tag 42
    UuidArray,        // PG `_uuid`        OID 2951, tag 43
    JsonArray,        // PG `_json`        OID 199,  tag 44
    JsonbArray,       // PG `_jsonb`       OID 3807, tag 45
    BytesArray,       // PG `_bytea`       OID 1001, tag 46
    VarcharArray,     // PG `_varchar`     OID 1015, tag 47
    CharArray,        // PG `_bpchar`      OID 1014, tag 48
    /// v7.37.5 δ — PG 14+ multirange types. A multirange is an
    /// ordered collection of non-overlapping ranges of the same
    /// element kind (e.g. `int4multirange(int4range(1,5),
    /// int4range(10,15))` → `{[1,5),[10,15)}`). The same DataType
    /// variant covers all six builtin multiranges; `RangeKind`
    /// pins the element type so encode/decode/display can route
    /// off one switch (parallel to `Range(RangeKind)`).
    /// Wire OIDs: int4multirange=4451, int8multirange=4537,
    /// nummultirange=4536, tsmultirange=4533, tstzmultirange=4534,
    /// datemultirange=4535. Catalog tag 49 + 1-byte RangeKind on
    /// the dense type-tag side. FILE_VERSION 48+ (same window as
    /// β/γ, no separate bump).
    Multirange(RangeKind),
    /// v7.37.5 ε — PG geometry scalar family. Mirrors PG's seven
    /// builtin geometric types one-for-one. Body shapes (LE):
    ///   Point   = 16 B fixed (f64 x + f64 y)            OID 600
    ///   Lseg    = 32 B fixed (Point p1 + Point p2)      OID 601
    ///   Path    = varlena ([u8 closed][u32 n][Point*n]) OID 602
    ///   Box     = 32 B fixed (Point ur + Point ll)      OID 603
    ///   Polygon = varlena ([u32 n][Point*n])            OID 604
    ///   Line    = 24 B fixed (f64 a + f64 b + f64 c)    OID 628
    ///   Circle  = 24 B fixed (Point center + f64 r)     OID 718
    /// Catalog tags 50..56. FILE_VERSION 48+ (same window as β/γ/δ;
    /// no separate bump). Geometric operators (`<->` / `@>` / `&&`
    /// / `<<` / `>>` / `~=`) are a planner-integration follow-up,
    /// parallel to the Range operator defer in e2e_pg_range.rs.
    Point,
    Lseg,
    Path,
    PgBox,
    Polygon,
    Line,
    Circle,
    /// v7.37.5 ζ-A — PG network address family. Body shapes (LE):
    ///   Inet     = 18 B fixed (u8 family + u8 bits + 16 B addr)  OID 869
    ///   Cidr     = 18 B fixed (same shape as Inet; CIDR rejects
    ///                          host bits at parse / coerce)       OID 650
    ///   Macaddr  = 6 B fixed                                      OID 829
    ///   Macaddr8 = 8 B fixed (EUI-64)                             OID 774
    /// Catalog tags 57-60. FILE_VERSION 48+. `family = 4` is IPv4
    /// (uses the first 4 bytes of the 16-B addr slot, rest 0);
    /// `family = 6` is IPv6 (full 16 B).
    Inet,
    Cidr,
    Macaddr,
    Macaddr8,
    /// v7.39 (read01 pg_lsn.c) — PG `pg_lsn` (WAL location). 8 bytes,
    /// rendered `%X/%X`. Catalog tag 66. OID 3220.
    PgLsn,
    /// v7.37.5 ζ-A — PG bit string. Body = `[u32 nbits][ceil(nbits/8) bytes]`,
    /// big-endian within each byte (matches PG binary).
    ///   Bit         OID 1560 (fixed-length, but SPG carries the
    ///                         length per cell — column declaration
    ///                         `BIT(n)` constrains at coerce time)
    ///   BitVarying  OID 1562 (variable-length, declared as `VARBIT`)
    /// Catalog tags 61-62.
    Bit,
    BitVarying,
    /// v7.37.5 ζ-A — PG `xml`. Body identical to TEXT (storage is
    /// the verbatim XML string; no parse-time validation). Only
    /// the wire OID (142) differs. Catalog tag 63.
    Xml,
    /// v7.37.5 ζ-A — PG `"char"` (the internal single-byte type,
    /// distinct from `CHAR(n)` / `BPCHAR`). Body = 1 byte raw.
    /// OID 18. Catalog tag 64.
    Char1,
    /// v7.37.5 ζ-A — `MONEY[]`. Body = `[u16 count][per elem: u8 null
    /// + (non-null) i64 LE cents]`. OID 791. Catalog tag 65.
    MoneyArray,
    /// v7.12.0: PG `tsvector` — ordered, deduplicated set of
    /// `(lexeme, positions, weight)` tuples. PG wire OID 3614.
    /// Catalog FILE_VERSION 20+. Storage shape is row-codec
    /// tag 22; the schema-agnostic `write_value` path emits tag
    /// 18. Literal: `'foo:1 bar:2,3'::tsvector` (PG external
    /// form). G-CRIT-3 entry — v7.12.0 only ships the type +
    /// codec; matching `@@` lands in v7.12.2.
    TsVector,
    /// v7.12.0: PG `tsquery` — parse tree of lexemes joined by
    /// `&` `|` `!` and phrase operators. PG wire OID 3615.
    /// Catalog FILE_VERSION 20+.
    TsQuery,
    /// v7.17.0: PG `uuid` — 128-bit identifier stored as
    /// `Value::Uuid([u8; 16])`. PG wire OID 2950. Canonical
    /// text form is lowercase 8-4-4-4-12 hyphenated; input
    /// also accepts uppercase, unhyphenated, and brace-wrapped
    /// forms (`{xxxx…}`). Catalog FILE_VERSION 36+; tag 24 on
    /// the dense type-tag side, tag 20 on the schema-agnostic
    /// value side. The drop-in PG/MySQL surface for Django /
    /// Rails / Hibernate "id UUID PRIMARY KEY DEFAULT
    /// gen_random_uuid()" default-PK pattern.
    Uuid,
    /// v7.17.0 Phase 3.P0-32: PG `time` (without time zone) — i64
    /// microseconds since 00:00:00. PG wire OID 1083. Display:
    /// canonical zero-padded `HH:MM:SS` when fractional is zero,
    /// `HH:MM:SS.ffffff` otherwise. Catalog FILE_VERSION 37+;
    /// tag 25 on the dense type-tag side, tag 21 on the schema-
    /// agnostic value side. The wall-clock-of-day half of PG's
    /// date/time triplet (date / time / timestamp).
    Time,
    /// v7.17.0 Phase 3.P0-33: MySQL `YEAR` — u16 in range
    /// 1901..=2155 plus the special zero-year sentinel 0. No
    /// dedicated PG OID (advertised as INT4 / OID 23 on the wire
    /// — psql renders integers, MySQL CLI renders 4-digit
    /// zero-padded text). Display always 4 digits: `0000` for the
    /// zero-year, `1985` / `2007` / etc otherwise. Catalog
    /// FILE_VERSION 38+; tag 26 on the dense type-tag side, tag
    /// 22 on the schema-agnostic value side.
    Year,
    /// v7.17.0 Phase 3.P0-34: PG `time with time zone` (TIMETZ) —
    /// i64 microseconds since 00:00:00 in the local wall clock
    /// PLUS i32 offset-from-UTC in seconds. PG wire OID 1266.
    /// Display: `HH:MM:SS[.ffffff]±HH[:MM]` (PG `timetz_out`).
    /// Range: offset in ±50400 seconds (±14 hours). Catalog
    /// FILE_VERSION 39+; tag 27 on the dense type-tag side, tag
    /// 23 on the schema-agnostic value side.
    TimeTz,
    /// v7.17.0 Phase 3.P0-35: PG `money` — i64 cents (locale-
    /// independent storage). PG wire OID 790. Display: en_US
    /// locale (`$N,NNN.CC`, negative → `-$1.23`). Input accepts
    /// `$N.NN`, `$N,NNN.NN`, bare integer (treated as major
    /// units), optional leading `-`. Range: full i64. Catalog
    /// FILE_VERSION 40+; tag 28 on the dense type-tag side, tag
    /// 24 on the schema-agnostic value side.
    Money,
    /// v7.17.0 Phase 3.P0-38: PG range type. The same DataType
    /// variant covers all six builtin ranges (int4range,
    /// int8range, numrange, tsrange, tstzrange, daterange) —
    /// `RangeKind` pins the element type so encode / decode /
    /// display can route off one switch. Catalog FILE_VERSION
    /// 43+; tag 29 + a 1-byte RangeKind on the dense type-tag
    /// side, tag 25 on the schema-agnostic value side.
    Range(RangeKind),
    /// v7.17.0 Phase 3.P0-39: PG `hstore` extension type — flat
    /// `text => text` map with NULL value support. Catalog
    /// FILE_VERSION 44+; tag 30 on the dense type-tag side, tag
    /// 26 on the schema-agnostic value side. The contrib OID is
    /// installation-dependent in real PG; SPG advertises it via
    /// dynamic lookup, falling back to TEXT (OID 25) on the wire
    /// when the installed `hstore` extension hasn't claimed an
    /// OID yet.
    Hstore,
    /// v7.17.0 Phase 3.P0-40: PG `int[][]` — 2-dimensional INT
    /// matrix. Storage: row-major Vec<Vec<Option<i32>>>. All
    /// rows must share the same column count. Wire OID 1007
    /// (same as INT[]; the dimension count travels in the data
    /// header, not the OID). Catalog FILE_VERSION 45+; tag 31
    /// on the dense type-tag side, tag 27 on the schema-agnostic
    /// value side.
    IntArray2D,
    /// v7.17.0 Phase 3.P0-40: PG `bigint[][]` — 2-dimensional
    /// BIGINT matrix. Storage / OID / tags mirror IntArray2D.
    /// Tag 32 dense, tag 28 schema-agnostic.
    BigIntArray2D,
    /// v7.17.0 Phase 3.P0-40: PG `text[][]` — 2-dimensional TEXT
    /// matrix. Storage: row-major Vec<Vec<Option<String>>>.
    /// Tag 33 dense, tag 29 schema-agnostic.
    TextArray2D,
}

/// v7.17.0 Phase 3.P0-38 — pins the element type of a range value
/// or column. Wire OIDs: Int4=3904, Int8=3926, Num=3906,
/// Ts=3908, TsTz=3910, Date=3912.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RangeKind {
    Int4,
    Int8,
    Num,
    Ts,
    TsTz,
    Date,
}

impl RangeKind {
    pub const fn tag(self) -> u8 {
        match self {
            Self::Int4 => 0,
            Self::Int8 => 1,
            Self::Num => 2,
            Self::Ts => 3,
            Self::TsTz => 4,
            Self::Date => 5,
        }
    }
    pub const fn from_tag(t: u8) -> Option<Self> {
        Some(match t {
            0 => Self::Int4,
            1 => Self::Int8,
            2 => Self::Num,
            3 => Self::Ts,
            4 => Self::TsTz,
            5 => Self::Date,
            _ => return None,
        })
    }
    pub const fn keyword(self) -> &'static str {
        match self {
            Self::Int4 => "INT4RANGE",
            Self::Int8 => "INT8RANGE",
            Self::Num => "NUMRANGE",
            Self::Ts => "TSRANGE",
            Self::TsTz => "TSTZRANGE",
            Self::Date => "DATERANGE",
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SmallInt => f.write_str("SMALLINT"),
            Self::Int => f.write_str("INT"),
            Self::BigInt => f.write_str("BIGINT"),
            Self::Float => f.write_str("FLOAT"),
            Self::Real => f.write_str("REAL"),
            Self::Text => f.write_str("TEXT"),
            Self::Varchar(n) => write!(f, "VARCHAR({n})"),
            Self::Char(n) => write!(f, "CHAR({n})"),
            Self::Bool => f.write_str("BOOL"),
            Self::Vector { dim, encoding } => match encoding {
                VecEncoding::F32 => write!(f, "VECTOR({dim})"),
                VecEncoding::Sq8 => write!(f, "VECTOR({dim}) USING SQ8"),
                VecEncoding::F16 => write!(f, "VECTOR({dim}) USING HALF"),
            },
            Self::Numeric { precision, scale } => {
                if *scale == 0 {
                    write!(f, "NUMERIC({precision})")
                } else {
                    write!(f, "NUMERIC({precision}, {scale})")
                }
            }
            Self::Date => f.write_str("DATE"),
            Self::Timestamp => f.write_str("TIMESTAMP"),
            Self::Timestamptz => f.write_str("TIMESTAMPTZ"),
            Self::Interval => f.write_str("INTERVAL"),
            Self::Json => f.write_str("JSON"),
            Self::Jsonb => f.write_str("JSONB"),
            Self::Bytes => f.write_str("BYTEA"),
            Self::TextArray => f.write_str("TEXT[]"),
            Self::IntArray => f.write_str("INT[]"),
            Self::BigIntArray => f.write_str("BIGINT[]"),
            Self::IntervalArray => f.write_str("INTERVAL[]"),
            Self::BoolArray => f.write_str("BOOL[]"),
            Self::SmallIntArray => f.write_str("SMALLINT[]"),
            Self::FloatArray => f.write_str("FLOAT[]"),
            Self::NumericArray => f.write_str("NUMERIC[]"),
            Self::DateArray => f.write_str("DATE[]"),
            Self::TimestampArray => f.write_str("TIMESTAMP[]"),
            Self::TimestamptzArray => f.write_str("TIMESTAMPTZ[]"),
            Self::UuidArray => f.write_str("UUID[]"),
            Self::JsonArray => f.write_str("JSON[]"),
            Self::JsonbArray => f.write_str("JSONB[]"),
            Self::BytesArray => f.write_str("BYTEA[]"),
            Self::VarcharArray => f.write_str("VARCHAR[]"),
            Self::CharArray => f.write_str("CHAR[]"),
            Self::Multirange(k) => f.write_str(match k {
                RangeKind::Int4 => "INT4MULTIRANGE",
                RangeKind::Int8 => "INT8MULTIRANGE",
                RangeKind::Num => "NUMMULTIRANGE",
                RangeKind::Ts => "TSMULTIRANGE",
                RangeKind::TsTz => "TSTZMULTIRANGE",
                RangeKind::Date => "DATEMULTIRANGE",
            }),
            Self::Point => f.write_str("POINT"),
            Self::Lseg => f.write_str("LSEG"),
            Self::Path => f.write_str("PATH"),
            Self::PgBox => f.write_str("BOX"),
            Self::Polygon => f.write_str("POLYGON"),
            Self::Line => f.write_str("LINE"),
            Self::Circle => f.write_str("CIRCLE"),
            Self::Inet => f.write_str("INET"),
            Self::Cidr => f.write_str("CIDR"),
            Self::Macaddr => f.write_str("MACADDR"),
            Self::Macaddr8 => f.write_str("MACADDR8"),
            Self::PgLsn => f.write_str("PG_LSN"),
            Self::Bit => f.write_str("BIT"),
            Self::BitVarying => f.write_str("VARBIT"),
            Self::Xml => f.write_str("XML"),
            Self::Char1 => f.write_str("\"char\""),
            Self::MoneyArray => f.write_str("MONEY[]"),
            Self::TsVector => f.write_str("TSVECTOR"),
            Self::TsQuery => f.write_str("TSQUERY"),
            Self::Uuid => f.write_str("UUID"),
            Self::Time => f.write_str("TIME"),
            Self::Year => f.write_str("YEAR"),
            Self::TimeTz => f.write_str("TIMETZ"),
            Self::Money => f.write_str("MONEY"),
            Self::Range(k) => f.write_str(k.keyword()),
            Self::Hstore => f.write_str("HSTORE"),
            Self::IntArray2D => f.write_str("INT[][]"),
            Self::BigIntArray2D => f.write_str("BIGINT[][]"),
            Self::TextArray2D => f.write_str("TEXT[][]"),
        }
    }
}

/// v7.12.0 — one entry in a `Value::TsVector`. The lexeme is the
/// (already-tokenised + stemmed in v7.12.1+) word; `positions` is
/// a strictly-ascending list of 1-based positions; `weight` is the
/// PG weight letter (A=3, B=2, C=1, D=0) — v7.12.0 defaults every
/// lexeme to D, the v7.12.2 ranking path consumes the weight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TsLexeme {
    pub word: String,
    pub positions: Vec<u16>,
    pub weight: u8,
}

/// v7.12.0 — parse tree for a PG `tsquery`. v7.12.0 ships the
/// type + codec only; the `to_tsquery` / `plainto_tsquery` lexer
/// lands in v7.12.1 and the `@@` evaluator in v7.12.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TsQueryAst {
    /// Single lexeme term. The `weight_mask` is the PG-style
    /// bitmask of accepted weights (`A=1<<3`, `B=1<<2`, `C=1<<1`,
    /// `D=1<<0`); `0` = any weight. v7.12.0 always sets it to 0.
    Term {
        word: String,
        weight_mask: u8,
    },
    And(Box<TsQueryAst>, Box<TsQueryAst>),
    Or(Box<TsQueryAst>, Box<TsQueryAst>),
    Not(Box<TsQueryAst>),
    /// `phrase <distance> phrase`. v7.12.0 only persists this; the
    /// match semantics arrive in v7.12.2 alongside `@@`.
    Phrase {
        left: Box<TsQueryAst>,
        right: Box<TsQueryAst>,
        distance: u16,
    },
}

/// A row-cell value, including SQL `NULL`. `Float` uses `f64`; NaN compares
/// non-equal to itself (PG behaviour) — `PartialEq` is derived so callers
/// must opt into NaN-aware comparison if they need stronger guarantees.
///
/// v7.37.42-arena Phase 1: parameterised on `'arena` so heap-bearing
/// variants (Text/Json/Xml/Bytes/Vector/BitString.bytes) can borrow from
/// a per-query bump arena (`Cow::Borrowed(&'arena ...)`). Persistent /
/// catalog Values use `Value<'static>` (alias `ValueOwned`) with
/// `Cow::Owned(...)`. Phase 1 keeps Range/Multirange recursive `Box<Value>`
/// at `'static` (owned) — arena migration deferred to a later phase.
/// Array-of-Option<String> variants (TextArray etc.) also stay owned in
/// Phase 1; their nested shape is awkward for the simple Cow lift and the
/// SCALARSQ hot path doesn't touch them.
/// v7.38 (read01, T6) — the IEEE-style class of a NUMERIC value. `Finite` is the
/// ordinary fixed-point case; the specials mirror PG's `'NaN'` / `'Infinity'` /
/// `'-Infinity'`. Derived `PartialEq` gives `NaN == NaN` — correct for NUMERIC
/// (unlike float's NaN ≠ NaN); the total order (`-Inf < finite < +Inf < NaN`)
/// lives in the comparison paths, not in `Ord`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum NumericKind {
    #[default]
    Finite,
    NaN,
    PosInf,
    NegInf,
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Value<'arena> {
    SmallInt(i16),
    Int(i32),
    BigInt(i64),
    Float(f64),
    /// v7.38 (read01, T-float4) — PG `real` (32-bit IEEE float).
    Real(f32),
    Text(Cow<'arena, str>),
    Bool(bool),
    Vector(Cow<'arena, [f32]>),
    /// v6.0.1: 8-bit scalar-quantised vector cell. Lives in
    /// columns declared `VECTOR(N) USING SQ8`. Layout per cell:
    /// `Sq8Vector { min: f32, max: f32, bytes: Vec<u8> }` —
    /// 4× compression vs `Vector(Vec<f32>)`. The wire layer
    /// dequantises to `f32` on SELECT; INSERT path quantises
    /// incoming `Vector(Vec<f32>)` cells into this variant.
    Sq8Vector(crate::quantize::Sq8Vector),
    /// v6.0.3: IEEE-754 binary16 vector cell. Lives in columns
    /// declared `VECTOR(N) USING HALF`. Stores raw u16 LE bits
    /// (2× compression vs `Vector(Vec<f32>)`). Wire / display
    /// paths dequantise to f32 bit-exactly; INSERT path converts
    /// incoming f32 vectors at the engine boundary.
    HalfVector(crate::halfvec::HalfVector),
    /// Exact fixed-point decimal. `scaled` holds the value as
    /// `actual * 10^scale` so the storage type is always integral —
    /// arithmetic never falls back to floating-point. v7.38 (read01, T6) —
    /// `kind` classifies the value as finite (the common case, using
    /// `scaled`/`scale`) or one of PG's NUMERIC specials (NaN / ±Infinity),
    /// which ignore `scaled`/`scale` (canonicalized to 0).
    Numeric {
        scaled: i128,
        scale: u8,
        kind: NumericKind,
    },
    /// v7.38 (read01, T3) — an exact NUMERIC whose mantissa overflows `i128`
    /// (PG's NUMERIC is unbounded). Boxed so the common finite case keeps its
    /// small footprint; specials never take this form (they stay `Numeric`).
    NumericBig(alloc::boxed::Box<crate::bignum::BigNumeric>),
    /// Days since the Unix epoch (1970-01-01). Negative for earlier dates.
    Date(i32),
    /// Microseconds since the Unix epoch (1970-01-01T00:00:00Z).
    Timestamp(i64),
    /// Calendar span: `months` + `days` + `micros`. Three fields are
    /// required for PG byte-equal: `'1 day'` ≠ `'24 hours'` (DST,
    /// month-boundary, and the on-wire `pg_type` `interval` are all
    /// `i64 micros + i32 days + i32 months`). v7.37.5 β widened from
    /// `{months, micros}`; column storage lands in the same window.
    Interval {
        months: i32,
        days: i32,
        micros: i64,
    },
    /// v4.9 `JSON` — raw JSON text. No structural validation
    /// happens at the storage layer; whatever the parser hands us
    /// round-trips verbatim. Equality is byte-wise.
    Json(Cow<'arena, str>),
    /// v7.10.4 `BYTEA` — raw binary blob. Equality is byte-wise.
    /// Layout matches `Text`'s length-prefixed shape (`[u32 LE
    /// len][bytes]`) under tag 18; the engine accepts PG hex
    /// literals (`'\xDEADBEEF'`) and escape literals at the
    /// coercion boundary.
    Bytes(Cow<'arena, [u8]>),
    /// v7.10.9 `TEXT[]` — single-dimension TEXT array with
    /// optional NULL elements. Equality is element-wise. PG's
    /// NULL-element comparison semantics: NULL ≠ NULL inside
    /// arrays under `=`, so `[NULL] != [NULL]` (the engine
    /// honours this).
    TextArray(Vec<Option<String>>),
    /// v7.11.12 `INT[]` — single-dimension i32 array with optional
    /// NULL elements. Codec mirrors TextArray with i32 LE per
    /// element instead of length-prefixed UTF-8.
    IntArray(Vec<Option<i32>>),
    /// v7.11.12 `BIGINT[]` — single-dimension i64 array with optional
    /// NULL elements.
    BigIntArray(Vec<Option<i64>>),
    /// v7.37.5 β-P4 `INTERVAL[]` — single-dimension array of
    /// `IntervalSpan { months, days, micros }` with optional NULL
    /// elements. PG external form quotes each non-NULL element
    /// (`{"1 day","24:00:00",NULL}`) because interval text contains
    /// spaces and colons. Storage codec follows the BigIntArray
    /// shape with a 16-byte per-element body.
    IntervalArray(Vec<Option<IntervalSpan>>),
    /// v7.37.5 γ — single-dimension arrays of the remaining PG
    /// scalar types. Each carries `Vec<Option<T>>` with the
    /// scalar's natural Rust shape; element NULLs are first-class
    /// (per PG: `{1,NULL,3}` is a 3-element array, not a 2-element
    /// one). Codec follows the IntervalArray shape — `[u16 count]
    /// [per elem: u8 null + (non-null) scalar body]`.
    BoolArray(Vec<Option<bool>>),
    SmallIntArray(Vec<Option<i16>>),
    FloatArray(Vec<Option<f64>>),
    /// PG `NUMERIC[]` — `(scaled: i128, scale: u8)` per element.
    NumericArray(Vec<Option<(i128, u8)>>),
    DateArray(Vec<Option<i32>>),
    TimestampArray(Vec<Option<i64>>),
    TimestamptzArray(Vec<Option<i64>>),
    UuidArray(Vec<Option<[u8; 16]>>),
    JsonArray(Vec<Option<String>>),
    JsonbArray(Vec<Option<String>>),
    BytesArray(Vec<Option<Vec<u8>>>),
    VarcharArray(Vec<Option<String>>),
    CharArray(Vec<Option<String>>),
    /// v7.37.5 δ — PG 14+ multirange. `ranges` is a Vec of
    /// non-overlapping bounds spans of the shared `kind`. PG's
    /// canonical text form is `{[a,b),[c,d),...}` (comma-separated
    /// ranges in braces; `{}` for the empty multirange). SPG's
    /// constructor enforces no overlap/coalescing — for now the
    /// engine trusts the caller (mirrors PG's `_construct_array`
    /// pattern). Catalog tag 49 + 1-byte RangeKind on the dense
    /// type-tag side; schema-less path is unreachable (multirange
    /// is column-typed only).
    Multirange {
        kind: RangeKind,
        ranges: Vec<RangeSpan>,
    },
    /// v7.37.5 ε — PG geometry scalars. Per-type Vec/struct shape;
    /// codec body shape is described on the matching DataType
    /// variant. PG canonical text forms:
    ///   Point   `(x,y)`
    ///   Lseg    `[(x1,y1),(x2,y2)]`
    ///   Path    open `[(x,y),(x,y),...]` / closed `((x,y),(x,y),...)`
    ///   Box     `(ux,uy),(lx,ly)` (PG normalises to upper-right + lower-left)
    ///   Polygon `((x,y),(x,y),...)` (implicit closed)
    ///   Line    `{a,b,c}` (Ax + By + C = 0)
    ///   Circle  `<(x,y),r>`
    Point(Point2D),
    Lseg(Point2D, Point2D),
    /// `closed = true` is `((p,p,...))`; `false` is `[(p,p,...)]`.
    Path {
        points: Vec<Point2D>,
        closed: bool,
    },
    /// PG `box` — stored as `(upper_right, lower_left)` (PG's
    /// normalised order). The engine accepts both endpoint
    /// orderings at parse time and normalises here.
    PgBox(Point2D, Point2D),
    Polygon(Vec<Point2D>),
    Line {
        a: f64,
        b: f64,
        c: f64,
    },
    Circle {
        center: Point2D,
        radius: f64,
    },
    /// v7.37.5 ζ-A — PG `inet`. `family = 4` (IPv4) or `6` (IPv6).
    /// `bits` is the netmask bit count (0..=32 for IPv4, 0..=128
    /// for IPv6). `addr` is right-padded with zeros when family=4
    /// (first 4 bytes are the address).
    Inet {
        family: u8,
        bits: u8,
        addr: [u8; 16],
    },
    /// v7.37.5 ζ-A — PG `cidr`. Same shape as Inet; CIDR's
    /// invariant (host bits zero) is enforced at parse / coerce.
    Cidr {
        family: u8,
        bits: u8,
        addr: [u8; 16],
    },
    /// v7.37.5 ζ-A — PG `macaddr`. 6 bytes (XX:XX:XX:XX:XX:XX).
    Macaddr([u8; 6]),
    /// v7.37.5 ζ-A — PG `macaddr8`. 8 bytes (EUI-64).
    Macaddr8([u8; 8]),
    /// v7.39 (read01 pg_lsn.c) — PG `pg_lsn`, a 64-bit WAL location.
    PgLsn(u64),
    /// v7.39 (read01 ruleutils.c) — PG `regclass`: an OID-typed relation
    /// reference that renders as the relation name. SPG carries BOTH
    /// (the synthetic oid for catalog joins, the name for display) so
    /// `conrelid = 't'::regclass` and `'t'::regclass::text` agree.
    /// Eval-only (no column storage).
    RegClass(i64, alloc::boxed::Box<str>),
    /// v7.37.5 ζ-A — PG `bit` / `bit varying`. `nbits` is the
    /// actual bit count; `bytes` is the packed representation
    /// (big-endian within each byte; final byte right-padded
    /// with 0s if `nbits % 8 != 0`).
    BitString {
        nbits: u32,
        bytes: Cow<'arena, [u8]>,
    },
    /// v7.37.5 ζ-A — PG `xml`. Stored verbatim as a string; no
    /// parse-time validation (matches the SPG JSON convention).
    Xml(Cow<'arena, str>),
    /// v7.37.5 ζ-A — PG `"char"` (internal single-byte type,
    /// distinct from CHAR(n)).
    Char1(u8),
    /// v7.38 (read01, T11) — PG `bpchar` / CHAR(n): blank-padded fixed-length
    /// string. Stored space-padded to the declared width (as PG does + for wire
    /// display); length / comparison / ::text / concat all ignore the trailing
    /// blanks (handled at those sites).
    BpChar(Cow<'arena, str>),
    /// v7.37.5 ζ-A — PG `money[]`.
    MoneyArray(Vec<Option<i64>>),
    /// v7.12.0 `tsvector` — sorted-by-word, deduped lexeme set with
    /// positions + weights. The engine enforces sort/dedup on
    /// construction; consumers can rely on `lexemes.windows(2)`
    /// being strictly ascending by `word`.
    TsVector(Vec<TsLexeme>),
    /// v7.12.0 `tsquery` — boolean / phrase parse tree over
    /// lexemes. Engine builds via `to_tsquery` family.
    TsQuery(TsQueryAst),
    /// v7.17.0 `uuid` — 128-bit identifier. Stored as 16 bytes
    /// (big-endian / network-byte order, same as RFC 4122).
    /// Display normalises to canonical lowercase 8-4-4-4-12
    /// hyphenated form. Equality is byte-wise.
    Uuid([u8; 16]),
    /// v7.17.0 Phase 3.P0-32 — PG `time` (without time zone) —
    /// i64 microseconds since 00:00:00. Range 0..86_400_000_000.
    /// Display: `HH:MM:SS` zero-padded, with optional `.ffffff`
    /// suffix when fractional is non-zero.
    Time(i64),
    /// v7.17.0 Phase 3.P0-33 — MySQL `YEAR` — u16 in range
    /// 1901..=2155 plus the special zero-year sentinel 0.
    /// Display always 4 digits zero-padded (`0000` for the
    /// sentinel; `1985`/`2007` otherwise).
    Year(u16),
    /// v7.17.0 Phase 3.P0-34 — PG `time with time zone` — i64
    /// microseconds since 00:00:00 in the LOCAL wall clock PLUS
    /// an i32 offset-from-UTC in seconds. PG preserves the
    /// offset on output, so the wall-clock value is NOT shifted
    /// to UTC at storage time. Offset range: ±50400 seconds
    /// (±14 hours).
    TimeTz {
        us: i64,
        offset_secs: i32,
    },
    /// v7.17.0 Phase 3.P0-35 — PG `money` — i64 cents
    /// (locale-independent storage; the en_US locale renders on
    /// display via `$N,NNN.CC`).
    Money(i64),
    /// v7.17.0 Phase 3.P0-39 — PG `hstore` value: flat
    /// `text => text` map with NULL value support. Insertion
    /// order preserved on input; duplicate keys take last-write-
    /// wins at parse time.
    Hstore(Vec<(String, Option<String>)>),
    /// v7.17.0 Phase 3.P0-40 — 2D INT matrix (row-major).
    IntArray2D(Vec<Vec<Option<i32>>>),
    /// v7.17.0 Phase 3.P0-40 — 2D BIGINT matrix (row-major).
    BigIntArray2D(Vec<Vec<Option<i64>>>),
    /// v7.17.0 Phase 3.P0-40 — 2D TEXT matrix (row-major).
    TextArray2D(Vec<Vec<Option<String>>>),
    /// v7.17.0 Phase 3.P0-38 — PG range value. One shape covers
    /// all six builtin range types; `kind` pins the element type
    /// (must match the column's `DataType::Range(kind)`).
    /// `lower` / `upper` are `None` for the unbounded sides;
    /// `lower_inc` / `upper_inc` mirror the canonical PG
    /// `[` / `(` / `]` / `)` bracket inclusivity. `empty=true`
    /// supersedes all other fields (the empty range has no
    /// bounds).
    Range {
        kind: RangeKind,
        // v7.37.42-arena Phase 1: Range bounds stay owned ('static).
        // Recursive arena lifetimes are awkward to migrate at this
        // phase and the SCALARSQ hot path doesn't construct ranges.
        lower: Option<alloc::boxed::Box<Value<'static>>>,
        upper: Option<alloc::boxed::Box<Value<'static>>>,
        lower_inc: bool,
        upper_inc: bool,
        empty: bool,
    },
    /// v7.38 (read01, T9) — a composite / record value (a `row(...)`
    /// constructor or a whole-row reference). Fields are `(name, value)`; the
    /// names are `f1..fN` for an anonymous `row(...)` or the source column
    /// names for a table row. Transient — flows through row_to_json / to_json
    /// and the composite text form `(a,b)`; not a storable column type here.
    Composite(alloc::vec::Vec<(alloc::string::String, Value<'static>)>),
    Null,
}

/// Owned `Value` — heap-bearing variants are `Cow::Owned`. Used everywhere
/// a Value must outlive a query-scoped arena (catalog defaults, persistent
/// storage, public APIs).
pub type ValueOwned = Value<'static>;

/// v7.37.5 ε — PG `point` building block. Shared by every other
/// geometric type (lseg / path / box / polygon / circle all
/// reduce to compositions of `Point2D`). Packed `{x: f64, y: f64}`,
/// 16 B, on-disk LE field order matches the PG binary point
/// format byte-for-byte (so a future binary BIND path lands
/// without rearrangement).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point2D {
    pub x: f64,
    pub y: f64,
}

/// v7.37.5 δ — single-range bounds without the kind tag. Used as
/// the element type of `Value::Multirange { kind, ranges }` so a
/// multirange carries one shared `RangeKind` plus N bounds-only
/// spans (saves 1 byte/elem vs duplicating the kind). The five
/// other fields mirror `Value::Range` exactly.
#[derive(Debug, Clone, PartialEq)]
pub struct RangeSpan {
    // v7.37.42-arena Phase 1: stays owned ('static) — same rationale as
    // Range bounds above.
    pub lower: Option<alloc::boxed::Box<Value<'static>>>,
    pub upper: Option<alloc::boxed::Box<Value<'static>>>,
    pub lower_inc: bool,
    pub upper_inc: bool,
    pub empty: bool,
}

/// v7.37.5 β-P4 — element type for `Value::IntervalArray`. Mirrors
/// the `{months, days, micros}` shape of scalar `Value::Interval`,
/// broken out as a named struct so `IntervalArray`'s element type
/// is concrete (24 bytes, packed) instead of an enum-boxed Value.
/// All three dimensions are independent — `IntervalSpan { days: 1,
/// .. }` is distinct from `IntervalSpan { micros: 86_400_000_000,
/// .. }` per PG byte-equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntervalSpan {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

impl<'arena> Value<'arena> {
    /// Type tag, or `None` for `NULL` (unknown at value level).
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Self::SmallInt(_) => Some(DataType::SmallInt),
            Self::Int(_) => Some(DataType::Int),
            Self::BigInt(_) => Some(DataType::BigInt),
            Self::Float(_) => Some(DataType::Float),
            Self::Real(_) => Some(DataType::Real),
            // `Text` covers both unbounded TEXT and bounded VARCHAR/CHAR
            // — the constraint lives on the column schema, not the value.
            Self::Text(_) => Some(DataType::Text),
            Self::Bool(_) => Some(DataType::Bool),
            Self::Vector(v) => Some(DataType::Vector {
                dim: u32::try_from(v.len()).expect("vector dim ≤ u32"),
                encoding: VecEncoding::F32,
            }),
            Self::Sq8Vector(q) => Some(DataType::Vector {
                dim: u32::try_from(q.bytes.len()).expect("vector dim ≤ u32"),
                encoding: VecEncoding::Sq8,
            }),
            Self::HalfVector(h) => Some(DataType::Vector {
                dim: u32::try_from(h.dim()).expect("vector dim ≤ u32"),
                encoding: VecEncoding::F16,
            }),
            // `Value::Numeric` doesn't carry its precision (the column
            // schema does); we surface precision=0 as "unknown" and let
            // the engine reconcile against the column type at coercion
            // time.
            Self::Numeric { scale, .. } => Some(DataType::Numeric {
                precision: 0,
                scale: *scale,
            }),
            Self::NumericBig(b) => Some(DataType::Numeric {
                precision: 0,
                scale: b.scale(),
            }),
            Self::Date(_) => Some(DataType::Date),
            Self::Timestamp(_) => Some(DataType::Timestamp),
            Self::Interval { .. } => Some(DataType::Interval),
            Self::Json(_) => Some(DataType::Json),
            Self::Bytes(_) => Some(DataType::Bytes),
            Self::TextArray(_) => Some(DataType::TextArray),
            Self::IntArray(_) => Some(DataType::IntArray),
            Self::BigIntArray(_) => Some(DataType::BigIntArray),
            Self::IntervalArray(_) => Some(DataType::IntervalArray),
            Self::BoolArray(_) => Some(DataType::BoolArray),
            Self::SmallIntArray(_) => Some(DataType::SmallIntArray),
            Self::FloatArray(_) => Some(DataType::FloatArray),
            Self::NumericArray(_) => Some(DataType::NumericArray),
            Self::DateArray(_) => Some(DataType::DateArray),
            Self::TimestampArray(_) => Some(DataType::TimestampArray),
            Self::TimestamptzArray(_) => Some(DataType::TimestamptzArray),
            Self::UuidArray(_) => Some(DataType::UuidArray),
            Self::JsonArray(_) => Some(DataType::JsonArray),
            Self::JsonbArray(_) => Some(DataType::JsonbArray),
            Self::BytesArray(_) => Some(DataType::BytesArray),
            Self::VarcharArray(_) => Some(DataType::VarcharArray),
            Self::CharArray(_) => Some(DataType::CharArray),
            Self::Multirange { kind, .. } => Some(DataType::Multirange(*kind)),
            Self::Point(_) => Some(DataType::Point),
            Self::Lseg(_, _) => Some(DataType::Lseg),
            Self::Path { .. } => Some(DataType::Path),
            Self::PgBox(_, _) => Some(DataType::PgBox),
            Self::Polygon(_) => Some(DataType::Polygon),
            Self::Line { .. } => Some(DataType::Line),
            Self::Circle { .. } => Some(DataType::Circle),
            Self::Inet { .. } => Some(DataType::Inet),
            Self::Cidr { .. } => Some(DataType::Cidr),
            Self::Macaddr(_) => Some(DataType::Macaddr),
            Self::Macaddr8(_) => Some(DataType::Macaddr8),
            Self::PgLsn(_) => Some(DataType::PgLsn),
            // BitString could be either Bit or BitVarying; column
            // schema decides. Default to BitVarying when called
            // schema-less (rare; storage path is always
            // schema-aware so this only matters for diagnostics).
            Self::BitString { .. } => Some(DataType::BitVarying),
            Self::Xml(_) => Some(DataType::Xml),
            Self::Char1(_) => Some(DataType::Char1),
            // BpChar reports its declared width from the padded length.
            Self::BpChar(s) => Some(DataType::Char(
                u32::try_from(s.chars().count()).unwrap_or(0),
            )),
            Self::MoneyArray(_) => Some(DataType::MoneyArray),
            Self::TsVector(_) => Some(DataType::TsVector),
            Self::TsQuery(_) => Some(DataType::TsQuery),
            Self::Uuid(_) => Some(DataType::Uuid),
            Self::Time(_) => Some(DataType::Time),
            Self::Year(_) => Some(DataType::Year),
            Self::TimeTz { .. } => Some(DataType::TimeTz),
            Self::Money(_) => Some(DataType::Money),
            Self::Range { kind, .. } => Some(DataType::Range(*kind)),
            Self::Hstore(_) => Some(DataType::Hstore),
            Self::IntArray2D(_) => Some(DataType::IntArray2D),
            Self::BigIntArray2D(_) => Some(DataType::BigIntArray2D),
            Self::TextArray2D(_) => Some(DataType::TextArray2D),
            // v7.38 (read01, T9) — a transient composite/record has no storable
            // column DataType (it flows through row_to_json / to_json).
            Self::Composite(_) => None,
            // v7.39 (read01 ruleutils.c) — regclass is eval-only (dual
            // oid+name shape); no column storage type.
            Self::RegClass(..) => None,
            Self::Null => None,
        }
    }

    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// v7.37.42-arena Phase 1: lift any `Value<'arena>` (possibly
    /// borrowing from a bump arena) into a fully-owned `Value<'static>`.
    /// Used at boundaries that must outlive the per-query arena
    /// (catalog write, public QueryResult emit, sqlx materialise).
    ///
    /// For the recursive Range/Multirange variants — bounds are already
    /// `Box<Value<'static>>` per Phase 1 design, so we just rebuild the
    /// outer enum at `'static`.
    pub fn into_owned(self) -> Value<'static> {
        match self {
            Value::SmallInt(n) => Value::SmallInt(n),
            Value::Int(n) => Value::Int(n),
            Value::BigInt(n) => Value::BigInt(n),
            Value::Float(f) => Value::Float(f),
            Value::Real(f) => Value::Real(f),
            Value::Text(s) => Value::Text(Cow::Owned(s.into_owned())),
            Value::Bool(b) => Value::Bool(b),
            Value::Vector(v) => Value::Vector(Cow::Owned(v.into_owned())),
            Value::Sq8Vector(q) => Value::Sq8Vector(q),
            Value::HalfVector(h) => Value::HalfVector(h),
            Value::Numeric {
                scaled,
                scale,
                kind,
            } => Value::Numeric {
                scaled,
                scale,
                kind,
            },
            Value::NumericBig(b) => Value::NumericBig(b),
            Value::Date(d) => Value::Date(d),
            Value::Timestamp(t) => Value::Timestamp(t),
            Value::Interval {
                months,
                days,
                micros,
            } => Value::Interval {
                months,
                days,
                micros,
            },
            Value::Json(s) => Value::Json(Cow::Owned(s.into_owned())),
            Value::Bytes(b) => Value::Bytes(Cow::Owned(b.into_owned())),
            Value::TextArray(v) => Value::TextArray(v),
            Value::IntArray(v) => Value::IntArray(v),
            Value::BigIntArray(v) => Value::BigIntArray(v),
            Value::IntervalArray(v) => Value::IntervalArray(v),
            Value::BoolArray(v) => Value::BoolArray(v),
            Value::SmallIntArray(v) => Value::SmallIntArray(v),
            Value::FloatArray(v) => Value::FloatArray(v),
            Value::NumericArray(v) => Value::NumericArray(v),
            Value::DateArray(v) => Value::DateArray(v),
            Value::TimestampArray(v) => Value::TimestampArray(v),
            Value::TimestamptzArray(v) => Value::TimestamptzArray(v),
            Value::UuidArray(v) => Value::UuidArray(v),
            Value::JsonArray(v) => Value::JsonArray(v),
            Value::JsonbArray(v) => Value::JsonbArray(v),
            Value::BytesArray(v) => Value::BytesArray(v),
            Value::VarcharArray(v) => Value::VarcharArray(v),
            Value::CharArray(v) => Value::CharArray(v),
            Value::Multirange { kind, ranges } => Value::Multirange { kind, ranges },
            // v7.38 (read01, T9) — Composite fields are already `Value<'static>`.
            Value::Composite(fields) => Value::Composite(fields),
            Value::RegClass(oid, name) => Value::RegClass(oid, name),
            Value::Point(p) => Value::Point(p),
            Value::Lseg(a, b) => Value::Lseg(a, b),
            Value::Path { points, closed } => Value::Path { points, closed },
            Value::PgBox(a, b) => Value::PgBox(a, b),
            Value::Polygon(p) => Value::Polygon(p),
            Value::Line { a, b, c } => Value::Line { a, b, c },
            Value::Circle { center, radius } => Value::Circle { center, radius },
            Value::Inet { family, bits, addr } => Value::Inet { family, bits, addr },
            Value::Cidr { family, bits, addr } => Value::Cidr { family, bits, addr },
            Value::Macaddr(m) => Value::Macaddr(m),
            Value::Macaddr8(m) => Value::Macaddr8(m),
            Value::PgLsn(l) => Value::PgLsn(l),
            Value::BitString { nbits, bytes } => Value::BitString {
                nbits,
                bytes: Cow::Owned(bytes.into_owned()),
            },
            Value::Xml(s) => Value::Xml(Cow::Owned(s.into_owned())),
            Value::Char1(c) => Value::Char1(c),
            Value::BpChar(s) => Value::BpChar(Cow::Owned(s.into_owned())),
            Value::MoneyArray(v) => Value::MoneyArray(v),
            Value::TsVector(v) => Value::TsVector(v),
            Value::TsQuery(q) => Value::TsQuery(q),
            Value::Uuid(u) => Value::Uuid(u),
            Value::Time(t) => Value::Time(t),
            Value::Year(y) => Value::Year(y),
            Value::TimeTz { us, offset_secs } => Value::TimeTz { us, offset_secs },
            Value::Money(m) => Value::Money(m),
            Value::Range {
                kind,
                lower,
                upper,
                lower_inc,
                upper_inc,
                empty,
            } => Value::Range {
                kind,
                lower,
                upper,
                lower_inc,
                upper_inc,
                empty,
            },
            Value::Hstore(h) => Value::Hstore(h),
            Value::IntArray2D(a) => Value::IntArray2D(a),
            Value::BigIntArray2D(a) => Value::BigIntArray2D(a),
            Value::TextArray2D(a) => Value::TextArray2D(a),
            Value::Null => Value::Null,
        }
    }

    /// v7.37.42-arena Phase 4 — copy heap payloads into the supplied
    /// bump arena, yielding a `Value<'a>` whose Cow-variant payloads
    /// are arena-borrowed (or stay as small owned scalars for the
    /// `Copy`-able variants).
    ///
    /// Used at the catalog ↔ ephemeral boundary: a `ColumnSchema.default`
    /// is `Value<'static>` but INSERT-time eval may want it stamped into
    /// the per-statement arena alongside other arena-built scalars.
    ///
    /// Allocates only into the supplied arena; the input `&self` keeps
    /// its own storage. For `Copy`-able / nested-owned variants the
    /// implementation falls back to `clone()` (the nested heap blocks
    /// stay on the global allocator, which is fine — the boundary
    /// requirement is just "no aliasing of caller-owned strings").
    pub fn clone_into<'a>(&self, arena: &'a bumpalo::Bump) -> Value<'a> {
        match self {
            Value::Text(s) => Value::Text(Cow::Borrowed(arena.alloc_str(s))),
            Value::Json(s) => Value::Json(Cow::Borrowed(arena.alloc_str(s))),
            Value::Xml(s) => Value::Xml(Cow::Borrowed(arena.alloc_str(s))),
            Value::BpChar(s) => Value::BpChar(Cow::Borrowed(arena.alloc_str(s))),
            Value::Bytes(b) => {
                let slot = arena.alloc_slice_copy::<u8>(b);
                Value::Bytes(Cow::Borrowed(slot))
            }
            Value::Vector(v) => {
                let slot = arena.alloc_slice_copy::<f32>(v);
                Value::Vector(Cow::Borrowed(slot))
            }
            Value::BitString { nbits, bytes } => {
                let slot = arena.alloc_slice_copy::<u8>(bytes);
                Value::BitString {
                    nbits: *nbits,
                    bytes: Cow::Borrowed(slot),
                }
            }
            // Copy-able scalars + variants whose nested heap blocks are
            // `'static` regardless of `'arena` (TextArray, JsonArray,
            // Hstore, TsVector, Range bounds, …). Clone the heap block
            // via the standard `into_owned()` path then lift the
            // resulting `Value<'static>` to `Value<'a>` via the Cow
            // variance — `'static` covers any lifetime.
            other => other.clone().into_owned(),
        }
    }
}

impl Value<'static> {
    /// v7.37.42-arena Phase 1 — owned-Text constructor. The variant now
    /// holds `Cow<'arena, str>`, so the previous `Value::Text(String)`
    /// shape no longer compiles directly. This helper preserves the
    /// historical ergonomics: `Value::text("foo")` or
    /// `Value::text(String::from("foo"))`.
    pub fn text<S: Into<String>>(s: S) -> Self {
        Value::Text(Cow::Owned(s.into()))
    }

    /// v7.38 (read01, T6) — a finite NUMERIC from its fixed-point parts.
    pub const fn numeric(scaled: i128, scale: u8) -> Self {
        Value::Numeric {
            scaled,
            scale,
            kind: NumericKind::Finite,
        }
    }

    /// v7.38 (read01, T6) — a special NUMERIC (NaN / ±Infinity). The fixed-point
    /// fields are canonicalized to 0 so equal specials compare byte-identical.
    pub const fn numeric_special(kind: NumericKind) -> Self {
        Value::Numeric {
            scaled: 0,
            scale: 0,
            kind,
        }
    }

    /// v7.37.42-arena Phase 1 — owned-Json constructor (mirrors `text`).
    pub fn json<S: Into<String>>(s: S) -> Self {
        Value::Json(Cow::Owned(s.into()))
    }

    /// v7.37.42-arena Phase 1 — owned-Xml constructor.
    pub fn xml<S: Into<String>>(s: S) -> Self {
        Value::Xml(Cow::Owned(s.into()))
    }

    /// v7.37.42-arena Phase 1 — owned-Bytes constructor.
    pub fn bytes<B: Into<Vec<u8>>>(b: B) -> Self {
        Value::Bytes(Cow::Owned(b.into()))
    }

    /// v7.37.42-arena Phase 1 — owned-Vector constructor.
    pub fn vector<V: Into<Vec<f32>>>(v: V) -> Self {
        Value::Vector(Cow::Owned(v.into()))
    }

    /// v7.37.42-arena Phase 1 — owned-BitString constructor.
    pub fn bit_string<B: Into<Vec<u8>>>(nbits: u32, bytes: B) -> Self {
        Value::BitString {
            nbits,
            bytes: Cow::Owned(bytes.into()),
        }
    }
}

/// One table row — values are positional and must match
/// `TableSchema.columns` in length and (modulo NULL) in `DataType`.
///
/// v7.37.42-arena Phase 1: parameterised on `'arena` so per-query rows
/// can borrow from a bump arena. The owned shape (`Row<'static>`, alias
/// `RowOwned`) is what catalog storage, public APIs, and tests use.
#[derive(Debug, Clone, PartialEq)]
pub struct Row<'arena> {
    pub values: Vec<Value<'arena>>,
}

/// Owned `Row` — values are `Value<'static>`. Used everywhere a row must
/// outlive a query-scoped arena.
pub type RowOwned = Row<'static>;

impl<'arena> Row<'arena> {
    pub const fn new(values: Vec<Value<'arena>>) -> Self {
        Self { values }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl<'arena> Row<'arena> {
    /// v7.37.42-arena Phase 4 — copy every cell into the supplied bump
    /// arena, yielding a `Row<'a>` whose Cow-payloads are arena-borrowed.
    /// Boundary helper for catalog defaults → DML eval handoff and
    /// arena-local row scratch.
    pub fn clone_into<'a>(&self, arena: &'a bumpalo::Bump) -> Row<'a> {
        Row {
            values: self.values.iter().map(|v| v.clone_into(arena)).collect(),
        }
    }

    /// v7.37.42-arena Phase 4 — lift this `Row<'arena>` to a fully-owned
    /// `Row<'static>` for catalog write / WAL serialisation. Equivalent
    /// to `Row::from_arena(self)` but consumes by value at any lifetime
    /// (callers can write `row.into_owned()` mirroring `Value::into_owned`).
    pub fn into_owned(self) -> Row<'static> {
        Row {
            values: self.values.into_iter().map(Value::into_owned).collect(),
        }
    }
}

impl Row<'static> {
    /// v7.37.42-arena Phase 1 — lift any `Row<'arena>` (possibly arena-
    /// borrowed) into a fully-owned `Row<'static>`. Mirrors
    /// `Value::into_owned`.
    pub fn from_arena(row: Row<'_>) -> Self {
        Self {
            values: row.values.into_iter().map(Value::into_owned).collect(),
        }
    }
}

/// Each bool is an independent, separately-persisted column attribute
/// (`nullable`, `auto_increment`, `is_unsigned`, `identity_always`) that the
/// catalog appendix reads and writes by name. Packing them into a bitflags
/// word would buy nothing and would put a decoding step between the on-disk
/// format and every reader of the schema.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnSchema {
    pub name: String,
    pub ty: DataType,
    pub nullable: bool,
    /// Optional `DEFAULT` value, frozen at CREATE TABLE time. `None`
    /// means "no default" (so omitted columns become NULL, or error
    /// out when the column is NOT NULL). Literal defaults take this
    /// path.
    ///
    /// v7.37.42-arena Phase 1: explicitly `Value<'static>` — catalog
    /// defaults must outlive any per-query arena.
    pub default: Option<Value<'static>>,
    /// v7.9.21 — for DEFAULT expressions that need INSERT-time
    /// evaluation (e.g. `DEFAULT now()`, `DEFAULT CURRENT_TIMESTAMP`),
    /// the Display form of the expression. The engine re-parses
    /// it on each INSERT default-fill, evaluates against an empty
    /// row context, and coerces to the column type. mailrs G4.
    /// Persisted in catalog FILE_VERSION 15+; older catalogs
    /// deserialise with None.
    pub runtime_default: Option<String>,
    /// MySQL-style `AUTO_INCREMENT`. When set, an INSERT that leaves
    /// this column unbound (or sets it to NULL) gets the next integer
    /// computed from the column's current max + 1.
    pub auto_increment: bool,
    /// v7.17.0 Phase 1.4 — when the column is bound to a user-
    /// defined ENUM type (the parser saw an unknown type ident
    /// and the engine resolved it against `catalog.enum_types`),
    /// this carries the enum name so INSERT/UPDATE can validate
    /// the cell value against the enum's labels. `ty` is
    /// `DataType::Text` in that case. Persisted in catalog
    /// FILE_VERSION 29+; older catalogs deserialise with None.
    pub user_enum_type: Option<String>,
    /// v7.17.0 Phase 1.5 — when the column is bound to a user-
    /// defined DOMAIN (the parser saw an unknown type ident and
    /// the engine resolved it against `catalog.domain_types`),
    /// this carries the domain name. `ty` is the domain's base
    /// type; INSERT/UPDATE re-evaluates the domain's CHECK list
    /// + NOT NULL against the cell value. Persisted in catalog
    /// FILE_VERSION 30+; older catalogs deserialise with None.
    pub user_domain_type: Option<String>,
    /// v7.39 (read01 round 56) — when the column is bound to a user-defined
    /// COMPOSITE type. `ty` stays `DataType::Jsonb` (the on-disk form), but the
    /// engine REHYDRATES the stored JSON into a `Value::Composite` on read, so
    /// field access `(p).x`, `= ROW(…)`, ordering and the canonical `(2,b)`
    /// text form all work — they were already implemented on Value::Composite;
    /// what was missing was that the column never recorded WHICH composite type
    /// it holds (this field's doc comment existed for two releases, the field
    /// itself did not). Persisted in the composite-column appendix
    /// (FILE_VERSION 63+); older catalogs deserialise with None.
    pub user_composite_type: Option<String>,
    /// v7.39 (read01 round 59) — column-level privileges (PG
    /// `pg_attribute.attacl`). `GRANT SELECT (pub) ON t TO dan` lands here and
    /// does NOT touch the table's `relacl`. Empty = no column grant, which is
    /// every column until one is made.
    pub acl: Vec<AclItem>,
    /// v7.17.0 Phase 2.1 — MySQL `ON UPDATE CURRENT_TIMESTAMP`
    /// column attribute. When `Some(expr_src)`, an UPDATE that
    /// does NOT bind this column overrides the new value with
    /// the engine-evaluated expression (always `now()` in
    /// v7.17.0). Stored as Display-form source so storage
    /// stays free of spg-sql; the engine re-parses at UPDATE
    /// time. Persisted in catalog FILE_VERSION 32+; older
    /// catalogs deserialise with None — preserves the existing
    /// "silent ignore" behaviour for snapshots written before
    /// the upgrade.
    pub on_update_runtime: Option<String>,
    /// v7.17.0 Phase 2.5 — text collation. Pre-2.5 SPG accepted
    /// `COLLATE <name>` clauses but discarded the name, so a
    /// column declared `COLLATE "case_insensitive"` (or any
    /// MySQL `_ci` collation) still compared byte-wise — a
    /// Tier-S silent failure where `WHERE name = 'foo'` never
    /// matched stored `'Foo'`. This carries the parser-derived
    /// classification so the engine's WHERE evaluator can route
    /// text equality through a case-aware compare. `Binary` (the
    /// default) preserves the prior byte-wise behaviour. Only
    /// CaseInsensitive lands in the catalog appendix — Binary
    /// columns stay implicit, keeping snapshots compact.
    /// Persisted in catalog FILE_VERSION 34+; older catalogs
    /// deserialise every column as `Binary`.
    pub collation: Collation,
    /// v7.17.0 Phase 4.4 — MySQL `UNSIGNED` modifier flag. Drives
    /// engine-side INSERT / UPDATE range enforcement (rejects
    /// negative values on UNSIGNED int columns). Pre-4.4 the
    /// parser consumed and discarded the keyword silently, so
    /// every UNSIGNED column quietly accepted negatives — a
    /// Tier-A correctness drift. Sparse: only UNSIGNED columns
    /// land in the catalog appendix; the default `false` keeps
    /// snapshots compact for the common signed-int path.
    /// Persisted in catalog FILE_VERSION 35+; older catalogs
    /// deserialise every column as `is_unsigned = false`.
    pub is_unsigned: bool,
    /// v7.17.0 Phase 3.P0-36 — MySQL inline `ENUM('a','b','c')`
    /// value list. Distinct from `user_enum_type` (which points
    /// to a separately CREATE TYPE'd PG enum); this carries the
    /// column-local list MySQL DDL declares inline. When `Some`,
    /// `ty` is `DataType::Text` and INSERT/UPDATE validates the
    /// cell value against this list. Variant ORDER is preserved
    /// (MySQL uses it for `ORDER BY col`). Sparse: only ENUM
    /// columns land in the catalog appendix.
    /// Persisted in catalog FILE_VERSION 41+; older catalogs
    /// deserialise with None — preserves silent-drop behaviour
    /// for snapshots written before P0-36.
    pub inline_enum_variants: Option<Vec<String>>,
    /// v7.17.0 Phase 3.P0-37 — MySQL inline `SET('a','b','c')`
    /// variant list. Storage is TEXT (canonical comma-joined in
    /// definition order, de-duplicated). INSERT/UPDATE validates
    /// every comma-separated token against this list. Sparse:
    /// only SET columns land in the catalog appendix.
    /// Persisted in catalog FILE_VERSION 42+; older catalogs
    /// deserialise with None.
    pub inline_set_variants: Option<Vec<String>>,
    /// v7.37.7(sentori Epic 3 P1)— `GENERATED ALWAYS AS (<expr>)
    /// STORED` computed-column source. When `Some`, INSERT / UPDATE
    /// recompute the cell against the candidate row(re-parse the
    /// stored Display form and evaluate)and overwrite any
    /// user-supplied value, matching PG's stored-generated-column
    /// semantics. `None` (the default) preserves the regular
    /// "column value is whatever the caller passed" path.
    /// Persisted in catalog FILE_VERSION 50+; older catalogs
    /// deserialise with None.
    pub generated_stored_expr: Option<String>,
    /// v7.38 (read01) — `GENERATED ALWAYS AS IDENTITY`. Both identity
    /// flavours set `auto_increment`; this additionally marks the ALWAYS
    /// flavour, whose explicit INSERT value PG rejects ("cannot insert a
    /// non-DEFAULT value into column …") unless `OVERRIDING SYSTEM VALUE`.
    /// `false` (serial / `BY DEFAULT`) keeps the permissive path. In-memory
    /// only for now — not yet in the catalog appendix, so a reloaded table
    /// deserialises as `false` (the pre-existing permissive behaviour).
    pub identity_always: bool,
    /// v7.38 (read01) — the DEFAULT expression's source text, deparsed to
    /// PG-compatible form at CREATE TABLE time (e.g. `0`, `(3 + 4)`,
    /// `'hi'::text`, `now()`, `CURRENT_DATE`). Distinct from `default`
    /// (the coerced value the INSERT path fills) and `runtime_default`
    /// (the recompute-per-row Display form): those lose the source
    /// spelling, so `information_schema.columns.column_default` /
    /// `pg_attrdef` / `pg_get_expr` reported the coerced render
    /// (`0.00` for `numeric(10,2) DEFAULT 0`) instead of PG's `0`.
    /// `None` for a column with no explicit default. Persisted in catalog
    /// FILE_VERSION 58+; older catalogs deserialise with None.
    pub default_text: Option<String>,
}

/// v7.17.0 Phase 2.5 — column-level text collation. Drives the
/// engine's WHERE / GROUP BY equality routing for `Value::Text`.
/// Only two variants are modelled in v7.17:
///   * `Binary`  — byte-wise comparison (the SPG default;
///                 matches PG `COLLATE "C"` / `pg_catalog.default`
///                 and MySQL `*_bin`).
///   * `CaseInsensitive` — ASCII case-folded comparison
///                 (matches PG `COLLATE "case_insensitive"` and
///                 MySQL `*_ci` collations). Non-ASCII bytes
///                 still compare byte-wise; full ICU folding is
///                 out of v7.17 scope.
/// New variants append at the end — older catalogs read missing
/// columns as `Binary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collation {
    Binary,
    CaseInsensitive,
}

#[allow(clippy::derivable_impls)]
impl Default for Collation {
    fn default() -> Self {
        Self::Binary
    }
}

impl Collation {
    /// Wire tag persisted in the FILE_VERSION 34+ catalog appendix.
    /// Stable: future variants append above the recognised range
    /// and unknown tags read back as `Binary` for forward-compat
    /// on rollback.
    pub const TAG_BINARY: u8 = 0;
    pub const TAG_CASE_INSENSITIVE: u8 = 1;
}

/// v7.39 (RLS) — the command a policy applies to. `ALL` is the default and
/// covers every command; the others scope the policy to one statement kind.
/// Persisted as a single byte in the policy appendix (FILE_VERSION 59+).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyCmd {
    All,
    Select,
    Insert,
    Update,
    Delete,
}

impl PolicyCmd {
    /// PG `pg_policy.polcmd` single-char encoding.
    #[must_use]
    pub const fn as_pg_char(self) -> char {
        match self {
            Self::All => '*',
            Self::Select => 'r',
            Self::Insert => 'a',
            Self::Update => 'w',
            Self::Delete => 'd',
        }
    }

    /// PG `pg_policies.cmd` word form.
    #[must_use]
    pub const fn as_pg_word(self) -> &'static str {
        match self {
            Self::All => "ALL",
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
        }
    }

    #[must_use]
    pub const fn to_wire_byte(self) -> u8 {
        match self {
            Self::All => 0,
            Self::Select => 1,
            Self::Insert => 2,
            Self::Update => 3,
            Self::Delete => 4,
        }
    }

    #[must_use]
    pub const fn from_wire_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::All),
            1 => Some(Self::Select),
            2 => Some(Self::Insert),
            3 => Some(Self::Update),
            4 => Some(Self::Delete),
            _ => None,
        }
    }
}

/// v7.39 (RLS) — one `CREATE POLICY` object, stored per table. The `using_expr`
/// / `with_check_expr` hold the qualifying expression's `Display` form
/// (re-parsed and evaluated per row at enforcement time, exactly like
/// `TableSchema.checks`); `None` means the clause was absent. `roles` empty =
/// PUBLIC. Persisted in the policy appendix (FILE_VERSION 59+).
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyDef {
    pub name: String,
    pub cmd: PolicyCmd,
    /// `true` = PERMISSIVE (default, OR-combined), `false` = RESTRICTIVE
    /// (AND-combined).
    pub permissive: bool,
    pub roles: Vec<String>,
    pub using_expr: Option<String>,
    pub with_check_expr: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<ColumnSchema>,
    /// v6.7.2 — per-table hot-tier byte budget override. `None`
    /// falls through to the global `SPG_HOT_TIER_BYTES` setting;
    /// `Some(n)` overrides it for this specific table. Set via
    /// `ALTER TABLE t SET hot_tier_bytes = X`. Persisted in
    /// catalog FILE_VERSION 11+.
    pub hot_tier_bytes: Option<u64>,
    /// v7.6.1 — FOREIGN KEY constraints declared on this table.
    /// Engine maintains this in lock-step with `spg-sql`'s parser
    /// AST; the storage layer carries the on-disk shape so a
    /// catalog snapshot round-trips without external mapping.
    /// Persisted in catalog FILE_VERSION 13+. Older catalogs
    /// deserialise with an empty vec.
    pub foreign_keys: Vec<ForeignKeyConstraint>,
    /// v7.9.19 — composite UNIQUE / PRIMARY KEY constraints
    /// declared at the table level. Each entry's leading column
    /// has a BTree index (created via the constraint), and INSERT
    /// path enforces the full-tuple uniqueness via a scan keyed
    /// by the leading column. Persisted in catalog FILE_VERSION
    /// 15+. Older catalogs (≤ 14) deserialise with an empty vec.
    pub uniqueness_constraints: Vec<UniquenessConstraint>,
    /// v7.13.0 — `CHECK (<expr>)` predicates declared on this
    /// table. Both column-level inline `CHECK (…)` and
    /// table-level `CHECK (…)` fold into this list. Each entry
    /// is the AST Expr's `Display` form, re-parsed on every
    /// INSERT/UPDATE and evaluated against the candidate row.
    /// A false / NULL result rejects the mutation (PG semantics).
    /// Persisted in catalog FILE_VERSION 23+. Older catalogs
    /// deserialise with an empty vec. v7.39 (read01 round 48) — each entry
    /// now carries the user's constraint name too (FILE_VERSION 60+).
    pub checks: Vec<CheckConstraint>,
    /// v7.37.6-B — declarative partition role(sentori Epic 2 P0).
    /// `None` = 普通表(后向兼容,< v49 catalog 默认 None)。
    /// `Some(Parent { … })` = `CREATE TABLE p (...) PARTITION BY RANGE (key_col)` 父表 —
    /// 父表自己 `rows` 永远空,INSERT 在引擎层路由到命中的 child。
    /// `Some(Range { … })` = `CREATE TABLE c PARTITION OF p FOR VALUES FROM (a) TO (b)` 范围子表。
    /// `Some(Default { … })` = `CREATE TABLE c PARTITION OF p DEFAULT` 兜底子表。
    /// 持久化于 FILE_VERSION 49+。
    pub partition_role: Option<PartitionRole>,
    /// v7.39 (RLS) — `CREATE POLICY` objects on this table, independent of the
    /// `row_security` flag (PG stores policies even on non-RLS tables; they
    /// only take effect once RLS is enabled). Persisted in the policy appendix
    /// (FILE_VERSION 59+). Older catalogs deserialise with an empty vec.
    pub policies: Vec<PolicyDef>,
    /// v7.39 (RLS) — `ALTER TABLE … ENABLE ROW LEVEL SECURITY`
    /// (PG `pg_class.relrowsecurity`). Fresh table = `false`.
    pub row_security: bool,
    /// v7.39 (RLS) — `ALTER TABLE … FORCE ROW LEVEL SECURITY`
    /// (PG `pg_class.relforcerowsecurity`); subjects the table owner to RLS
    /// too. Fresh table = `false`.
    pub force_row_security: bool,
    /// v7.39 (read01 round 57, ACL) — the role that owns this table: whoever
    /// ran CREATE TABLE (PG `pg_class.relowner`). The owner holds every
    /// privilege implicitly and is the only role that may ALTER / DROP it.
    /// `None` = an image written before FILE_VERSION 64, which predates roles
    /// entirely; those tables read back as owned by the login role.
    pub owner: Option<String>,
    /// v7.39 (read01 round 57, ACL) — explicit GRANTs on this table
    /// (PG `pg_class.relacl`). EMPTY means "never granted": PG leaves relacl
    /// NULL while only the owner's implicit privileges apply, and materialises
    /// the whole list — owner's default entry included — on the first GRANT.
    /// Once materialised it stays, even after every grant is revoked.
    pub acl: Vec<AclItem>,
}

/// v7.39 (read01 round 57) — one PG `aclitem`: what `grantee` may do to a
/// table, and who granted it. Renders as `grantee=privs/grantor`, with an
/// EMPTY grantee meaning PUBLIC (`=r/owner`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclItem {
    /// The role the privileges are held by. Empty string = PUBLIC.
    pub grantee: String,
    /// Bitmask over `priv_bits`: which privileges are held.
    pub privs: u16,
    /// Bitmask over `priv_bits`: which of them carry WITH GRANT OPTION
    /// (PG renders those with a trailing `*` — `r*`).
    pub grantable: u16,
    /// The role that ran the GRANT.
    pub grantor: String,
}

/// v7.39 (read01 round 57) — the table-privilege bits, in PG's `aclitem`
/// rendering order (`arwdDxtm`). The order matters: `relacl` output is
/// byte-compared against PG.
pub mod priv_bits {
    pub const INSERT: u16 = 1 << 0; // a
    pub const SELECT: u16 = 1 << 1; // r
    pub const UPDATE: u16 = 1 << 2; // w
    pub const DELETE: u16 = 1 << 3; // d
    pub const TRUNCATE: u16 = 1 << 4; // D
    pub const REFERENCES: u16 = 1 << 5; // x
    pub const TRIGGER: u16 = 1 << 6; // t
    pub const MAINTAIN: u16 = 1 << 7; // m
    /// v7.39 (read01 round 60) — the non-table privileges. They share the
    /// bitmask because an aclitem is an aclitem whatever it hangs off; which
    /// bits are MEANINGFUL depends on the object (a sequence has r / w / U, a
    /// schema has U / C, a database has C / c / T).
    pub const USAGE: u16 = 1 << 8; // U
    pub const CREATE: u16 = 1 << 9; // C
    pub const CONNECT: u16 = 1 << 10; // c
    pub const TEMPORARY: u16 = 1 << 11; // T
    pub const EXECUTE: u16 = 1 << 12; // X
    /// Every TABLE privilege — what `GRANT ALL ON <table>` grants and what a
    /// table's owner holds.
    pub const ALL: u16 = INSERT | SELECT | UPDATE | DELETE | TRUNCATE | REFERENCES | TRIGGER | MAINTAIN;
    /// `GRANT ALL ON SEQUENCE` — PG renders a sequence owner's default as `rwU`.
    pub const ALL_SEQUENCE: u16 = SELECT | UPDATE | USAGE;
    /// `GRANT ALL ON SCHEMA` — `UC`.
    pub const ALL_SCHEMA: u16 = USAGE | CREATE;
    /// `GRANT ALL ON DATABASE` — `CTc`.
    pub const ALL_DATABASE: u16 = CREATE | CONNECT | TEMPORARY;
    /// `GRANT ALL ON FUNCTION` — just `X`.
    pub const ALL_FUNCTION: u16 = EXECUTE;
}

/// v7.37.6-B — partition 三态(parent / range child / default child)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartitionRole {
    Parent {
        kind: PartitionKind,
        /// 父表 columns 中 key 列的下标(单列 v7.37.6-B,
        /// `Vec` 为将来扩多列预留)。
        key_column_positions: Vec<usize>,
        /// `CREATE INDEX ON parent (…)` 的 Display-form 源串。
        /// child 创建时再 parse + 在 child 上 execute,这样 future
        /// child 也自动继承父表索引。fan-out 实施在引擎层。
        index_template_sources: Vec<String>,
    },
    Range {
        parent_name: String,
        /// 半开区间下界(`>=`,SQL `FROM (lower)`).
        lower: PartitionBound,
        /// 半开区间上界(`<`,SQL `TO (upper)`).
        upper: PartitionBound,
    },
    /// v7.37.16 (16.1) — LIST child:行属于本 child iff key ∈ values。
    /// `values` 在 child 创建时从 SQL `FOR VALUES IN (lit, …)` 求值;
    /// 跟 PG 一样,显式 NULL ∈ values 由 caller 单独处理(不在
    /// PartitionBound 内表达 NULL)。
    List {
        parent_name: String,
        values: Vec<PartitionBound>,
    },
    /// v7.37.16 (16.2) — HASH child:行属于本 child iff
    /// `pg_compatible_hash(key) mod modulus == remainder`。
    /// PG 强制 `0 ≤ remainder < modulus`;parser/DDL 层先 gate。
    Hash {
        parent_name: String,
        modulus: u32,
        remainder: u32,
    },
    Default {
        parent_name: String,
    },
}

/// v7.37.6-B — 分区策略。
///
/// - `Range`:半开区间 `[lower, upper)`(v7.37.6-B 初始)
/// - `List` (v7.37.16):枚举集合 — 行属于 partition iff key ∈ children list
/// - `Hash` (v7.37.16):`hash(key) mod modulus == remainder`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionKind {
    Range,
    List,
    Hash,
}

/// v7.37.6-B — partition 边界 literal。
///
/// v7.37.6-B 仅 `TimestampTz`(i64 microseconds since epoch);
/// v7.37.16 (16.6) 加全 PG 内建可比类型,匹配 `Value` 的对应 variant
/// 以避免 LIST membership 比较时的类型转换。
///
/// `MinValue` / `MaxValue` 对应 SQL `MINVALUE` / `MAXVALUE`,仅
/// Range 策略有意义(LIST 无 minvalue/maxvalue 概念,HASH 不
/// 使用 PartitionBound)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartitionBound {
    MinValue,
    MaxValue,
    TimestampTz(i64),
    /// v7.37.16 (16.6) — BIGINT partition key.
    BigInt(i64),
    /// v7.37.16 (16.6) — INTEGER partition key (also covers
    /// `SERIAL` since SPG decomposes it to INTEGER + sequence).
    Int(i32),
    /// v7.37.16 (16.6) — SMALLINT partition key.
    SmallInt(i16),
    /// v7.37.16 (16.6) — DATE partition key. Stored as days
    /// since the Unix epoch (matches `Value::Date`).
    Date(i32),
    /// v7.37.16 (16.6) — TEXT / VARCHAR partition key.
    Text(alloc::string::String),
}

impl PartitionBound {
    /// v7.37.16 (16.6) — true iff this bound's underlying value
    /// equals `other`'s. Used for LIST partition membership
    /// checks. Returns false for `MinValue` / `MaxValue`
    /// (sentinels — never literal equality).
    #[must_use]
    pub fn equals_value(&self, other: &Value<'_>) -> bool {
        match (self, other) {
            (PartitionBound::TimestampTz(a), Value::Timestamp(b)) => a == b,
            (PartitionBound::BigInt(a), Value::BigInt(b)) => a == b,
            (PartitionBound::Int(a), Value::Int(b)) => a == b,
            (PartitionBound::SmallInt(a), Value::SmallInt(b)) => a == b,
            (PartitionBound::Date(a), Value::Date(b)) => a == b,
            (PartitionBound::Text(a), Value::Text(b)) => a.as_str() == b.as_ref(),
            _ => false,
        }
    }
}

/// v7.9.19 — composite UNIQUE / PRIMARY KEY constraint persisted
/// on the table schema. The leading column always has a BTree
/// index (created at CREATE TABLE time); INSERT enforcement
/// scans that index for collisions on the full column tuple.
/// v7.39 (read01 round 48) — a `CHECK` constraint: the SQL name the user
/// gave it (via `ADD CONSTRAINT <name> CHECK (...)` or the inline
/// `CONSTRAINT <name> CHECK (...)` form) plus the predicate source. `None`
/// name = unnamed, in which case `pg_constraint` synthesises PG's
/// `<table>_<col>_check` form. Names are persisted in the constraint-name
/// appendix (FILE_VERSION 60+); older catalogs deserialise with `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckConstraint {
    pub name: Option<String>,
    /// The AST Expr's `Display` form, re-parsed on every INSERT/UPDATE.
    pub expr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniquenessConstraint {
    /// `true` when this constraint was declared as `PRIMARY KEY`
    /// (vs `UNIQUE`). Semantically PK implies NOT NULL on all
    /// referenced columns; the engine enforces that at CREATE
    /// TABLE time.
    pub is_primary_key: bool,
    /// Column positions on the parent table. ≥ 1 element. For
    /// single-column UNIQUE this is exactly one position; the
    /// BTree index alone enforces it.
    pub columns: Vec<usize>,
    /// v7.13.0 — `UNIQUE NULLS NOT DISTINCT` modifier
    /// (mailrs round-5 G10; PG 15+ surface). When `true`, two
    /// rows whose constrained columns are all NULL collide on
    /// the constraint. Default (`false`) is the SQL-standard
    /// `NULLS DISTINCT` behaviour where any NULL passes.
    /// Persisted in catalog FILE_VERSION 23+.
    pub nulls_not_distinct: bool,
    /// v7.39 (read01 round 48) — the constraint's SQL name when the user
    /// supplied one (`ADD CONSTRAINT <name> PRIMARY KEY/UNIQUE (...)`, or
    /// the inline `CONSTRAINT <name>` form). `None` = unnamed, in which
    /// case `pg_constraint` synthesises PG's `<table>_pkey` /
    /// `<table>_<col>_key` form. DROP CONSTRAINT resolves the stored name
    /// first and falls back to the synthesised one, so catalogs written
    /// before this field (< FILE_VERSION 60) keep working unchanged.
    pub name: Option<String>,
}

/// v7.6.1 — Storage-layer mirror of `spg_sql::ast::ForeignKeyConstraint`.
/// The engine's CREATE TABLE path translates between the two; keeping
/// them separate preserves the no-deps boundary between
/// `spg-storage` and `spg-sql`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyConstraint {
    /// Optional user-supplied constraint name (`CONSTRAINT <name>`
    /// prefix). Used by `ALTER TABLE DROP CONSTRAINT <name>` in
    /// v7.6.8; ignored by enforcement.
    pub name: Option<String>,
    /// Positions of local columns in this table's column list.
    /// Same arity as `parent_columns`.
    pub local_columns: Vec<usize>,
    /// Referenced parent table name.
    pub parent_table: String,
    /// Positions of parent columns in the parent's column list.
    /// Engine resolves these at CREATE TABLE time (after the parent
    /// schema is known) so enforcement paths can skip the name
    /// lookup on every row.
    pub parent_columns: Vec<usize>,
    /// Referential action when a parent row is deleted.
    pub on_delete: FkAction,
    /// Referential action when a parent row's referenced columns
    /// are updated.
    pub on_update: FkAction,
    /// v7.38 (read01, T29) — `MATCH SIMPLE | FULL`. Defaults to `Simple`.
    pub match_type: MatchType,
}

/// v7.38 (read01, T29) — FK MATCH type. Mirrors `spg_sql::ast::MatchType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchType {
    #[default]
    Simple,
    Full,
}

impl MatchType {
    /// On-disk tag byte (catalog appendix, `FILE_VERSION` 55+).
    pub const fn tag(self) -> u8 {
        match self {
            Self::Simple => 0,
            Self::Full => 1,
        }
    }
    pub const fn from_tag(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Simple,
            1 => Self::Full,
            _ => return None,
        })
    }
}

/// v7.6.1 — referential action tag. Mirrors `spg_sql::ast::FkAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FkAction {
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
    NoAction,
}

impl FkAction {
    /// On-disk tag byte (v13 catalog appendix).
    pub const fn tag(self) -> u8 {
        match self {
            Self::Restrict => 0,
            Self::Cascade => 1,
            Self::SetNull => 2,
            Self::SetDefault => 3,
            Self::NoAction => 4,
        }
    }
    pub const fn from_tag(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Restrict,
            1 => Self::Cascade,
            2 => Self::SetNull,
            3 => Self::SetDefault,
            4 => Self::NoAction,
            _ => return None,
        })
    }
}

impl TableSchema {
    pub fn column_position(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
}

/// Key type accepted by secondary indices. Float / NULL / Vector values
/// can't participate in a B-tree index — `f64` is only `PartialOrd`, NULL
/// has SQL-three-valued semantics, and Vector belongs to the (future) HNSW
/// path. Index lookups on those columns fall back to full scan.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum IndexKey {
    Int(i64),
    Text(String),
    Bool(bool),
    /// v7.17.0 — `Value::Uuid` index key. Comparison is byte-wise
    /// (RFC 4122 byte order) so PRIMARY KEY UUID lookups land on
    /// the same fast-path as Int / Text.
    Uuid([u8; 16]),
}

impl IndexKey {
    /// v7.37.43 (INSUBQ B-4) — inline-friendly BigInt fast path.
    /// `try_count_star_pk_in_subquery_fast` (and any other hot loop
    /// probing an integer PK) already holds an `i64`; this builds the
    /// `IndexKey` without going through the generic `from_value`
    /// dispatch tree.
    #[inline]
    pub fn from_i64(n: i64) -> Self {
        Self::Int(n)
    }

    pub fn from_value(v: &Value<'_>) -> Option<Self> {
        match v {
            // v7.37.43 (INSUBQ B-4) — BigInt hits first (the dominant
            // INSUBQ shape probes PK as BigInt). Tiny micro-win.
            Value::BigInt(n) => Some(Self::Int(*n)),
            Value::SmallInt(n) => Some(Self::Int(i64::from(*n))),
            Value::Int(n) => Some(Self::Int(i64::from(*n))),
            Value::Text(s) => Some(Self::Text(s.clone().into_owned())),
            // v7.38 (read01, T11) — bpchar keys compare blank-insensitively.
            Value::BpChar(s) => Some(Self::Text(s.trim_end_matches(' ').to_string())),
            Value::Bool(b) => Some(Self::Bool(*b)),
            // Date/Timestamp use their integer storage repr as the
            // index key — same order semantics, same comparison.
            Value::Date(d) => Some(Self::Int(i64::from(*d))),
            Value::Timestamp(t) => Some(Self::Int(*t)),
            // v7.17.0: UUID indexable via byte-wise ordering. Lookup
            // on `id = '...'::uuid` resolves through the secondary
            // index rather than full-scan.
            Value::Uuid(b) => Some(Self::Uuid(*b)),
            // v7.17.0 Phase 3.P0-32: TIME indexable via i64 — same
            // order semantics as Date/Timestamp.
            Value::Time(us) => Some(Self::Int(*us)),
            // v7.17.0 Phase 3.P0-33: YEAR indexable as i64 — u16
            // widens losslessly and gives the natural calendar
            // ordering.
            Value::Year(y) => Some(Self::Int(i64::from(*y))),
            // v7.17.0 Phase 3.P0-34: TIMETZ indexable by its
            // UTC-equivalent microseconds (local wall - offset).
            // Without normalising, two values for the same
            // physical instant in different zones would sort
            // wrong. Matches PG's TIMETZ index behaviour.
            Value::TimeTz { us, offset_secs } => {
                Some(Self::Int(us - i64::from(*offset_secs) * 1_000_000))
            }
            // v7.17.0 Phase 3.P0-35: MONEY indexable as i64 cents
            // (no scaling needed — natural numeric ordering).
            Value::Money(c) => Some(Self::Int(*c)),
            // v7.17.0 Phase 3.P0-38: ranges are NOT indexable in
            // v7.17.0 — they'd need a custom comparator (PG uses
            // SP-GiST for this). Skip.
            Value::Range { .. } => None,
            // v7.17.0 Phase 3.P0-39: hstore is NOT indexable in
            // v7.17.0 — map columns need GIN with bespoke ops.
            Value::Hstore(_) => None,
            Value::NumericBig(_) => None,
            // v7.17.0 Phase 3.P0-40: 2D arrays aren't indexable.
            Value::IntArray2D(_) | Value::BigIntArray2D(_) | Value::TextArray2D(_) => None,
            // v7.37.5 β-P4: INTERVAL[] isn't indexable (PG uses
            // GIN/intarray for array-contains queries; SPG plans
            // that as a separate axis under v7.37.8 GIN-on-jsonb).
            Value::IntervalArray(_) => None,
            // v7.37.5 γ — none of the array-of-scalar family is
            // B-tree indexable. Same reason as IntervalArray: PG
            // serves array-contains / array-overlap queries via
            // GIN, and SPG's GIN axis lands in v7.37.8.
            Value::BoolArray(_)
            | Value::SmallIntArray(_)
            | Value::FloatArray(_)
            | Value::NumericArray(_)
            | Value::DateArray(_)
            | Value::TimestampArray(_)
            | Value::TimestamptzArray(_)
            | Value::UuidArray(_)
            | Value::JsonArray(_)
            | Value::JsonbArray(_)
            | Value::BytesArray(_)
            | Value::VarcharArray(_)
            | Value::CharArray(_)
            // v7.37.5 δ — multirange not indexable (PG uses GiST/
            // SP-GiST + a custom operator class; SPG plans the same
            // axis under v7.37.8 with ranges).
            | Value::Multirange { .. }
            // v7.37.5 ε — geometric scalars not B-tree indexable
            // (PG uses GiST/SP-GiST for these too; SPG plans the
            // same axis under v7.37.8).
            | Value::Point(_)
            | Value::Lseg(_, _)
            | Value::Path { .. }
            | Value::PgBox(_, _)
            | Value::Polygon(_)
            | Value::Line { .. }
            | Value::Circle { .. }
            // v7.37.5 ζ-A — network / bit / xml / "char" / money[].
            // INET / CIDR / MACADDR / MACADDR8 could be B-tree
            // indexable (PG does this), but the byte-wise compare
            // family-blind would mis-order IPv4 vs IPv6; left as
            // a follow-up under v7.37.8 GIN window.
            | Value::Inet { .. }
            | Value::Cidr { .. }
            | Value::Macaddr(_)
            | Value::Macaddr8(_)
            | Value::PgLsn(_)
            | Value::BitString { .. }
            | Value::Xml(_)
            | Value::Char1(_)
            | Value::MoneyArray(_)
            | Value::Composite(_)
            | Value::RegClass(..) => None,
            // Numeric isn't (yet) indexable — exact-decimal index keys
            // would need a stable scale-normalised representation.
            // Interval isn't index-eligible either (and can't reach this
            // path through column storage anyway).
            Value::Null
            | Value::Float(_)
            | Value::Vector(_)
            | Value::Sq8Vector(_)
            | Value::HalfVector(_)
            | Value::Numeric { .. }
            | Value::Interval { .. }
            | Value::Json(_)
            | Value::Bytes(_)
            | Value::TextArray(_)
            | Value::IntArray(_)
            | Value::BigIntArray(_)
            | Value::TsVector(_)
            | Value::TsQuery(_)
            | Value::Real(_) => None,
        }
    }
}

/// A single-column secondary index. v2.0 carries either a B-tree map
/// (the default — used for equality / range lookups on scalar columns)
/// or a navigable-small-world graph (used for kNN over vector
/// columns).
#[derive(Debug, Clone)]
pub struct Index {
    pub name: String,
    pub column_position: usize,
    pub kind: IndexKind,
    /// v6.8.0 — column positions of `INCLUDE (col1, col2, …)`
    /// non-key columns. Carries the planner's "this query is
    /// covered by the index" signal; lookup paths still resolve
    /// via the `RowLocator` to fetch the row body, but EXPLAIN
    /// surfaces the covered-scan annotation so operators can
    /// confirm the planner sees the coverage.
    ///
    /// Empty `Vec` = no `INCLUDE` clause (the legacy shape). v12
    /// catalog snapshots deserialise with an empty vec.
    pub included_columns: Vec<usize>,
    /// v6.8.1 — partial-index predicate stored as its canonical
    /// Display form (the engine re-parses it on the maintenance
    /// path). `None` = unconditional index (the legacy shape).
    /// Persisted as `[u8 has_pred][u16 LE len][bytes]` on the
    /// catalog snapshot (FILE_VERSION 12, appended after
    /// `included_columns`).
    pub partial_predicate: Option<String>,
    /// v6.8.2 — expression-index key, stored as the expression's
    /// canonical Display form. `None` = bare column-reference
    /// index (the legacy shape). Persisted alongside
    /// `partial_predicate` on the v12 catalog snapshot.
    pub expression: Option<String>,
    /// v7.39 (read01 round 52) — `CREATE UNIQUE INDEX … NULLS NOT DISTINCT`
    /// (PG 15+): a NULL in the key no longer exempts the row, so two
    /// all-NULL keys collide. Default `false` = SQL-standard NULLS DISTINCT.
    /// Persisted in the index appendix (FILE_VERSION 62+); older catalogs
    /// deserialise with `false`.
    pub nulls_not_distinct: bool,
    /// v7.9.29 — `CREATE UNIQUE INDEX …`. When true the engine
    /// rejects INSERTs whose key already appears in this index
    /// (combined with `partial_predicate` when present — only
    /// rows matching the predicate enter the uniqueness check).
    /// Catalog FILE_VERSION 16+; older snapshots deserialise
    /// with `false`. mailrs K1.
    pub is_unique: bool,
    /// v7.9.29 — extra (non-leading) column positions for
    /// multi-column indexes (`CREATE INDEX … (a, b, c)`). The
    /// planner today still only uses the leading
    /// `column_position` for index seeks, but UNIQUE INDEX
    /// enforcement walks the full tuple so partial-unique
    /// invariants like CalDAV `(calendar_id, uid,
    /// recurrence_id)` are enforced correctly. Catalog
    /// FILE_VERSION 16+; older snapshots deserialise empty.
    pub extra_column_positions: Vec<usize>,
}

/// Default neighbor degree (M) for the NSW graph. Picked at construction
/// time and persisted with the index.
pub const NSW_DEFAULT_M: usize = 16;

/// v5.2.2: outcome of a successful [`Catalog::freeze_oldest_to_cold`]
/// call. The catalog state has already been mutated by the time this
/// is returned (hot rows dropped + segment registered + Cold locators
/// flipped). The caller's only remaining concern is `segment_bytes` —
/// persist them to disk under `<db>.spg/segments/seg_<id>.spg` so a
/// future restart can reload via the v5.1 `SPG_PRELOAD_COLD_SEGMENT`
/// path. (v5.3's manifest will subsume this manual step.)
#[derive(Debug, Clone)]
pub struct FreezeReport {
    /// Id allocated by [`Catalog::load_segment_bytes`] for the new
    /// cold-tier segment. Stable across the call's success path.
    pub segment_id: u32,
    /// Number of rows that moved hot → cold. Equals the `max_rows`
    /// the caller asked for (the API is strict on the count).
    pub frozen_rows: usize,
    /// Hot-tier bytes reclaimed by the freeze — the
    /// [`Table::hot_bytes`] delta before vs after. Useful to feed
    /// back into the freezer's budget check on the next tick.
    pub bytes_freed: u64,
    /// Encoded segment bytes, byte-identical to what
    /// [`encode_segment`] produced. The catalog already owns a
    /// copy inside `cold_segments`; this hand-off lets the caller
    /// persist them without re-encoding.
    pub segment_bytes: Vec<u8>,
}

/// v6.7.4 — read-only output of [`Catalog::prepare_freeze_slice`].
/// Carries every row body + key in a contiguous hot-row range,
/// already encoded and sorted by PK so the coordinator's merge
/// step is a k-way merge over already-sorted streams.
///
/// `Vec<FreezeSlice>` from N independent workers feeds
/// [`Catalog::commit_freeze_slices`], which concats + encodes the
/// merged segment + atomically swaps the catalog state.
#[derive(Debug, Clone)]
pub struct FreezeSlice {
    /// Hot-row index range this slice covered (half-open, in the
    /// table's `rows: PersistentVec` ordering at call time). The
    /// commit step uses this to compute the union range that
    /// gets passed to [`Table::delete_rows`].
    pub row_range: core::ops::Range<usize>,
    /// `(pk_u64, encoded_row_body, IndexKey)` triples, sorted
    /// ascending by `pk_u64`. Per-slice sort happens inside
    /// `prepare_freeze_slice`; the coordinator does only a
    /// k-way merge to reach the global PK ordering
    /// [`encode_segment`] requires.
    pub rows: Vec<(u64, Vec<u8>, IndexKey)>,
}

/// v6.7.3 — outcome of a [`Catalog::compact_cold_segments`] call.
/// The catalog state has already been mutated when this is returned:
/// the merged segment is loaded into `cold_segments`, the source
/// segment slots are tombstoned (`None`), and every BTree-index
/// `RowLocator::Cold` that previously pointed at a source now
/// points at the merged segment. The caller's remaining job is to
/// persist `merged_segment_bytes` under
/// `<db>.spg/segments/seg_<merged_segment_id>.spg` and update the
/// in-memory `segment_id → path` map (remove the source ids, add
/// the merged id) so the next CHECKPOINT writes a manifest that
/// no longer lists the retired sources.
///
/// On a no-op (fewer than 2 candidate segments under the threshold),
/// `merged_segment_id` is `None` and `sources` is empty; the
/// catalog was not mutated.
#[derive(Debug, Clone)]
pub struct CompactReport {
    /// Source segment ids that were merged + tombstoned.
    pub sources: Vec<u32>,
    /// Id allocated for the merged segment. `None` on no-op.
    pub merged_segment_id: Option<u32>,
    /// Encoded merged-segment bytes (empty on no-op).
    pub merged_segment_bytes: Vec<u8>,
    /// Number of rows that landed in the merged segment.
    pub merged_rows: usize,
    /// `Σ source.num_rows − merged_rows`. Rows present in source
    /// segment payloads but unreferenced by any live BTree
    /// `Cold` locator — DELETE'd-but-still-frozen rows that
    /// compaction GC'd during the merge.
    pub deleted_rows_pruned: usize,
    /// `Σ source.bytes() − merged.bytes()`. Estimate of on-disk
    /// space the merge will reclaim once the source segment files
    /// are GC'd. Saturating subtract — never negative.
    pub bytes_reclaimed_estimate: u64,
}

#[derive(Debug, Clone)]
pub enum IndexKind {
    /// v4.40: structural-sharing B-tree over `IndexKey`. Replaces the v0.8
    /// `BTreeMap<IndexKey, Vec<usize>>` — `Index::clone` is now an `Arc`
    /// bump regardless of index size, so `Catalog::clone` inside the
    /// v4.34 auto-commit wrap stays O(1) even for tables with secondary
    /// indices (the case that bottlenecked v4.39 at 1M rows in the
    /// sweep).
    ///
    /// v5.1: value type widened from `Vec<usize>` to `Vec<RowLocator>` so
    /// a single key can point to a mix of hot-tier rows (`RowLocator::Hot`,
    /// equivalent to the pre-v5 `usize` row index) and cold-tier rows
    /// (`RowLocator::Cold { segment_id, page_offset }`) once the v5.2
    /// freezer starts producing them. Pre-v5.2 only `Hot` entries appear
    /// — the on-disk encoding stays at `FILE_VERSION` 8 (raw u64 row index)
    /// because every locator round-trips through `RowLocator::from_legacy_v8_u64`
    /// without information loss. `FILE_VERSION` 9 with tagged encoding lands
    /// alongside the first freezer commit (v5.1 step 2b / v5.2).
    BTree(PersistentBTreeMap<IndexKey, Vec<RowLocator>>),
    /// Navigable-small-world graph for vector kNN search.
    Nsw(NswGraph),
    /// v6.7.1 — BRIN (Block Range INdex). Pure metadata: BRIN
    /// indexes carry NO in-memory key→locator map. The (min,
    /// max) summaries live in each cold-tier segment's v2
    /// envelope sidecar; the BRIN entry in `Table.indices` only
    /// records THAT a BRIN index exists on this column so the
    /// segment encoder + planner can opt into the summary path.
    Brin {
        /// The cell type at `column_position` at CREATE INDEX time.
        /// Used by the planner to type-check WHERE-clause range
        /// predicates against the BRIN-indexed column.
        column_type: DataType,
    },
    /// v7.12.3 — GIN inverted index over a `tsvector` column.
    ///
    /// Storage shape: `lexeme word → Vec<RowLocator>`. The posting
    /// list per word is appended in row-order, so range scans are
    /// O(matching rows) once the per-word lookup is done. Multi-
    /// term queries intersect / union posting lists.
    ///
    /// `IndexKey::from_value(TsVector)` returns `None` — GIN doesn't
    /// participate in `try_index_seek` (which is BTree-equality-keyed).
    /// The engine consults this index through `try_gin_lookup` on
    /// `WHERE col @@ tsquery` predicates instead.
    ///
    /// Backed by a `PersistentBTreeMap` so `Catalog::clone` (the
    /// per-write snapshot) stays O(1) — same structural-sharing
    /// invariant as BTree.
    Gin(PersistentBTreeMap<alloc::string::String, Vec<RowLocator>>),
    /// v7.15.0 — `USING gin (col gin_trgm_ops)` over a `TEXT`
    /// column. Posting lists map `trigram` (PG-compatible 3-byte
    /// shingle on the lower-cased + space-padded input) to row
    /// locators. The planner uses this index to accelerate
    /// `WHERE col LIKE '…'` / `ILIKE '…'` / `similarity(col, q) >
    /// t` — every literal run of length ≥ 1 in the pattern
    /// produces a trigram set, the engine intersects the posting
    /// lists, and the LIKE / similarity predicate is re-evaluated
    /// per candidate row to filter the over-approximation.
    /// Persisted via tag-4 index payload in `FILE_VERSION` 24+.
    GinTrgm(PersistentBTreeMap<alloc::string::String, Vec<RowLocator>>),
    /// v7.17.0 Phase 2.2 — MySQL `FULLTEXT KEY (col)` over a
    /// `TEXT` / `VARCHAR` column. Posting lists map
    /// `tsvector('simple') lexeme` to row locators. At insert /
    /// build time the engine derives the lexemes from the cell
    /// via the same lower-case tokenisation rule as
    /// `to_tsvector('simple', ...)` — the column itself stays a
    /// plain text type on disk (mysqldump round-trips would be
    /// broken otherwise). The planner uses this index to
    /// accelerate MySQL-shape `MATCH(col) AGAINST('term')`
    /// queries by mapping them onto the existing tsquery `@@`
    /// walker. Persisted via tag-5 index payload in
    /// `FILE_VERSION` 33+.
    GinFulltext(PersistentBTreeMap<alloc::string::String, Vec<RowLocator>>),
    /// v7.37.8(sentori Epic 5 P2)— `USING gin (col)` over a
    /// `JSON` / `JSONB` column. Posting lists map a canonical
    /// `(path, leaf)` token(see [`crate::jsonb_gin::extract_tokens`])
    /// to row locators so the planner can resolve
    /// `<col> @> <jsonb_literal>` to a candidate row set via
    /// posting-list intersection + per-row `json::contains`
    /// re-verification. Pre-7.37.8 the same DDL loaded as a
    /// BTree fallback so `pg_dump` JSONB-GIN scripts kept loading
    /// without query-time acceleration. Persisted via tag-6 index
    /// payload in `FILE_VERSION` 51+.
    GinJsonb(PersistentBTreeMap<alloc::string::String, Vec<RowLocator>>),
}

impl IndexKind {
    /// v7.31 (memory campaign, C2) — bytes this index variant holds
    /// resident in RAM, computed by walking its OWN structure rather
    /// than a parametric guess made by the engine. Replaces the old
    /// `spg_admin::memory_stats` inline match, which charged NSW with
    /// a stale `m_max_0 * 8` per node (neighbour slots are `u32` = 4 B
    /// since v6.1.x, and most nodes never fill `m_max_0`) and lumped
    /// every GIN family index into a flat 1 KiB token — a gross
    /// undercount for the text-heavy posting lists that dominate
    /// mailrs' footprint. Per-entry container overhead uses the
    /// 3-word (24 B on 64-bit) `Vec`/`String` header as the charge.
    ///
    /// O(index entries): operator/monitoring surface (`memory_stats` /
    /// `spg_memory_stats`), not a query path.
    #[must_use]
    pub fn approx_resident_bytes(&self) -> u64 {
        const HEADER: usize = 24; // Vec/String 3-word header on 64-bit.
        let loc = core::mem::size_of::<RowLocator>();
        match self {
            IndexKind::BTree(map) => {
                let key = core::mem::size_of::<IndexKey>();
                map.iter()
                    .map(|(_, locs)| (key + HEADER + locs.len() * loc) as u64)
                    .sum()
            }
            IndexKind::Nsw(g) => {
                // `levels` is one byte per node; each layer's adjacency
                // is a `Vec<u32>` per node whose actual length we walk
                // (the dense layer-0 list dominates, but upper layers
                // are sparse — the old estimate ignored that).
                let mut b = g.levels.len() as u64;
                for layer in &g.layers {
                    for nbrs in layer.iter() {
                        b += (HEADER + nbrs.len() * core::mem::size_of::<u32>()) as u64;
                    }
                }
                b
            }
            // BRIN carries NO in-memory key→locator map (the (min,max)
            // summaries live in cold-segment sidecars on disk); the
            // resident footprint is just the column-type token.
            IndexKind::Brin { .. } => core::mem::size_of::<DataType>() as u64,
            IndexKind::Gin(map)
            | IndexKind::GinTrgm(map)
            | IndexKind::GinFulltext(map)
            | IndexKind::GinJsonb(map) => map
                .iter()
                .map(|(word, postings)| {
                    (word.len() + HEADER + HEADER + postings.len() * loc) as u64
                })
                .sum(),
        }
    }
}

/// Multi-layer HNSW graph (v2.13). Each node is assigned a `top_level`;
/// it appears in layers `0..=top_level`. Higher layers are sparser, so
/// search starts from the entry at the top layer, greedy-descends to
/// layer 0, and beam-searches there. Layer 0 keeps a larger neighbour
/// budget (`m_max_0 = 2 * m` per the HNSW paper); upper layers cap at
/// `m`. The struct name stays `NswGraph` so external users / on-disk
/// callers don't have to track a rename — the algorithm changed, the
/// data slot didn't.
#[derive(Debug, Clone)]
pub struct NswGraph {
    /// Max neighbours per node on layers ≥ 1.
    pub m: usize,
    /// Max neighbours on layer 0 (the dense bottom layer). HNSW
    /// convention: `m_max_0 = 2 * m`.
    pub m_max_0: usize,
    /// Entry point — the node that sits on the topmost layer. Search
    /// always starts here.
    pub entry: Option<usize>,
    /// Top layer of the entry node (== `layers.len() - 1` when populated).
    pub entry_level: u8,
    /// `levels[i]` = top layer of node `i`. Nodes whose vector cell is
    /// NULL / non-Vector have `levels[i] = 0` and no neighbour entries.
    ///
    /// v5.5.0: backed by `PersistentVec` so `NswGraph::clone` (and the
    /// `Catalog::clone` on every group-commit write that contains it) is O(1)
    /// structural-sharing instead of an O(N) element copy.
    pub levels: PersistentVec<u8>,
    /// `layers[l][i]` = neighbours of node `i` at layer `l`. Inner vec
    /// is empty when node `i` doesn't reach layer `l`.
    ///
    /// v5.5.0: the per-node middle dimension (the O(N) one) is a
    /// `PersistentVec`; the outer layer dimension stays a plain `Vec`
    /// (layer count ≤ 8, so its clone is O(1) in practice) and the inner
    /// neighbour list stays a `Vec` (bounded by `m_max_0`).
    ///
    /// v6.1.x: neighbour slot widened from `usize` (8 B on 64-bit) to
    /// `u32` (4 B). Row indices are catalog-bounded by `u32::MAX` (4G
    /// rows per table); the cast at the NSW boundary asserts this. At
    /// 1M dim-128 SQ8, layer 0 adjacency alone shrinks by ~128 MiB
    /// — the largest single contribution to the v6.0.5-measured
    /// 624 MiB ambition gap. On-disk format already used u32 LE, so
    /// this is a pure in-memory layout change; no `FILE_VERSION` bump.
    pub layers: Vec<PersistentVec<Vec<u32>>>,
}

impl NswGraph {
    fn new(m: usize) -> Self {
        Self {
            m,
            m_max_0: m.saturating_mul(2),
            entry: None,
            entry_level: 0,
            levels: PersistentVec::new(),
            layers: alloc::vec![PersistentVec::new()],
        }
    }

    /// Max-neighbour budget for layer `l`.
    pub const fn cap_for_layer(&self, layer: u8) -> usize {
        if layer == 0 { self.m_max_0 } else { self.m }
    }
}

/// Deterministic level assignment, seeded on the row index so the same
/// insert order reproduces the same topology. Distribution is roughly
/// HNSW-flavoured with `mL ≈ 1/ln(M) ≈ 0.36` for M=16: each 4-bit
/// chunk that comes up zero promotes the node one layer (so P(level ≥
/// L) ≈ (1/16)^L).
#[allow(clippy::verbose_bit_mask)] // clippy suggests trailing_zeros(); we need an explicit MAX cap and a stable distribution shape.
pub fn nsw_assign_level(row_idx: usize) -> u8 {
    const MAX_LEVEL: u8 = 7; // 7 ⇒ ~16^7 ≈ 2.7e8 expected nodes between promotions; ample.
    // SplitMix-style mixer — cheap and seedable.
    let mut x = (row_idx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    // Count contiguous low-end zero nibbles (4-bit chunks). Each zero
    // nibble has probability 1/16, mirroring HNSW's `mL ≈ 1/ln(M)` for
    // M=16. `trailing_zeros / 4` would lose the ordering when x = 0, so
    // a plain loop with a cap is clearer.
    let mut level: u8 = 0;
    while x & 0xF == 0 && level < MAX_LEVEL {
        level += 1;
        x >>= 4;
    }
    level
}

impl Index {
    fn new_btree(name: String, column_position: usize) -> Self {
        Self {
            name,
            column_position,
            kind: IndexKind::BTree(PersistentBTreeMap::new()),
            included_columns: Vec::new(),
            partial_predicate: None,
            expression: None,
            is_unique: false,
            nulls_not_distinct: false,
            extra_column_positions: Vec::new(),
        }
    }

    fn new_nsw(name: String, column_position: usize, m: usize) -> Self {
        Self {
            name,
            column_position,
            kind: IndexKind::Nsw(NswGraph::new(m)),
            included_columns: Vec::new(),
            partial_predicate: None,
            expression: None,
            is_unique: false,
            nulls_not_distinct: false,
            extra_column_positions: Vec::new(),
        }
    }

    /// v6.7.1 — BRIN index constructor. BRIN carries no in-memory
    /// data; the `column_type` snapshot is used by the segment
    /// encoder + planner for type-checking range predicates.
    fn new_brin(name: String, column_position: usize, column_type: DataType) -> Self {
        Self {
            name,
            column_position,
            kind: IndexKind::Brin { column_type },
            included_columns: Vec::new(),
            partial_predicate: None,
            expression: None,
            is_unique: false,
            nulls_not_distinct: false,
            extra_column_positions: Vec::new(),
        }
    }

    /// v7.12.3 — GIN inverted-index constructor. Empty posting-list
    /// map; caller (typically [`Table::add_gin_index`] or
    /// [`Table::restore_gin_index`]) populates it from existing rows
    /// or from a deserialised snapshot.
    fn new_gin(name: String, column_position: usize) -> Self {
        Self {
            name,
            column_position,
            kind: IndexKind::Gin(PersistentBTreeMap::new()),
            included_columns: Vec::new(),
            partial_predicate: None,
            expression: None,
            is_unique: false,
            nulls_not_distinct: false,
            extra_column_positions: Vec::new(),
        }
    }

    /// v7.15.0 — `gin_trgm_ops`-flavoured GIN constructor. Same
    /// shape as `new_gin` but the posting-list keys are 3-byte
    /// trigram shingles (`pg_trgm`-compatible) and the column
    /// type is `TEXT` / `VARCHAR` (not `TSVECTOR`).
    fn new_gin_trgm(name: String, column_position: usize) -> Self {
        Self {
            name,
            column_position,
            kind: IndexKind::GinTrgm(PersistentBTreeMap::new()),
            included_columns: Vec::new(),
            partial_predicate: None,
            expression: None,
            is_unique: false,
            nulls_not_distinct: false,
            extra_column_positions: Vec::new(),
        }
    }

    /// v7.17.0 Phase 2.2 — MySQL `FULLTEXT KEY` GIN constructor.
    /// Same shape as `new_gin_trgm` but the posting-list keys
    /// are lower-cased word lexemes (`to_tsvector('simple', col)`
    /// equivalent) instead of trigrams, and the column type is
    /// `TEXT` / `VARCHAR` (not `TSVECTOR`).
    fn new_gin_fulltext(name: String, column_position: usize) -> Self {
        Self {
            name,
            column_position,
            kind: IndexKind::GinFulltext(PersistentBTreeMap::new()),
            included_columns: Vec::new(),
            partial_predicate: None,
            expression: None,
            is_unique: false,
            nulls_not_distinct: false,
            extra_column_positions: Vec::new(),
        }
    }

    /// v7.37.8(sentori Epic 5 P2)— JSONB-GIN constructor. Same
    /// shape as the other GIN-family indexes; posting-list keys
    /// are the canonical `(path, leaf)` tokens emitted by
    /// `crate::jsonb_gin::extract_tokens`. Maintains posting
    /// lists from `Value::Json` cells(JSONB is a synonym for the
    /// same in-memory string-backed Value).
    fn new_gin_jsonb(name: String, column_position: usize) -> Self {
        Self {
            name,
            column_position,
            kind: IndexKind::GinJsonb(PersistentBTreeMap::new()),
            included_columns: Vec::new(),
            partial_predicate: None,
            expression: None,
            is_unique: false,
            nulls_not_distinct: false,
            extra_column_positions: Vec::new(),
        }
    }

    /// v7.34.4 — descending-order iterator over `(IndexKey, locators)`
    /// pairs for a BTree index, with O(log N) descent to the rightmost
    /// leaf and lazy emission thereafter. Returns an empty iterator
    /// for non-BTree index kinds — callers handle both uniformly.
    /// Used by the ORDER BY `<indexed col>` DESC + LIMIT N executor
    /// path: walking only the first N matches off the rightmost leaf
    /// avoids the per-row materialisation + partial-sort cost on
    /// large tables (mailrs `content_worker` at 250 k rows).
    pub fn iter_desc(
        &self,
    ) -> alloc::boxed::Box<dyn Iterator<Item = (&IndexKey, &alloc::vec::Vec<RowLocator>)> + '_>
    {
        match &self.kind {
            IndexKind::BTree(m) => alloc::boxed::Box::new(m.iter_rev()),
            IndexKind::Nsw(_)
            | IndexKind::Brin { .. }
            | IndexKind::Gin(_)
            | IndexKind::GinTrgm(_)
            | IndexKind::GinFulltext(_)
            | IndexKind::GinJsonb(_) => alloc::boxed::Box::new(core::iter::empty()),
        }
    }

    /// v7.34.4 — ascending-order iterator over `(IndexKey, locators)`
    /// pairs. Mirror of `iter_desc` for ORDER BY ... ASC + LIMIT N.
    pub fn iter_asc(
        &self,
    ) -> alloc::boxed::Box<dyn Iterator<Item = (&IndexKey, &alloc::vec::Vec<RowLocator>)> + '_>
    {
        match &self.kind {
            IndexKind::BTree(m) => alloc::boxed::Box::new(m.iter()),
            IndexKind::Nsw(_)
            | IndexKind::Brin { .. }
            | IndexKind::Gin(_)
            | IndexKind::GinTrgm(_)
            | IndexKind::GinFulltext(_)
            | IndexKind::GinJsonb(_) => alloc::boxed::Box::new(core::iter::empty()),
        }
    }

    /// Look up the locators stored under `key` (B-tree only). Returns
    /// an empty slice when the key is absent or the index isn't a
    /// BTree — callers can treat both cases uniformly.
    ///
    /// v5.1: return type widened from `&[usize]` to `&[RowLocator]`.
    /// Pre-v5.2 callers can read the slice and `.as_hot().unwrap()`
    /// each entry (no `Cold` variants exist until the freezer lands);
    /// post-v5.2 callers dispatch hot vs. cold per locator.
    pub fn lookup_eq(&self, key: &IndexKey) -> &[RowLocator] {
        match &self.kind {
            IndexKind::BTree(m) => m.get(key).map_or(&[][..], Vec::as_slice),
            // BRIN / NSW / GIN / trigram-GIN / fulltext-GIN have
            // no IndexKey-keyed map; lookup is a no-op. GIN uses
            // [`Index::gin_lookup_word`] instead.
            IndexKind::Nsw(_)
            | IndexKind::Brin { .. }
            | IndexKind::Gin(_)
            | IndexKind::GinTrgm(_)
            | IndexKind::GinFulltext(_)
            | IndexKind::GinJsonb(_) => &[][..],
        }
    }

    /// v7.37.43 (INSUBQ B-2) — specialised lookup for integer-PK probes.
    /// `try_count_star_pk_in_subquery_fast` already holds an `i64` (the
    /// inner survivor key); skip the `IndexKey::from_value` enum-dispatch
    /// trip and build the key inline. ~20 ns × N_survivors saved on
    /// the INSUBQ hot loop.
    #[inline]
    pub fn lookup_eq_i64(&self, n: i64) -> &[RowLocator] {
        match &self.kind {
            IndexKind::BTree(m) => m.get(&IndexKey::Int(n)).map_or(&[][..], Vec::as_slice),
            IndexKind::Nsw(_)
            | IndexKind::Brin { .. }
            | IndexKind::Gin(_)
            | IndexKind::GinTrgm(_)
            | IndexKind::GinFulltext(_)
            | IndexKind::GinJsonb(_) => &[][..],
        }
    }

    /// v7.38 (perf, index range scan) — flatten the row locators for every key
    /// in `[lo, hi]` (bounds per `core::ops::Bound`) via the BTree's `O(log N +
    /// k)` range walk. Returns `None` once more than `cap` locators accumulate
    /// — a "this range isn't selective enough, seq-scan instead" signal that
    /// stops a wide range from materialising a near-full table's worth of rows
    /// through the index. BTree only (other kinds → None).
    pub fn lookup_range_capped(
        &self,
        lo: core::ops::Bound<&IndexKey>,
        hi: core::ops::Bound<&IndexKey>,
        cap: usize,
    ) -> Option<Vec<RowLocator>> {
        match &self.kind {
            IndexKind::BTree(m) => {
                let mut out: Vec<RowLocator> = Vec::new();
                for (_, locs) in m.range(lo, hi) {
                    out.extend(locs.iter().copied());
                    if out.len() > cap {
                        return None;
                    }
                }
                Some(out)
            }
            IndexKind::Nsw(_)
            | IndexKind::Brin { .. }
            | IndexKind::Gin(_)
            | IndexKind::GinTrgm(_)
            | IndexKind::GinFulltext(_)
            | IndexKind::GinJsonb(_) => None,
        }
    }

    /// v7.12.3 — GIN posting-list lookup. Returns the row locators
    /// whose `tsvector` cell contains `word`. Empty when the word is
    /// absent from the index or this isn't a GIN index.
    pub fn gin_lookup_word(&self, word: &str) -> &[RowLocator] {
        match &self.kind {
            // v7.17.0 Phase 2.2 — fulltext-GIN shares the same
            // lexeme-keyed posting list shape as the
            // tsvector-typed GIN, so the same lookup applies.
            IndexKind::Gin(m) | IndexKind::GinFulltext(m) => {
                m.get(&String::from(word)).map_or(&[][..], Vec::as_slice)
            }
            IndexKind::BTree(_)
            | IndexKind::Nsw(_)
            | IndexKind::Brin { .. }
            | IndexKind::GinTrgm(_)
            | IndexKind::GinJsonb(_) => &[][..],
        }
    }

    /// v7.15.0 — trigram-GIN posting-list lookup. Returns the row
    /// locators whose indexed `TEXT` cell contains the trigram
    /// `tri`. Empty when the trigram is absent or this isn't a
    /// trigram-GIN index.
    pub fn gin_trgm_lookup(&self, tri: &str) -> &[RowLocator] {
        match &self.kind {
            IndexKind::GinTrgm(m) => m.get(&String::from(tri)).map_or(&[][..], Vec::as_slice),
            IndexKind::BTree(_)
            | IndexKind::Nsw(_)
            | IndexKind::Brin { .. }
            | IndexKind::Gin(_)
            | IndexKind::GinFulltext(_)
            | IndexKind::GinJsonb(_) => &[][..],
        }
    }

    /// v7.37.8(sentori Epic 5 P2)— JSONB-GIN posting-list lookup.
    /// Returns the row locators whose indexed JSONB cell carries
    /// the canonical `token`(see [`crate::jsonb_gin::extract_tokens`]).
    /// Empty when the token is absent or this isn't a JSONB-GIN
    /// index. Planners drive `<col> @> <jsonb_literal>` through here.
    pub fn gin_jsonb_lookup(&self, token: &str) -> &[RowLocator] {
        match &self.kind {
            IndexKind::GinJsonb(m) => m.get(&String::from(token)).map_or(&[][..], Vec::as_slice),
            IndexKind::BTree(_)
            | IndexKind::Nsw(_)
            | IndexKind::Brin { .. }
            | IndexKind::Gin(_)
            | IndexKind::GinTrgm(_)
            | IndexKind::GinFulltext(_) => &[][..],
        }
    }

    /// Borrow the NSW graph (if this is an NSW index). Callers that need
    /// the graph for a kNN search go through here.
    pub const fn nsw(&self) -> Option<&NswGraph> {
        match &self.kind {
            IndexKind::Nsw(g) => Some(g),
            IndexKind::BTree(_)
            | IndexKind::Brin { .. }
            | IndexKind::Gin(_)
            | IndexKind::GinTrgm(_)
            | IndexKind::GinFulltext(_)
            | IndexKind::GinJsonb(_) => None,
        }
    }

    /// v6.7.1 — true when this index is a BRIN (block range) index.
    /// Used by the segment encoder to opt into BRIN sidecar emission
    /// at freeze time, and by the planner to opt into page-skipping
    /// on range predicates.
    pub const fn is_brin(&self) -> bool {
        matches!(self.kind, IndexKind::Brin { .. })
    }

    /// v7.15.0 — true when this index is a trigram GIN
    /// (`gin_trgm_ops`-flavoured). Used by the LIKE planner to
    /// opt into trigram acceleration.
    pub const fn is_gin_trgm(&self) -> bool {
        matches!(self.kind, IndexKind::GinTrgm(_))
    }

    /// v7.12.3 — true when this index is a GIN inverted index.
    /// Used by the planner to opt into posting-list acceleration on
    /// `WHERE col @@ tsquery` predicates.
    pub const fn is_gin(&self) -> bool {
        matches!(self.kind, IndexKind::Gin(_))
    }

    /// v7.17.0 Phase 2.2 — true when this index is a fulltext
    /// GIN over a TEXT / VARCHAR column (MySQL `FULLTEXT KEY`
    /// surface). Used by the planner to opt the FULLTEXT-indexed
    /// column into MATCH AGAINST acceleration.
    pub const fn is_gin_fulltext(&self) -> bool {
        matches!(self.kind, IndexKind::GinFulltext(_))
    }

    /// v7.37.8(sentori Epic 5 P2)— true when this index is a
    /// real JSONB-GIN(posting-list backed). Used by the planner
    /// to opt `<col> @> <jsonb_literal>` into posting-list seek.
    pub const fn is_gin_jsonb(&self) -> bool {
        matches!(self.kind, IndexKind::GinJsonb(_))
    }
}

/// In-memory table: schema + a persistent row vector + secondary indices.
///
/// v4.39: `rows` is a [`PersistentVec`] (Bitmapped Vector Trie, 32-way) so
/// `Table::clone()` is `O(1)` — the whole reason for v4.39's existence is
/// to make `Catalog::clone()` cheap inside the v4.34 auto-commit wrap.
///
/// v5.2.1: `hot_bytes` tracks the encoded byte size of every row currently
/// in [`Self::rows`], summed over rows. Updated incrementally by `insert`
/// (+= encoded row size), `delete_rows` (-= removed rows' encoded sizes),
/// and `update_row` (-= old size, += new size). The value is what the
/// v5.2 freezer reads to decide when to demote cold rows — when the
/// catalog-wide sum crosses `SPG_HOT_TIER_BYTES` (default 4 GiB) the
/// freezer thread wakes. v5.2.1 ships measurement only; the freezer
/// itself lands in v5.2.2. Stored as `u64` so a single field clone in
/// `Catalog::clone` stays at the O(1) invariant v4.39 built.
/// v7.34 (crash-recovery P0 #2) — one row-level physical redo record.
/// Row-level redo replaces statement-based WAL replay (which re-executes
/// each SQL through the full engine — O(records × catalog_rows), the
/// superlinear recovery hang root-caused on the mailrs crash-recovery
/// P0). A `RowChange` is the exact storage mutation the engine applied
/// (`Table::insert` / `update_row` / `delete_rows`); replaying it on a
/// catalog restored from the matching checkpoint reproduces the state
/// WITHOUT re-validating uniqueness/FK/parse/plan — O(changed rows).
///
/// Positions are physical, not key-based: `serialize`/`deserialize`
/// preserve row order exactly (rows written + read back in `self.rows`
/// order) and the mutation ops are deterministic, so the same op sequence
/// replayed from the same checkpoint reproduces the same positions. This
/// matches PostgreSQL's physical redo and supports tables with no primary
/// key. (Caveat handled at replay integration: a post-checkpoint cold-tier
/// freeze shifts hot positions and must itself be logged or fenced by a
/// checkpoint — see `row-level-redo-design`.)
/// ## v7.37.15 (Epic W slice 1) — additive MVCC identity metadata
///
/// Each variant now also carries, additively, the stable
/// [`RowId`](row_header::RowId) of the affected row(s) and the
/// **writer version** (`xmin` for an insert, `xmax` for a
/// delete/update). This is the codec foundation for making
/// in-place MVCC tombstones durable across crash/upgrade recovery.
///
/// Two important properties for the durability path:
///
/// 1. **Replay resolution is UNCHANGED.** `apply_redo_run_on_table`
///    still resolves every change by physical `pos`/`positions`
///    exactly as before. The new metadata is *carried but unused*
///    by replay in this slice; resolving-by-`RowId` and
///    header-preserving replay are later slices.
/// 2. **Backward compatibility.** A redo payload written by
///    pre-Epic-W code carries no metadata; [`decode_redo_log`]
///    fills `rowid`/`rowids` with [`RowId::UNASSIGNED`](row_header::RowId::UNASSIGNED)
///    (empty for `Delete`) and `writer_version` with `0`. See the
///    codec version gate in [`encode_redo_log`]/[`decode_redo_log`].
///
/// The `writer_version` is captured as `0` at the storage layer
/// (`Table::insert`/`delete_rows`/`update_row` don't have the
/// committing `TxId`), then **stamped with the real committing
/// version by the engine** after it drains the statement's changes
/// (Epic W slice 2 — [`RowChange::set_writer_version`], driven from
/// `Engine::writer_version_for_current_stmt`). All changes from one
/// statement share the one version. Replay still resolves by
/// physical position and does not read `writer_version` — that is a
/// later slice (header-preserving replay).
#[derive(Debug, Clone, PartialEq)]
pub enum RowChange {
    /// Append `row` to `table`.
    Insert {
        table: String,
        row: Row<'static>,
        /// Epic W: stable id the appended row will receive.
        /// [`RowId::UNASSIGNED`](row_header::RowId::UNASSIGNED) when
        /// decoded from a pre-Epic-W redo payload.
        rowid: row_header::RowId,
        /// Epic W: writer version (`xmin`). `0` until the writing
        /// `TxId` is threaded to the storage layer (later slice).
        writer_version: u64,
    },
    /// Replace the row at physical `pos` in `table` with `new_row`.
    Update {
        table: String,
        pos: usize,
        new_row: Vec<Value<'static>>,
        /// Epic W: stable id of the row at `pos`.
        /// [`RowId::UNASSIGNED`](row_header::RowId::UNASSIGNED) when
        /// decoded from a pre-Epic-W redo payload.
        rowid: row_header::RowId,
        /// Epic W: writer version (`xmax` of the superseded tuple).
        /// `0` until the writing `TxId` is threaded (later slice).
        writer_version: u64,
    },
    /// Remove the rows at the given physical `positions` from `table`.
    Delete {
        table: String,
        positions: Vec<usize>,
        /// Epic W: stable ids parallel to `positions` (same length,
        /// [`RowId::UNASSIGNED`](row_header::RowId::UNASSIGNED) for an
        /// out-of-bounds input position). **Empty** when decoded from
        /// a pre-Epic-W redo payload (no metadata was recorded).
        rowids: Vec<row_header::RowId>,
        /// Epic W: writer version (`xmax`). `0` until the writing
        /// `TxId` is threaded to the storage layer (later slice).
        writer_version: u64,
    },
    /// v7.37.15 (Epic W durable-tombstone slice) — an **in-place MVCC
    /// delete**: the row(s) named by `rowids` are NOT physically
    /// removed; their header `xmax` is stamped so newer snapshots stop
    /// seeing them (vacuum reclaims later). This is the redo shape of
    /// the gate-on (`SPG_MVCC_INPLACE`) DELETE / UPDATE-old-version /
    /// ON-CONFLICT paths, which call [`Table::mark_row_deleted`]
    /// instead of `delete_rows`.
    ///
    /// Unlike `Delete`, the target is named by **stable `RowId`**, not
    /// physical position: a tombstone keeps the slot, so position would
    /// be ambiguous after later compaction, and the header-preserving
    /// replay must re-find the exact row the writer tombstoned. On
    /// replay the id is matched against the ids the same redo run
    /// produced (an `Insert`'s `rowid`, or the table's ids snapshotted
    /// at run start); an id that cannot be resolved is skipped and
    /// counted (see `apply_redo_run_on_table`) — this is the documented
    /// cross-checkpoint limitation until the V6 envelope persists ids.
    Tombstone {
        table: String,
        /// Stable ids of the tombstoned rows (from `self.rowids()[pos]`
        /// at capture). Never empty for a recorded tombstone.
        rowids: Vec<row_header::RowId>,
        /// The version stamped into each target row's header `xmax`
        /// (the deleting statement's writer version).
        xmax: u64,
    },
}

impl RowChange {
    /// v7.37.15 (Epic W slice 2) — stamp the committing writer
    /// version onto this change. Every change drained from a single
    /// statement shares one version (the statement's `xmin`/`xmax`),
    /// so the engine calls this on each drained change with the value
    /// from [`Engine::writer_version_for_current_stmt`]. Additive
    /// metadata only: replay still resolves by physical position and
    /// does not read `writer_version` (that is a later slice).
    pub fn set_writer_version(&mut self, v: u64) {
        match self {
            RowChange::Insert { writer_version, .. }
            | RowChange::Update { writer_version, .. }
            | RowChange::Delete { writer_version, .. } => *writer_version = v,
            // A tombstone captures `xmax` directly from the deleting
            // statement's version at record time (via
            // `mark_row_deleted`), so it already equals `v`. Keep the
            // "one statement, one version" invariant mechanical by
            // asserting agreement in debug builds rather than silently
            // overwriting a possibly-different value.
            RowChange::Tombstone { xmax, .. } => {
                debug_assert_eq!(
                    *xmax, v,
                    "tombstone xmax must match the statement writer version"
                );
                *xmax = v;
            }
        }
    }
}

/// v7.37.15 (Epic W slice 1) — leading marker byte of the
/// metadata-carrying redo layout. A **pre-Epic-W** redo payload leads
/// with `FILE_VERSION` (8..=52 today, rising ~1 per release); this
/// marker is `0xFF` and can therefore never collide with a real
/// `FILE_VERSION`, so [`decode_redo_log`] tells the two layouts apart
/// by inspecting the first byte alone. The compile-time assertion
/// below makes the "never collide" invariant a hard build gate: if
/// `FILE_VERSION` ever climbs toward `0xFF` the build breaks and forces
/// a redesign long before an ambiguity could ship.
const REDO_META_MARKER: u8 = 0xFF;
/// v7.37.15 (Epic W slice 1) — version of the metadata-carrying redo
/// layout that follows [`REDO_META_MARKER`]. Bumped when the per-change
/// metadata shape changes; an unknown value is a hard decode error.
const REDO_META_VERSION: u8 = 1;

/// v7.37.15 (Epic W durable-tombstone slice) — process-wide count of
/// [`RowChange::Tombstone`] targets that `apply_redo` could NOT resolve
/// to a row by `RowId`. A non-zero value is expected only across a
/// checkpoint boundary (the table's ids are reassigned on deserialize
/// and the V6 envelope does not yet persist them), where a tombstone
/// naming a pre-checkpoint row is left visible rather than mis-applied.
/// Surfaced for observability; never affects correctness of the resolved
/// tombstones. Read via [`unresolved_tombstone_count`].
static UNRESOLVED_TOMBSTONES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// v7.39 (flip crash-replay P0) — observability read for the replay
/// tombstones that could not be resolved to a row (each one is a
/// resurrected delete).
#[must_use]
pub fn unresolved_tombstones() -> u64 {
    UNRESOLVED_TOMBSTONES.load(core::sync::atomic::Ordering::Relaxed)
}

/// v7.37.15 (Epic W durable-tombstone slice) — read the process-wide
/// count of redo tombstones that could not be resolved to a row by
/// `RowId` during `apply_redo`. See [`UNRESOLVED_TOMBSTONES`].
#[must_use]
pub fn unresolved_tombstone_count() -> u64 {
    UNRESOLVED_TOMBSTONES.load(core::sync::atomic::Ordering::Relaxed)
}
// Provably-unambiguous old/new distinction: the pre-Epic-W layout's
// first byte is `FILE_VERSION`, which must stay strictly below the
// marker forever.
const _: () = assert!(FILE_VERSION < REDO_META_MARKER);

/// v7.34 (crash-recovery P0 #2), extended v7.37.15 (Epic W slice 1) —
/// encode a row-level redo log to bytes for a WAL record.
///
/// ## Layout (Epic W metadata-carrying form, always emitted now)
///
/// `[u8 REDO_META_MARKER=0xFF][u8 REDO_META_VERSION][u8 FILE_VERSION]
/// [u32 count]` then per change `[u8 op][str table]` and, per op:
/// - `Insert [u32 n][value×n][u64 rowid][u64 writer_version]`
/// - `Update [u32 pos][u32 n][value×n][u64 rowid][u64 writer_version]`
/// - `Delete [u32 n][u32 pos×n][u64 rowid×n][u64 writer_version]`
/// - `Tombstone [u32 n][u64 rowid×n][u64 xmax]` (op byte 3; only ever
///   emitted under the metadata-carrying layout — the pre-Epic-W layout
///   had no in-place tombstone, so a legacy stream can never carry it)
///
/// Positions are physical (u32 ≤ 4 G rows). The `FILE_VERSION` byte
/// still rides along (now the 3rd byte) so the value codec decodes
/// string / BYTEA escapes exactly as before.
///
/// ## Backward compatibility
///
/// The **pre-Epic-W** layout was `[u8 FILE_VERSION][u32 count]…` with
/// no per-change metadata. [`decode_redo_log`] still decodes that form
/// (first byte < `0xFF`) byte-for-byte identically — every WAL file
/// written by released code replays unchanged.
#[must_use]
pub fn encode_redo_log(changes: &[RowChange]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(REDO_META_MARKER);
    out.push(REDO_META_VERSION);
    out.push(FILE_VERSION);
    codec::write_u32(&mut out, changes.len() as u32);
    let write_values = |out: &mut Vec<u8>, vals: &[Value<'static>]| {
        codec::write_u32(out, vals.len() as u32);
        for v in vals {
            codec::write_value(out, v);
        }
    };
    for change in changes {
        match change {
            RowChange::Insert {
                table,
                row,
                rowid,
                writer_version,
            } => {
                out.push(0);
                codec::write_str(&mut out, table);
                write_values(&mut out, &row.values);
                codec::write_u64(&mut out, rowid.0);
                codec::write_u64(&mut out, *writer_version);
            }
            RowChange::Update {
                table,
                pos,
                new_row,
                rowid,
                writer_version,
            } => {
                out.push(1);
                codec::write_str(&mut out, table);
                codec::write_u32(&mut out, *pos as u32);
                write_values(&mut out, new_row);
                codec::write_u64(&mut out, rowid.0);
                codec::write_u64(&mut out, *writer_version);
            }
            RowChange::Delete {
                table,
                positions,
                rowids,
                writer_version,
            } => {
                out.push(2);
                codec::write_str(&mut out, table);
                codec::write_u32(&mut out, positions.len() as u32);
                for p in positions {
                    codec::write_u32(&mut out, *p as u32);
                }
                // Epic W: one RowId per position (parallel). Capture
                // sites always produce `rowids.len() == positions.len()`;
                // this assertion pins that invariant at encode time so a
                // mismatch is a loud bug, not a silently short payload.
                debug_assert_eq!(
                    rowids.len(),
                    positions.len(),
                    "redo Delete: rowids must be parallel to positions"
                );
                for rid in rowids {
                    codec::write_u64(&mut out, rid.0);
                }
                codec::write_u64(&mut out, *writer_version);
            }
            RowChange::Tombstone {
                table,
                rowids,
                xmax,
            } => {
                out.push(3);
                codec::write_str(&mut out, table);
                codec::write_u32(&mut out, rowids.len() as u32);
                for rid in rowids {
                    codec::write_u64(&mut out, rid.0);
                }
                codec::write_u64(&mut out, *xmax);
            }
        }
    }
    out
}

/// v7.34, extended v7.37.15 (Epic W slice 1) — decode a row-level redo
/// log written by [`encode_redo_log`].
///
/// Decodes **both** the Epic W metadata-carrying layout (first byte
/// `REDO_META_MARKER = 0xFF`) and the pre-Epic-W layout (first byte is
/// `FILE_VERSION`, always `< 0xFF`). For the old layout the per-change
/// metadata is absent, so `rowid`/`rowids` come back
/// [`RowId::UNASSIGNED`](row_header::RowId::UNASSIGNED) (empty for
/// `Delete`) and `writer_version` comes back `0`.
///
/// A truncated / corrupt buffer is a hard error — never a panic — the
/// embedding layer frames each record with its own length + CRC, so a
/// frame that decodes short is corruption, not a torn tail.
pub fn decode_redo_log(bytes: &[u8]) -> Result<Vec<RowChange>, StorageError> {
    let first = *bytes
        .first()
        .ok_or_else(|| StorageError::Corrupt("redo log: empty".into()))?;
    // Epic W: `0xFF` marker ⇒ metadata-carrying layout; anything else
    // is a pre-Epic-W `FILE_VERSION` byte (old layout, no metadata).
    let has_meta = first == REDO_META_MARKER;
    let (codec_version, header_len) = if has_meta {
        let meta_version = *bytes
            .get(1)
            .ok_or_else(|| StorageError::Corrupt("redo log: short header".into()))?;
        if meta_version != REDO_META_VERSION {
            return Err(StorageError::Corrupt(alloc::format!(
                "redo log: unknown metadata version {meta_version}"
            )));
        }
        let file_version = *bytes
            .get(2)
            .ok_or_else(|| StorageError::Corrupt("redo log: short header".into()))?;
        // header = [marker][meta_version][file_version]
        (file_version, 3usize)
    } else {
        // Old layout: the first byte IS the FILE_VERSION.
        (first, 1usize)
    };
    let mut cur = codec::Cursor::new(bytes).with_codec_version(codec_version);
    for _ in 0..header_len {
        cur.read_u8()?;
    }
    let count = cur.read_u32()? as usize;
    let mut read_values =
        |cur: &mut codec::Cursor<'_>| -> Result<Vec<Value<'static>>, StorageError> {
            let n = cur.read_u32()? as usize;
            let mut vals = Vec::with_capacity(n);
            for _ in 0..n {
                vals.push(cur.read_value()?);
            }
            Ok(vals)
        };
    let mut changes = Vec::with_capacity(count);
    for _ in 0..count {
        let op = cur.read_u8()?;
        let table = cur.read_str()?;
        let change = match op {
            0 => {
                let row = Row::new(read_values(&mut cur)?);
                let (rowid, writer_version) = if has_meta {
                    (row_header::RowId(cur.read_u64()?), cur.read_u64()?)
                } else {
                    (row_header::RowId::UNASSIGNED, 0)
                };
                RowChange::Insert {
                    table,
                    row,
                    rowid,
                    writer_version,
                }
            }
            1 => {
                let pos = cur.read_u32()? as usize;
                let new_row = read_values(&mut cur)?;
                let (rowid, writer_version) = if has_meta {
                    (row_header::RowId(cur.read_u64()?), cur.read_u64()?)
                } else {
                    (row_header::RowId::UNASSIGNED, 0)
                };
                RowChange::Update {
                    table,
                    pos,
                    new_row,
                    rowid,
                    writer_version,
                }
            }
            2 => {
                let n = cur.read_u32()? as usize;
                let mut positions = Vec::with_capacity(n);
                for _ in 0..n {
                    positions.push(cur.read_u32()? as usize);
                }
                let (rowids, writer_version) = if has_meta {
                    let mut rowids = Vec::with_capacity(n);
                    for _ in 0..n {
                        rowids.push(row_header::RowId(cur.read_u64()?));
                    }
                    (rowids, cur.read_u64()?)
                } else {
                    // Old layout carried no RowId metadata.
                    (Vec::new(), 0)
                };
                RowChange::Delete {
                    table,
                    positions,
                    rowids,
                    writer_version,
                }
            }
            // Op 3 is the Epic W in-place tombstone — it only exists in
            // the metadata-carrying layout. Guarding on `has_meta` means
            // a legacy stream that happens to contain a `3` byte here is
            // reported as an unknown op (corruption), never mis-decoded.
            3 if has_meta => {
                let n = cur.read_u32()? as usize;
                let mut rowids = Vec::with_capacity(n);
                for _ in 0..n {
                    rowids.push(row_header::RowId(cur.read_u64()?));
                }
                let xmax = cur.read_u64()?;
                RowChange::Tombstone {
                    table,
                    rowids,
                    xmax,
                }
            }
            other => {
                return Err(StorageError::Corrupt(alloc::format!(
                    "redo log: unknown op {other}"
                )));
            }
        };
        changes.push(change);
    }
    Ok(changes)
}

/// v7.39 (pg_stat knife B) — per-table scan counters, bumped from
/// `&self` read paths. Clone (tx shadow catalogs clone tables) copies
/// the current values; the counters are volatile like PG's cumulative
/// stats.
#[derive(Debug, Default)]
pub struct ScanStats {
    pub seq_scan: core::sync::atomic::AtomicU64,
    pub seq_tup_read: core::sync::atomic::AtomicU64,
    pub idx_scan: core::sync::atomic::AtomicU64,
    pub idx_tup_fetch: core::sync::atomic::AtomicU64,
}

impl Clone for ScanStats {
    fn clone(&self) -> Self {
        use core::sync::atomic::{AtomicU64, Ordering};
        Self {
            seq_scan: AtomicU64::new(self.seq_scan.load(Ordering::Relaxed)),
            seq_tup_read: AtomicU64::new(self.seq_tup_read.load(Ordering::Relaxed)),
            idx_scan: AtomicU64::new(self.idx_scan.load(Ordering::Relaxed)),
            idx_tup_fetch: AtomicU64::new(self.idx_tup_fetch.load(Ordering::Relaxed)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Table {
    schema: TableSchema,
    /// v7.37.15 (Phase C.1) — stable per-catalog relation identity.
    /// [`RelId::UNASSIGNED`](row_header::RelId::UNASSIGNED) until
    /// `Catalog::create_table` (or the deserialize dense-assign pass)
    /// stamps a real id. Keys the Phase C.4 row-lock table and the
    /// Phase C.5 `RelationStore`; survives `DROP TABLE` slot shifts.
    rel_id: row_header::RelId,
    rows: PersistentVec<Row<'static>>,
    /// v7.37.15 (Phase A.2) — per-row MVCC visibility headers
    /// parallel to `rows`. `headers.len() == rows.len()` is the
    /// load-bearing invariant; debug builds assert it on every
    /// scan boundary, release builds rely on it from
    /// disciplined insert / delete / update paths.
    ///
    /// Pre-v7.37.15-loaded tables (every row currently in the
    /// fleet) start as `RowHeader::frozen()` — `is_all_visible_fast()`
    /// returns `true`, so the per-row visibility gate Phase B
    /// adds is a no-op against any snapshot.
    ///
    /// Headers are NOT yet serialised into the envelope at this
    /// commit — on snapshot deserialize every row gets a fresh
    /// `RowHeader::frozen()`. Phase D adds the visibility-map
    /// + segment-freeze story which makes serialisation
    /// meaningful; until then the on-disk story is "the catalog
    /// is the set of visible rows."
    headers: PersistentVec<row_header::RowHeader>,
    /// v7.37.15 (Phase C.1) — stable per-relation row identity
    /// parallel to `rows` / `headers`. `rowids[i]` is the never-
    /// reused [`RowId`](row_header::RowId) of the row physically at
    /// slot `i`; `rowids.len() == rows.len()` joins the same load-
    /// bearing lock-step invariant as `headers`. Compaction (delete
    /// / vacuum) rebuilds all three vecs together so the id travels
    /// with the row while the slot shifts.
    ///
    /// Introduced additively: allocated + kept lock-step, but index
    /// locators still address rows by physical slot at this commit.
    /// Later phases migrate the lock table (C.4), HOT chains (D),
    /// and the WAL (Epic W) to address by `RowId`.
    ///
    /// Not yet serialised into the envelope — on load every row is
    /// assigned a fresh dense id `1..=len` (see `next_rowid`), which
    /// is sufficient while the id is process-local bookkeeping. The
    /// V6 envelope (Phase C.6) will persist ids so a WAL redo can
    /// name a row across restart.
    rowids: PersistentVec<row_header::RowId>,
    /// v7.37.15 (Phase C.1) — per-relation monotonic allocator for
    /// `rowids`. Starts at 1 (0 is the `RowId::UNASSIGNED` sentinel);
    /// every append takes `next_rowid` then increments. Never reused
    /// even after the row is deleted / vacuumed, so a stale lock /
    /// redo reference can be detected rather than silently aliasing a
    /// later row that reused the slot.
    next_rowid: u64,
    /// v7.37.16 (autovacuum) — live count of tombstoned-but-present hot
    /// rows (`headers[i].xmax != XMAX_ALIVE`). Maintained incrementally:
    /// `mark_row_deleted` / `mark_rows_deleted` increment (the only
    /// tombstone producers), `delete_rows_no_index` recomputes over the
    /// survivors (it is the compaction hub every physical removal —
    /// including vacuum — flows through), and the v53 snapshot loader
    /// recounts verbatim-restored headers. Drives the engine's
    /// autovacuum threshold; not persisted (recomputed on load).
    dead_rows: u64,
    /// v7.39 (pg_stat knife A) — volatile per-table write counters
    /// backing `pg_stat_user_tables.n_tup_ins/upd/del`. Not persisted
    /// (PG's cumulative stats are shared-memory-volatile too — a
    /// restart zeroes them).
    stat_tup_ins: u64,
    stat_tup_upd: u64,
    stat_tup_del: u64,
    /// v7.39 (pg_stat knife B) — volatile scan counters
    /// (`seq_scan/seq_tup_read/idx_scan/idx_tup_fetch`). Atomics: the
    /// read paths that bump them hold only `&Table`.
    scan_stats: ScanStats,
    /// v7.39 (pg_stat knife C) — wall-clock stamps (unix µs, from the
    /// host ClockFn) for pg_stat_user_tables' last_autovacuum /
    /// last_analyze. Volatile, like PG's cumulative stats. SPG has no
    /// manual-VACUUM statement semantics, so last_vacuum stays NULL.
    last_autovacuum_us: Option<i64>,
    last_analyze_us: Option<i64>,
    indices: Vec<Index>,
    hot_bytes: u64,
    /// v6.7.0 — cached count of rows currently materialised in the
    /// cold tier via `RowLocator::Cold` entries across THIS table's
    /// indices. Populated by `ANALYZE` (walks every BTree index and
    /// counts Cold locators); the count survives until the next
    /// ANALYZE recomputes it. Surfaced via `spg_statistic.cold_row_count`
    /// and `spg_stat_segment.table_name`.
    ///
    /// Honest scope: this is a CACHED count, not a live one.
    /// Freezer / promote / DELETE don't currently update the cache
    /// incrementally — they invalidate it by setting the
    /// `cold_row_count_stale` flag, and the next ANALYZE re-walks.
    /// Incremental maintenance is a v6.7.x candidate if observation
    /// shows the ANALYZE walk cost dominates.
    cold_row_count: u64,
    /// v6.7.0 — set when the cached `cold_row_count` may be wrong
    /// because rows moved into / out of the cold tier since the last
    /// ANALYZE. The virtual-table surface reports the cached value
    /// regardless (operators run ANALYZE to refresh).
    cold_row_count_stale: bool,
    /// v7.34 (crash-recovery P0 #2) — row-level redo capture buffer.
    /// `None` (default, in-memory mode) captures nothing — zero overhead.
    /// `Some` (set by the engine when persistence is on, before a
    /// mutating call) makes `insert` / `update_row` / `delete_rows`
    /// record the physical [`RowChange`] they applied, which the engine
    /// drains after the statement and writes to the WAL in place of the
    /// SQL text. Transient: never serialized; a `Catalog::clone` between
    /// enable and drain copies it (cheap — empty in the steady state).
    redo_log: Option<Vec<RowChange>>,
}

/// Catalog: insertion-ordered `Vec<Table>` for stable iter / serialize,
/// plus a `BTreeMap<String, usize>` sidecar index so `get` / `get_mut`
/// run in O(log n) instead of the old linear scan with per-element
/// string compares.
///
/// A pure `BTreeMap<String, Table>` was tried in an interim version
/// of v3.1.2 and regressed the single-table catalog benches by ~10%
/// (the per-element `BTreeMap` overhead outweighs the lookup win
/// when n is small). The sidecar shape preserves the insertion-order
/// iteration the on-disk encoding relies on and keeps `last_mut`
/// (used by the deserialize hot path) cheap.
/// v7.39 (pg_stat blks knife) — catalog-wide cold-tier read counter
/// backing pg_stat_database.blks_read. Row-granular (SPG has no 8 KB
/// page notion): one cold-segment row resolution = one "block read",
/// one hot row access = one "block hit" — the hit RATIO monitoring
/// dashboards compute keeps its meaning. Volatile like PG's stats.
#[derive(Debug, Default)]
pub struct ColdReadStats {
    pub cold_reads: core::sync::atomic::AtomicU64,
}

impl Clone for ColdReadStats {
    fn clone(&self) -> Self {
        Self {
            cold_reads: core::sync::atomic::AtomicU64::new(
                self.cold_reads.load(core::sync::atomic::Ordering::Relaxed),
            ),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// v7.39 (pg_stat blks knife) — see [`ColdReadStats`].
    pub cold_read_stats: ColdReadStats,
    tables: Vec<Table>,
    /// `name → tables[index]`. Kept in lock-step with `tables`.
    /// `create_table` is the only write path.
    by_name: BTreeMap<String, usize>,
    /// v7.37.15 (Phase C.1) — monotonic allocator for stable
    /// [`RelId`](row_header::RelId)s. Pre-incremented on each
    /// `create_table` so real ids start at 1 (0 is `UNASSIGNED`);
    /// never reused even after `DROP TABLE`, so a stale lock / redo
    /// reference is detectable. Process-local bookkeeping — not yet
    /// serialised; `deserialize` re-assigns dense ids on load (the
    /// V6 envelope, Phase C.6, will round-trip real ids).
    next_rel_id: u64,
    /// v5.1: in-memory cold-tier segments. Side-loaded via
    /// [`Catalog::load_segment_bytes`] — they live outside the
    /// catalog snapshot (caller persists them as separate files
    /// and re-loads on boot, until v5.3's `CatalogManifest` makes
    /// that wiring automatic). `RowLocator::Cold { segment_id, .. }`
    /// indexes this `Vec`. Cleared on `Catalog::new` / fresh
    /// `deserialize`.
    ///
    /// `Arc` wrap keeps `Catalog::clone` at O(N segments) bumps
    /// (rather than O(total segment bytes) memcpy) so the v4.42
    /// group-commit pre-image rollback invariant — clone is
    /// effectively free — survives the cold-tier addition.
    ///
    /// v6.7.3 — slots became `Option<…>` so cold-segment compaction
    /// can tombstone merged sources without breaking the
    /// `segment_id = index_into_vec` contract that on-disk
    /// `RowLocator::Cold { segment_id }` already serialized.
    /// `None` slot = the segment was retired by compaction; the
    /// physical file may still be on disk (next CHECKPOINT writes
    /// a manifest that no longer lists it, and the file becomes
    /// an orphan eligible for offline cleanup).
    cold_segments: Vec<Option<Arc<OwnedSegment>>>,
    /// v7.12.4 — user-defined functions (PL/pgSQL + SQL).
    /// Keyed by function name (PG overloading is out of scope).
    /// Bodies are stored as the raw source text the parser saw
    /// between `$$ ... $$`; the engine re-parses on each
    /// invocation. This keeps `spg-storage` free of `spg-sql`
    /// dependency — same pattern as partial-index predicates.
    functions: BTreeMap<String, FunctionDef>,
    /// v7.12.4 — triggers in insertion order. Multiple triggers
    /// per table / event fire in this order (matching PG's
    /// alphabetical-by-default with insertion-stable tie-break
    /// behaviour — we just keep insertion order for now).
    triggers: Vec<TriggerDef>,
    /// v7.17.0 — catalogued SEQUENCE objects (Phase 1.1). Each
    /// `nextval(name)` reaches in here, atomically increments
    /// `last_value` / flips `is_called`, returns the new value.
    /// Persisted in catalog FILE_VERSION 26+; older catalogs
    /// deserialise with an empty map.
    sequences: BTreeMap<String, SequenceDef>,
    /// v7.39 (read01 round 60) — the `public` schema's ACL (PG
    /// `pg_namespace.nspacl`). EMPTY = PG's default, which is not "nothing":
    /// PUBLIC holds USAGE and the owner holds USAGE + CREATE. Materialised on
    /// the first GRANT / REVOKE, exactly like a table's relacl.
    schema_acl: Vec<AclItem>,
    /// v7.39 (read01 round 60) — the database's ACL. EMPTY = PG's default:
    /// PUBLIC holds CONNECT + TEMPORARY, the owner holds all three.
    database_acl: Vec<AclItem>,
    /// v7.17.0 — catalogued VIEW objects (Phase 1.2). Each
    /// `SELECT FROM v` at engine exec-time looks up `v` here and
    /// prepends the view body as a synthetic CTE. Persisted in
    /// catalog FILE_VERSION 27+; older catalogs deserialise with
    /// an empty map.
    views: BTreeMap<String, ViewDef>,
    /// v7.17.0 — catalogued MATERIALIZED VIEW source registry
    /// (Phase 1.3). Maps name → SELECT source. The materialised
    /// rows themselves live as a regular `Table` with the same
    /// name; REFRESH re-parses + re-executes the source against
    /// the table. Persisted in catalog FILE_VERSION 28+;
    /// older catalogs deserialise with an empty map.
    materialized_views: BTreeMap<String, String>,
    /// v7.17.0 — catalogued user-defined ENUM types (Phase 1.4).
    /// Maps name → label list. Columns reference these by name
    /// via `ColumnSchema.user_enum_type`. Persisted in catalog
    /// FILE_VERSION 29+; older catalogs deserialise with an empty
    /// map.
    enum_types: BTreeMap<String, EnumDef>,
    /// v7.17.0 — catalogued user-defined DOMAIN types (Phase 1.5).
    /// Maps name → base + CHECK constraints. Columns reference
    /// these by name via `ColumnSchema.user_domain_type`.
    /// Persisted in catalog FILE_VERSION 30+; older catalogs
    /// deserialise with an empty map.
    domain_types: BTreeMap<String, DomainDef>,
    /// v7.39 (read01 round 50) — `COMMENT ON <kind> <obj> IS '…'` store.
    /// Keyed by a canonical `"<kind>:<name>"` string (`"table:t"`,
    /// `"column:t.c"`, `"index:i"`, `"view:v"`, …) so a new commentable
    /// object kind needs no schema change. `COMMENT … IS NULL` removes the
    /// entry. Persisted in catalog FILE_VERSION 61+; older catalogs
    /// deserialise with an empty map. Read back by obj_description /
    /// col_description and the pg_description view.
    comments: BTreeMap<String, String>,
    /// v7.37.42-T2 ζ-B — catalogued user-defined COMPOSITE types
    /// (`CREATE TYPE name AS (field_name field_type, …)`). Columns
    /// reference these by name via
    /// `ColumnSchema.user_composite_type` (parallel to
    /// `user_enum_type` / `user_domain_type`). Persisted in catalog
    /// FILE_VERSION 52+; older catalogs deserialise with an empty
    /// map.
    composite_types: BTreeMap<String, CompositeDef>,
    /// v7.17.0 — schema-namespace registry (Phase 1.6). Tracks
    /// which schemas exist. `public`, `pg_catalog`, and
    /// `information_schema` are built-in and always present.
    /// Schema-qualified table references still strip the prefix
    /// at lookup time per v7.16-and-earlier — full
    /// schema-as-isolation is v7.18+ scope. Persisted in catalog
    /// FILE_VERSION 31+; older catalogs deserialise with just
    /// the built-ins.
    schemas: alloc::collections::BTreeSet<String>,
}

/// v7.12.4 — catalogued user-defined function. `body` is the raw
/// source text between `$$ ... $$`; the engine re-parses it on
/// invocation. This keeps the storage codec stable when the
/// PL/pgSQL surface grows (no breaking-change risk on the disk
/// format).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionDef {
    pub name: String,
    /// Display form of the argument list, e.g.
    /// `"(name TEXT, ts TIMESTAMP)"`. Empty `"()"` for the trigger
    /// function shape. Parser-side canonicalised before storage.
    pub args_repr: String,
    /// Display form of the return type, e.g. `"TRIGGER"` /
    /// `"INT"` / `"SETOF text"`. The engine special-cases
    /// `"TRIGGER"` (case-insensitive) to gate trigger-only
    /// semantics (NEW/OLD).
    pub returns: String,
    /// `LANGUAGE` clause, lowercased. `"plpgsql"` / `"sql"`.
    pub language: String,
    /// Source body of the function. PL/pgSQL: includes the
    /// surrounding `BEGIN ... END;`. SQL: includes the
    /// statement(s). The engine re-parses on invocation; bad
    /// bodies surface as a parse error at CALL time, not CREATE.
    pub body: String,
    /// v7.39 (read01 round 61) — the role that ran CREATE FUNCTION.
    pub owner: Option<String>,
    /// v7.39 (read01 round 61) — explicit GRANTs (PG `pg_proc.proacl`). EMPTY
    /// is NOT "nobody may call it": PG grants EXECUTE to PUBLIC by default, and
    /// leaves proacl NULL to say so. The list materialises on the first
    /// GRANT / REVOKE.
    pub acl: Vec<AclItem>,
}

/// v7.39 (read01 round 62) — the canonical key for one function overload:
/// `name(type, type)`, lower-cased, with PG's type aliases folded together so
/// `f(integer)` and `f(int)` are the SAME function (which they are).
#[must_use]
pub fn function_signature_key(name: &str, args_repr: &str) -> String {
    let types = function_arg_types(args_repr);
    format!("{}({})", name.to_ascii_lowercase(), types.join(","))
}

/// The declared argument TYPES of a function, out of its `args_repr`
/// (`"(x INT, y DOUBLE PRECISION)"` → `["int", "float"]`). An entry may be a
/// bare type with no name (`"(INT)"`).
#[must_use]
pub fn function_arg_types(args_repr: &str) -> Vec<String> {
    let inner = args_repr.trim().trim_start_matches('(').trim_end_matches(')');
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|part| {
            let mut words: Vec<&str> = part.split_whitespace().collect();
            // `OUT x INT` / `INOUT x INT` — the mode is not part of the type.
            if !words.is_empty()
                && (words[0].eq_ignore_ascii_case("OUT") || words[0].eq_ignore_ascii_case("INOUT"))
            {
                words.remove(0);
            }
            // Two or more words = `name TYPE …`; one word = a bare TYPE.
            let ty = if words.len() >= 2 {
                words[1..].join(" ")
            } else {
                words.first().map_or(String::new(), |w| (*w).to_string())
            };
            normalize_type_name(&ty)
        })
        .collect()
}

/// v7.39 (read01 round 65) — the declared argument NAMES of a function (`""` for
/// a bare type with no name).
#[must_use]
pub fn function_arg_names(args_repr: &str) -> Vec<String> {
    let inner = args_repr.trim().trim_start_matches('(').trim_end_matches(')');
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|part| {
            let mut words: Vec<&str> = part.split_whitespace().collect();
            if !words.is_empty()
                && (words[0].eq_ignore_ascii_case("OUT") || words[0].eq_ignore_ascii_case("INOUT"))
            {
                words.remove(0);
            }
            if words.len() >= 2 {
                words[0].to_string()
            } else {
                String::new()
            }
        })
        .collect()
}

/// Fold PG's type aliases so a signature key is stable across spellings.
/// Unknown names pass through lower-cased — consistency is what the key needs.
#[must_use]
pub fn normalize_type_name(ty: &str) -> String {
    let t = ty.trim().to_ascii_lowercase();
    // Peel a precision/length modifier: `numeric(10,2)`, `varchar(64)`.
    let base = t.split_once('(').map_or(t.as_str(), |(h, _)| h).trim();
    match base {
        "int" | "int4" | "integer" => "int",
        "bigint" | "int8" => "bigint",
        "smallint" | "int2" => "smallint",
        "text" | "varchar" | "character varying" | "char" | "character" | "bpchar" => "text",
        "bool" | "boolean" => "bool",
        "float" | "float8" | "double precision" => "float",
        "real" | "float4" => "real",
        "numeric" | "decimal" => "numeric",
        "timestamptz" | "timestamp with time zone" => "timestamptz",
        "timestamp" | "timestamp without time zone" => "timestamp",
        other => other,
    }
    .to_string()
}

/// v7.12.4 — catalogued trigger. References its function by
/// name; the function must exist at TRIGGER creation time
/// (forward references are deferred to v7.12.5+).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerDef {
    pub name: String,
    /// Watched table. Trigger is dropped when the table drops.
    pub table: String,
    /// `"BEFORE"` / `"AFTER"` / `"INSTEAD OF"`. Stored as the
    /// uppercased keyword so deserialised catalogs round-trip
    /// without canonicalisation surprises.
    pub timing: String,
    /// Each entry is one of `"INSERT"` / `"UPDATE"` / `"DELETE"`
    /// / `"TRUNCATE"`. `INSERT OR UPDATE` parses to two entries.
    pub events: Vec<String>,
    /// `"ROW"` / `"STATEMENT"`. v7.12.4 ships `"ROW"` only;
    /// `"STATEMENT"` parses and persists but the executor
    /// refuses it at trigger fire time.
    pub for_each: String,
    /// Name of the PL/pgSQL function to invoke.
    pub function: String,
    /// v7.13.0 — `UPDATE OF col, col, …` column-list filter
    /// (mailrs round-5 G7). Non-empty means the trigger fires
    /// only when at least one of these columns appears in the
    /// UPDATE's SET list. Empty = no column filter. Stored in
    /// catalog FILE_VERSION 23+; older catalogs deserialise with
    /// an empty vec.
    pub update_columns: Vec<String>,
    /// v7.16.1 — whether the trigger fires when its watched
    /// event occurs. Toggled by `ALTER TABLE … { ENABLE |
    /// DISABLE } TRIGGER …`; pg_dump --disable-triggers wraps
    /// every data block with a DISABLE/ENABLE pair so the
    /// rows already-computed in prod don't get re-rewritten.
    /// Defaults to `true` at CREATE TRIGGER time. Stored in
    /// catalog FILE_VERSION 25+; older catalogs deserialise
    /// with `enabled = true`.
    pub enabled: bool,
}

/// v7.17.0 — catalogued SEQUENCE. PG semantics: a counter object
/// returning monotonically increasing values via `nextval(name)`.
/// `last_value` is the most recent value handed out; `is_called`
/// is false until the first `nextval`/`setval`. Stored separately
/// from tables in the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceDef {
    pub name: String,
    /// Data type — narrows the i64 range. PG default BIGINT.
    pub data_type: SequenceDataType,
    pub start: i64,
    pub increment: i64,
    pub min_value: i64,
    pub max_value: i64,
    pub cache: i64,
    pub cycle: bool,
    /// `OWNED BY` target — `(table, column)` or NONE.
    pub owned_by: Option<(String, String)>,
    /// Most recently handed-out value. Meaningless when
    /// `is_called == false`; in that case the NEXT `nextval`
    /// will return `start`.
    pub last_value: i64,
    pub is_called: bool,
    /// v7.39 (read01 round 60) — the role that ran CREATE SEQUENCE. `None` = an
    /// image written before FILE_VERSION 66, which predates sequence owners.
    pub owner: Option<String>,
    /// v7.39 (read01 round 60) — explicit GRANTs on this sequence. A sequence's
    /// meaningful privileges are SELECT (`currval`), UPDATE (`setval`) and
    /// USAGE (`nextval`).
    pub acl: Vec<AclItem>,
}

/// v7.17.0 — sequence integer width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceDataType {
    SmallInt,
    Int,
    BigInt,
}

/// v7.17.0 Phase 1.6 — built-in schema names that every Catalog
/// understands without an explicit CREATE SCHEMA. Used by
/// [`Catalog::schema_exists`] and the engine's schema-qualified
/// lookup path.
#[must_use]
pub fn is_builtin_schema(name: &str) -> bool {
    name.eq_ignore_ascii_case("public")
        || name.eq_ignore_ascii_case("pg_catalog")
        || name.eq_ignore_ascii_case("information_schema")
}

/// v7.17.0 — parse a PG-canonical UUID text representation into the
/// 16-byte network-order layout used by `Value::Uuid`. Accepted input
/// shapes (all case-insensitive):
///   * Canonical hyphenated 8-4-4-4-12 (`550e8400-e29b-41d4-a716-446655440000`)
///   * Unhyphenated 32-char hex (`550e8400e29b41d4a716446655440000`)
///   * Either form wrapped in `{ ... }`
///
/// Returns `None` for any malformed input (wrong length, non-hex
/// characters, misplaced hyphens). The caller surfaces a SQL error
/// at coercion time — silent acceptance of garbage would mask
/// application bugs and is exactly the divergence from PG that
/// breaks the 0-change cutover promise.
#[must_use]
pub fn parse_uuid_str(input: &str) -> Option<[u8; 16]> {
    let s = input.trim();
    // Strip surrounding braces if present.
    let s = if let Some(inner) = s.strip_prefix('{').and_then(|x| x.strip_suffix('}')) {
        inner
    } else {
        s
    };
    // Two valid shapes after braces are stripped: 32 hex chars or
    // the canonical 36-char hyphenated form.
    let hex: String = match s.len() {
        32 => s.to_ascii_lowercase(),
        36 => {
            // Hyphens must be exactly at positions 8, 13, 18, 23.
            let b = s.as_bytes();
            if b[8] != b'-' || b[13] != b'-' || b[18] != b'-' || b[23] != b'-' {
                return None;
            }
            let mut out = String::with_capacity(32);
            out.push_str(&s[0..8]);
            out.push_str(&s[9..13]);
            out.push_str(&s[14..18]);
            out.push_str(&s[19..23]);
            out.push_str(&s[24..36]);
            out.make_ascii_lowercase();
            out
        }
        _ => return None,
    };
    let bytes = hex.as_bytes();
    let mut out = [0u8; 16];
    for i in 0..16 {
        let hi = hex_nibble(bytes[i * 2])?;
        let lo = hex_nibble(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + b - b'a'),
        b'A'..=b'F' => Some(10 + b - b'A'),
        _ => None,
    }
}

/// v7.17.0 — render a `Value::Uuid` payload as the canonical
/// lowercase 8-4-4-4-12 hyphenated form PG `text` cast surfaces.
#[must_use]
pub fn format_uuid(b: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// v7.17.0 Phase 1.5 — catalogued user-defined DOMAIN. A domain
/// is a named CHECK-constrained alias over a built-in type;
/// columns bound to it inherit the base type plus the CHECK
/// predicates + NOT NULL + DEFAULT at INSERT/UPDATE time.
/// v7.37.17 (Phase E RC rebase) — the write-set one writer version left
/// on a table, addressed by stable [`row_header::RowId`]s so it can be
/// replayed onto a fresher clone of the relation whose physical slots
/// differ. Produced by [`Table::extract_tx_writeset`], consumed by
/// [`Table::replay_tx_writeset`].
#[derive(Debug, Clone, Default)]
pub struct TxWriteSet {
    /// INSERTs and UPDATE-new-versions (`header.xmin == v`).
    pub inserted: Vec<(row_header::RowId, Row<'static>)>,
    /// DELETE / UPDATE-old-version targets (`header.xmax == v`).
    pub tombstoned: Vec<row_header::RowId>,
}

impl TxWriteSet {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inserted.is_empty() && self.tombstoned.is_empty()
    }
}

/// `default` / `checks` are stored as Display-form source so
/// `spg-storage` stays free of `spg-sql` dependency — same
/// pattern as FunctionDef / ViewDef.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainDef {
    pub name: String,
    pub base_type: DataType,
    pub nullable: bool,
    pub default: Option<String>,
    pub checks: Vec<String>,
}

/// v7.17.0 Phase 1.4 — catalogued user-defined ENUM type. The
/// label vector is order-preserving (PG enum ordering follows the
/// declared order). At INSERT/UPDATE on a column bound to this
/// enum, the engine looks up the value against `labels` and
/// rejects non-members.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumDef {
    pub name: String,
    pub labels: Vec<String>,
}

/// v7.37.42-T2 ζ-B — catalogued user-defined COMPOSITE type
/// (`CREATE TYPE name AS (field_name field_type, ...)`). Order
/// matters: PG composite literals are positional, and SPG mirrors
/// that. Stored as ordered `(name, DataType)` pairs to keep the
/// codec straightforward and to allow eventual `Value::Composite`
/// bodies to encode positionally. Persisted in catalog FILE_VERSION
/// 52+; older catalogs deserialise with an empty composite_types
/// map. Composite types can be used as a column type by spelling
/// the composite's name; the resolution from
/// `ColumnSchema.user_composite_type = Some(name)` happens at the
/// engine boundary (parallel to `user_enum_type` /
/// `user_domain_type`). The dense storage shape — JSON-text body
/// keyed by the composite's field list — keeps the codec free of
/// recursive `Value` bodies until the full Value::Composite arena
/// migration in a later phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositeDef {
    pub name: String,
    /// Ordered `(field_name, field_type)` pairs. PG composite
    /// literals are positional, so order is part of the type's
    /// identity.
    pub fields: Vec<(String, DataType)>,
}

/// v7.17.0 Phase 1.2 — catalogued VIEW. The body is stored as the
/// raw source text the parser saw between `AS` and the statement
/// terminator; the engine re-parses on each invocation. Same
/// pattern as `FunctionDef` — keeps `spg-storage` free of
/// `spg-sql` dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewDef {
    pub name: String,
    /// Optional `(col, col, …)` rename list. Empty when the body's
    /// projected names are used directly.
    pub columns: Vec<String>,
    /// Raw SELECT source. Display-rendered at storage time so the
    /// catalog round-trips a deterministic form regardless of
    /// whitespace / comments in the original input. Re-parsed at
    /// SELECT-from-view time to materialise as a synthetic CTE.
    pub body: String,
}

impl SequenceDataType {
    /// PG default min/max per AS clause.
    pub fn default_bounds(self, increment_positive: bool) -> (i64, i64) {
        match self {
            Self::SmallInt => {
                if increment_positive {
                    (1, i64::from(i16::MAX))
                } else {
                    (i64::from(i16::MIN), -1)
                }
            }
            Self::Int => {
                if increment_positive {
                    (1, i64::from(i32::MAX))
                } else {
                    (i64::from(i32::MIN), -1)
                }
            }
            Self::BigInt => {
                if increment_positive {
                    (1, i64::MAX)
                } else {
                    (i64::MIN, -1)
                }
            }
        }
    }
}

impl Catalog {
    /// v7.37.15 (Phase D) — fleet-wide vacuum pass. Walks every
    /// user table and reclaims rows whose delete-commit version is
    /// older than `oldest_active_snapshot`. Returns an aggregated
    /// report with per-table breakdown so hosts can emit metrics.
    ///
    /// `dry_run = true` reports the work without doing it. Use it
    /// to estimate the cost before scheduling a real pass.
    pub fn vacuum_all(
        &mut self,
        oldest_active_snapshot: u64,
        dry_run: bool,
    ) -> vacuum::VacuumReport {
        let mut total = vacuum::VacuumReport::default();
        // Snapshot the table names so we don't hold an immutable
        // borrow during the get_mut loop.
        let names: Vec<String> = self
            .tables
            .iter()
            .map(|t| t.schema().name.clone())
            .collect();
        for name in names {
            let Some(t) = self.get_mut(&name) else {
                continue;
            };
            let r = t.vacuum(oldest_active_snapshot, dry_run);
            if r.rows_reclaimed > 0 {
                total.per_table.push((name, r.rows_reclaimed));
            }
            total.rows_reclaimed += r.rows_reclaimed;
            total.rows_examined += r.rows_examined;
        }
        total
    }

    pub const fn new() -> Self {
        Self {
            cold_read_stats: ColdReadStats {
                cold_reads: core::sync::atomic::AtomicU64::new(0),
            },
            tables: Vec::new(),
            by_name: BTreeMap::new(),
            next_rel_id: 0,
            cold_segments: Vec::new(),
            functions: BTreeMap::new(),
            triggers: Vec::new(),
            sequences: BTreeMap::new(),
            schema_acl: Vec::new(),
            database_acl: Vec::new(),
            views: BTreeMap::new(),
            materialized_views: BTreeMap::new(),
            enum_types: BTreeMap::new(),
            domain_types: BTreeMap::new(),
            comments: BTreeMap::new(),
            composite_types: BTreeMap::new(),
            schemas: alloc::collections::BTreeSet::new(),
        }
    }

    /// v7.12.4 — read-only view of catalogued user-defined
    /// functions. Engine callers go through here to look up the
    /// function body before re-parsing it for invocation.
    pub const fn functions(&self) -> &BTreeMap<String, FunctionDef> {
        &self.functions
    }

    /// v7.12.4 — register a new user-defined function. With
    /// `or_replace = false`, errors if the name is taken. The
    /// engine validates the body before passing it here.
    pub fn create_function(
        &mut self,
        def: FunctionDef,
        or_replace: bool,
    ) -> Result<(), StorageError> {
        // v7.39 (read01 round 62) — functions are keyed by SIGNATURE, not by
        // name: `f(int)` and `f(text)` are two functions, as in PG. Keying by
        // name alone made a second overload an "already exists" error — so a
        // pg_dump carrying an overload set could not restore — and, worse, a
        // call to one overload silently ran the other.
        let key = function_signature_key(&def.name, &def.args_repr);
        if !or_replace && self.functions.contains_key(&key) {
            return Err(StorageError::Corrupt(format!(
                "function {:?} already exists (drop or use CREATE OR REPLACE)",
                def.name
            )));
        }
        self.functions.insert(key, def);
        Ok(())
    }

    /// v7.39 (read01 round 62) — every overload of `name`.
    #[must_use]
    pub fn functions_named(&self, name: &str) -> Vec<&FunctionDef> {
        self.functions
            .values()
            .filter(|f| f.name.eq_ignore_ascii_case(name))
            .collect()
    }

    /// v7.39 (read01 round 62) — one overload, by its signature key.
    #[must_use]
    pub fn function_by_key(&self, key: &str) -> Option<&FunctionDef> {
        self.functions.get(key)
    }

    /// v7.39 (read01 round 62) — drop ONE overload. `true` if it was there.
    pub fn drop_function_by_key(&mut self, key: &str) -> bool {
        self.functions.remove(key).is_some()
    }

    /// v7.12.4 — remove a user-defined function by name. Returns
    /// `true` if a function was removed, `false` if none matched.
    /// Caller decides whether to surface `if_exists` semantics.
    /// v7.39 (read01 round 62) — with no signature, PG drops the function only
    /// when the name is unambiguous. SPG mirrors that: this removes EVERY
    /// overload of `name`, and the caller (ddl.rs) refuses the ambiguous case
    /// before getting here.
    pub fn drop_function(&mut self, name: &str) -> bool {
        let keys: Vec<String> = self
            .functions
            .iter()
            .filter(|(_, f)| f.name.eq_ignore_ascii_case(name))
            .map(|(k, _)| k.clone())
            .collect();
        let hit = !keys.is_empty();
        for k in keys {
            self.functions.remove(&k);
        }
        hit
    }

    /// v7.17.0 — read-only handle to catalogued sequences.
    /// v7.39 (read01 round 60) — the `public` schema's ACL (PG nspacl).
    #[must_use]
    pub fn schema_acl(&self) -> &[AclItem] {
        &self.schema_acl
    }

    pub fn schema_acl_mut(&mut self) -> &mut Vec<AclItem> {
        &mut self.schema_acl
    }

    /// v7.39 (read01 round 60) — the database's ACL.
    #[must_use]
    pub fn database_acl(&self) -> &[AclItem] {
        &self.database_acl
    }

    pub fn database_acl_mut(&mut self) -> &mut Vec<AclItem> {
        &mut self.database_acl
    }

    /// v7.39 (read01 round 60) — mutable sequence access, for GRANT.
    pub fn sequence_mut(&mut self, name: &str) -> Option<&mut SequenceDef> {
        self.sequences.get_mut(name)
    }

    /// v7.39 (read01 round 61) — mutable function access, for GRANT.
    pub fn function_mut(&mut self, name: &str) -> Option<&mut FunctionDef> {
        self.functions.get_mut(name)
    }

    pub const fn sequences(&self) -> &BTreeMap<String, SequenceDef> {
        &self.sequences
    }

    /// v7.17.0 — register a new SEQUENCE. Errors if `name`
    /// collides with an existing sequence and `if_not_exists`
    /// is false.
    pub fn create_sequence(
        &mut self,
        def: SequenceDef,
        if_not_exists: bool,
    ) -> Result<(), StorageError> {
        if self.sequences.contains_key(&def.name) {
            if if_not_exists {
                return Ok(());
            }
            // v7.39 (read01 round 47) — a sequence is a relation to PG (42P07).
            return Err(StorageError::Corrupt(format!(
                "relation {:?} already exists",
                def.name
            )));
        }
        self.sequences.insert(def.name.clone(), def);
        Ok(())
    }

    /// v7.17.0 — remove a SEQUENCE by name. Returns `true` if a
    /// sequence was removed, `false` if none matched. Caller
    /// surfaces IF EXISTS semantics.
    /// v7.39 (read01 round 49) — `ALTER SEQUENCE old RENAME TO new`.
    /// Errors when `old` is missing or `new` is taken; the SequenceDef's own
    /// `name` field is rewritten so it stays self-describing.
    pub fn rename_sequence(&mut self, old: &str, new: &str) -> Result<(), StorageError> {
        if !self.sequences.contains_key(old) {
            return Err(StorageError::Corrupt(format!(
                "relation {old:?} does not exist"
            )));
        }
        if self.sequences.contains_key(new) {
            return Err(StorageError::Corrupt(format!(
                "relation {new:?} already exists"
            )));
        }
        if let Some(mut def) = self.sequences.remove(old) {
            def.name = new.to_string();
            self.sequences.insert(new.to_string(), def);
        }
        Ok(())
    }

    pub fn drop_sequence(&mut self, name: &str) -> bool {
        self.sequences.remove(name).is_some()
    }

    /// v7.17.0 — atomic nextval. Increments `last_value` per
    /// `increment`, returns the new value, sets `is_called`.
    /// Returns an error on CYCLE-less overflow.
    pub fn sequence_next_value(&mut self, name: &str) -> Result<i64, StorageError> {
        let Some(seq) = self.sequences.get_mut(name) else {
            return Err(StorageError::Corrupt(format!(
                "sequence {name:?} does not exist"
            )));
        };
        // PG semantics: when !is_called (fresh sequence or
        // setval(_, false)), the next nextval returns the stored
        // `last_value`. When is_called, it advances by `increment`
        // and CYCLE-wraps on overflow.
        let candidate = if seq.is_called {
            let next = seq.last_value.checked_add(seq.increment).ok_or_else(|| {
                StorageError::Corrupt(format!("sequence {name:?} arithmetic overflow"))
            })?;
            if seq.increment > 0 {
                if next > seq.max_value {
                    if seq.cycle {
                        seq.min_value
                    } else {
                        return Err(StorageError::Corrupt(format!(
                            "sequence {name:?} reached MAXVALUE ({})",
                            seq.max_value
                        )));
                    }
                } else {
                    next
                }
            } else if next < seq.min_value {
                if seq.cycle {
                    seq.max_value
                } else {
                    return Err(StorageError::Corrupt(format!(
                        "sequence {name:?} reached MINVALUE ({})",
                        seq.min_value
                    )));
                }
            } else {
                next
            }
        } else {
            seq.last_value
        };
        seq.last_value = candidate;
        seq.is_called = true;
        Ok(candidate)
    }

    /// v7.17.0 — currval. Errors if the session has never called
    /// nextval on this sequence (PG semantics). At the catalog
    /// level we approximate "session" with "is_called persisted";
    /// the engine session-tracking layer can wrap this for the
    /// strict per-session semantics later.
    pub fn sequence_current_value(&self, name: &str) -> Result<i64, StorageError> {
        let Some(seq) = self.sequences.get(name) else {
            return Err(StorageError::Corrupt(format!(
                "sequence {name:?} does not exist"
            )));
        };
        if !seq.is_called {
            return Err(StorageError::Corrupt(format!(
                "currval of sequence {name:?} is not yet defined in this session"
            )));
        }
        Ok(seq.last_value)
    }

    /// v7.17.0 — setval(name, value [, is_called]). PG returns
    /// `value` regardless. `is_called=true` means the NEXT
    /// nextval will return `value + increment`; `is_called=false`
    /// means the next nextval will return `value`.
    pub fn sequence_set_value(
        &mut self,
        name: &str,
        value: i64,
        is_called: bool,
    ) -> Result<i64, StorageError> {
        let Some(seq) = self.sequences.get_mut(name) else {
            return Err(StorageError::Corrupt(format!(
                "sequence {name:?} does not exist"
            )));
        };
        seq.last_value = value;
        seq.is_called = is_called;
        Ok(value)
    }

    /// v7.17.0 Phase 1.2 — read-only handle to catalogued views.
    pub const fn views(&self) -> &BTreeMap<String, ViewDef> {
        &self.views
    }

    /// v7.17.0 Phase 1.2 — install a VIEW. `or_replace=true`
    /// overwrites an existing entry; `if_not_exists=true` is a
    /// silent no-op when the name is taken. Errors if both flags
    /// are off and the name collides.
    pub fn create_view(
        &mut self,
        def: ViewDef,
        or_replace: bool,
        if_not_exists: bool,
    ) -> Result<(), StorageError> {
        if self.views.contains_key(&def.name) {
            if or_replace {
                self.views.insert(def.name.clone(), def);
                return Ok(());
            }
            if if_not_exists {
                return Ok(());
            }
            // v7.39 (read01 round 47) — a view is a relation to PG (42P07).
            return Err(StorageError::Corrupt(format!(
                "relation {:?} already exists",
                def.name
            )));
        }
        // Reject name collision with tables / sequences — same
        // namespace per PG.
        if self.by_name.contains_key(&def.name) {
            return Err(StorageError::Corrupt(format!(
                "view {:?} would shadow an existing table",
                def.name
            )));
        }
        if self.sequences.contains_key(&def.name) {
            return Err(StorageError::Corrupt(format!(
                "view {:?} would shadow an existing sequence",
                def.name
            )));
        }
        self.views.insert(def.name.clone(), def);
        Ok(())
    }

    /// v7.17.0 Phase 1.2 — remove a view by name. Returns true if
    /// a view was removed.
    pub fn drop_view(&mut self, name: &str) -> bool {
        self.views.remove(name).is_some()
    }

    /// v7.17.0 Phase 1.3 — read-only handle to the materialised-
    /// view source registry. Each entry pairs with a regular
    /// table of the same name that holds the cached rows.
    pub const fn materialized_views(&self) -> &BTreeMap<String, String> {
        &self.materialized_views
    }

    /// v7.17.0 Phase 1.3 — register a source for a materialised
    /// view. Caller has already created the backing table.
    pub fn register_materialized_view(&mut self, name: String, body: String) {
        self.materialized_views.insert(name, body);
    }

    /// v7.17.0 Phase 1.3 — drop the source registry entry. Returns
    /// true if a source was unregistered. Caller separately drops
    /// the backing table.
    pub fn drop_materialized_view_source(&mut self, name: &str) -> bool {
        self.materialized_views.remove(name).is_some()
    }

    /// v7.17.0 Phase 1.4 — read-only handle to user-defined ENUM
    /// catalog.
    pub const fn enum_types(&self) -> &BTreeMap<String, EnumDef> {
        &self.enum_types
    }

    /// v7.17.0 Phase 1.4 — install a new ENUM type. Errors if
    /// `name` collides with an existing enum (no IF NOT EXISTS
    /// per PG semantics for CREATE TYPE).
    pub fn create_enum_type(&mut self, def: EnumDef) -> Result<(), StorageError> {
        if self.enum_types.contains_key(&def.name) {
            return Err(StorageError::Corrupt(format!(
                "type {:?} already exists",
                def.name
            )));
        }
        self.enum_types.insert(def.name.clone(), def);
        Ok(())
    }

    /// v7.17.0 Phase 1.4 — drop an ENUM type by name. Returns
    /// true if a type was removed.
    /// v7.37 D.55 — `ALTER TYPE … ADD VALUE`. Appends `label` to an existing
    /// enum's ordered label list, or inserts it before/after an existing label.
    /// `if_not_exists` makes a duplicate a no-op; otherwise a duplicate errors.
    /// Returns `Ok(true)` if a label was added, `Ok(false)` if it already existed
    /// (only possible under `if_not_exists`).
    /// v7.39 (read01 round 49) — `ALTER TYPE t RENAME VALUE 'old' TO 'new'`.
    /// The parser used to swallow this form as a no-op, so the rename was
    /// accepted and silently ignored. Renaming in place keeps the label's
    /// sort position, which is what PG does (enumsortorder is untouched).
    pub fn rename_enum_value(
        &mut self,
        type_name: &str,
        old: &str,
        new: &str,
    ) -> Result<(), StorageError> {
        let def = self
            .enum_types
            .get_mut(type_name)
            .ok_or_else(|| StorageError::Corrupt(format!("type {type_name:?} does not exist")))?;
        if def.labels.iter().any(|l| l == new) {
            return Err(StorageError::Corrupt(format!(
                "enum label {new:?} already exists"
            )));
        }
        let at = def.labels.iter().position(|l| l == old).ok_or_else(|| {
            StorageError::Corrupt(format!(
                "{old:?} is not an existing enum label"
            ))
        })?;
        def.labels[at] = new.to_string();
        Ok(())
    }

    /// v7.39 (read01 round 50) — set (or, with `None`, remove) the comment on
    /// an object. `key` is the canonical `"<kind>:<name>"` form.
    pub fn set_comment(&mut self, key: &str, text: Option<&str>) {
        match text {
            Some(t) => {
                self.comments.insert(key.to_string(), t.to_string());
            }
            None => {
                self.comments.remove(key);
            }
        }
    }

    /// v7.39 (read01 round 50) — the comment on an object, if any.
    #[must_use]
    pub fn comment(&self, key: &str) -> Option<&str> {
        self.comments.get(key).map(String::as_str)
    }

    /// v7.39 (read01 round 50) — every `(key, text)` pair, for the
    /// pg_description view.
    #[must_use]
    pub const fn comments(&self) -> &BTreeMap<String, String> {
        &self.comments
    }

    /// v7.39 (read01 round 50) — drop every comment whose key names `obj`
    /// (the object itself and, for a table, its columns). Called when the
    /// object is dropped so a later object of the same name doesn't inherit
    /// a stale comment.
    pub fn drop_comments_for(&mut self, kind: &str, name: &str) {
        let exact = alloc::format!("{kind}:{name}");
        let col_prefix = alloc::format!("column:{name}.");
        self.comments
            .retain(|k, _| *k != exact && !k.starts_with(&col_prefix));
    }

    pub fn add_enum_value(
        &mut self,
        type_name: &str,
        label: &str,
        if_not_exists: bool,
        position: Option<(bool, String)>,
    ) -> Result<bool, StorageError> {
        let def = self
            .enum_types
            .get_mut(type_name)
            .ok_or_else(|| StorageError::Corrupt(format!("type {type_name:?} does not exist")))?;
        if def.labels.iter().any(|l| l == label) {
            if if_not_exists {
                return Ok(false);
            }
            // v7.39 (read01 round 49) — PG wording (42710 at the wire).
            return Err(StorageError::Corrupt(format!(
                "enum label {label:?} already exists"
            )));
        }
        match position {
            None => def.labels.push(label.to_string()),
            Some((is_before, anchor)) => {
                let at = def
                    .labels
                    .iter()
                    .position(|l| l == &anchor)
                    .ok_or_else(|| {
                        StorageError::Corrupt(format!(
                            "enum label {anchor:?} does not exist in type {type_name:?}"
                        ))
                    })?;
                let idx = if is_before { at } else { at + 1 };
                def.labels.insert(idx, label.to_string());
            }
        }
        Ok(true)
    }

    pub fn drop_enum_type(&mut self, name: &str) -> bool {
        self.enum_types.remove(name).is_some()
    }

    /// v7.17.0 Phase 1.5 — read-only handle to DOMAIN catalog.
    pub const fn domain_types(&self) -> &BTreeMap<String, DomainDef> {
        &self.domain_types
    }

    /// v7.17.0 Phase 1.5 — install a DOMAIN. Errors on collision
    /// with an existing domain.
    pub fn create_domain_type(&mut self, def: DomainDef) -> Result<(), StorageError> {
        if self.domain_types.contains_key(&def.name) {
            return Err(StorageError::Corrupt(format!(
                "domain {:?} already exists",
                def.name
            )));
        }
        self.domain_types.insert(def.name.clone(), def);
        Ok(())
    }

    /// v7.17.0 Phase 1.5 — drop a DOMAIN by name.
    pub fn drop_domain_type(&mut self, name: &str) -> bool {
        self.domain_types.remove(name).is_some()
    }

    /// v7.37.42-T2 ζ-B — read-only handle to user-defined COMPOSITE
    /// catalog. Used by the engine to resolve
    /// `ColumnSchema.user_composite_type` lookups + by
    /// information_schema-style introspection.
    pub const fn composite_types(&self) -> &BTreeMap<String, CompositeDef> {
        &self.composite_types
    }

    /// v7.37.42-T2 ζ-B — install a new COMPOSITE type. Errors if
    /// `name` already exists in the composite registry (PG forbids
    /// IF NOT EXISTS on CREATE TYPE composite; the engine surfaces
    /// the collision with the existing name).
    pub fn create_composite_type(&mut self, def: CompositeDef) -> Result<(), StorageError> {
        if self.composite_types.contains_key(&def.name) {
            return Err(StorageError::Corrupt(format!(
                "type {:?} already exists",
                def.name
            )));
        }
        self.composite_types.insert(def.name.clone(), def);
        Ok(())
    }

    /// v7.37.42-T2 ζ-B — drop a COMPOSITE type by name. Returns
    /// true if a type was removed.
    pub fn drop_composite_type(&mut self, name: &str) -> bool {
        self.composite_types.remove(name).is_some()
    }

    /// v7.17.0 Phase 1.6 — read-only handle to the user-created
    /// schema registry. Built-in schemas (`public`, `pg_catalog`,
    /// `information_schema`) are NOT included here; use
    /// [`schema_exists`](Self::schema_exists) for the full
    /// check.
    pub const fn user_schemas(&self) -> &alloc::collections::BTreeSet<String> {
        &self.schemas
    }

    /// v7.17.0 Phase 1.6 — schema-name resolver. Returns true
    /// for built-in schemas + every user-CREATEd one. Used by
    /// CREATE SCHEMA collision checks and (future) by
    /// information_schema.schemata.
    pub fn schema_exists(&self, name: &str) -> bool {
        is_builtin_schema(name) || self.schemas.contains(name)
    }

    /// v7.17.0 Phase 1.6 — register a new schema. Errors if the
    /// name already exists and `if_not_exists=false`. Built-in
    /// names cannot be redeclared.
    pub fn create_schema(&mut self, name: String, if_not_exists: bool) -> Result<(), StorageError> {
        if is_builtin_schema(&name) {
            if if_not_exists {
                return Ok(());
            }
            return Err(StorageError::Corrupt(format!(
                "schema {name:?} is built-in and cannot be redeclared"
            )));
        }
        if self.schemas.contains(&name) {
            if if_not_exists {
                return Ok(());
            }
            return Err(StorageError::Corrupt(format!(
                "schema {name:?} already exists"
            )));
        }
        self.schemas.insert(name);
        Ok(())
    }

    /// v7.17.0 Phase 1.6 — drop a user-created schema. Returns
    /// true if a schema was removed. Built-in names always
    /// return false (cannot be dropped). Tables that previously
    /// used the schema as a prefix keep their bare name and stay
    /// queryable — this is the "prefix routing, not isolation"
    /// posture documented in v7.17 Phase 1.6.
    pub fn drop_schema(&mut self, name: &str) -> Result<bool, StorageError> {
        if is_builtin_schema(name) {
            return Err(StorageError::Corrupt(format!(
                "schema {name:?} is built-in and cannot be dropped"
            )));
        }
        Ok(self.schemas.remove(name))
    }

    /// v7.17.0 — ALTER SEQUENCE option merge. Caller-provided
    /// updates overwrite the matching fields; unset fields keep
    /// their stored values. RESTART variants update last_value
    /// directly per PG: `RESTART` resets to current `start`;
    /// `RESTART WITH n` resets to `n`.
    #[allow(clippy::too_many_arguments)]
    pub fn alter_sequence(
        &mut self,
        name: &str,
        increment: Option<i64>,
        min_value: Option<i64>,
        max_value: Option<i64>,
        start: Option<i64>,
        restart: Option<Option<i64>>,
        cache: Option<i64>,
        cycle: Option<bool>,
        owned_by: Option<Option<(String, String)>>,
    ) -> Result<(), StorageError> {
        let Some(seq) = self.sequences.get_mut(name) else {
            return Err(StorageError::Corrupt(format!(
                "sequence {name:?} does not exist"
            )));
        };
        if let Some(v) = increment {
            seq.increment = v;
        }
        if let Some(v) = min_value {
            seq.min_value = v;
        }
        if let Some(v) = max_value {
            seq.max_value = v;
        }
        if let Some(v) = start {
            seq.start = v;
        }
        if let Some(restart_value) = restart {
            seq.last_value = restart_value.unwrap_or(seq.start);
            seq.is_called = false;
        }
        if let Some(v) = cache {
            seq.cache = v;
        }
        if let Some(v) = cycle {
            seq.cycle = v;
        }
        if let Some(v) = owned_by {
            seq.owned_by = v;
        }
        Ok(())
    }

    /// v7.12.4 — read-only slice of all catalogued triggers.
    /// Engine row-write paths filter this by (table, event,
    /// timing) and fire matches in slice order.
    pub fn triggers(&self) -> &[TriggerDef] {
        &self.triggers
    }

    /// v7.15.0 — mutable handle to the trigger slice for
    /// `ALTER TABLE … RENAME COLUMN`, which rewrites every
    /// `update_columns` entry that referenced the renamed
    /// column.
    pub fn triggers_mut(&mut self) -> &mut Vec<TriggerDef> {
        &mut self.triggers
    }

    /// v7.12.4 — register a new trigger. With `or_replace = false`,
    /// errors when a trigger with the same name already exists on
    /// the same table (PG scoping rule — trigger names are
    /// per-table, not global). Trigger function must already
    /// exist in the catalog at registration time.
    pub fn create_trigger(
        &mut self,
        def: TriggerDef,
        or_replace: bool,
    ) -> Result<(), StorageError> {
        if !self.by_name.contains_key(&def.table) {
            return Err(StorageError::TableNotFound {
                name: def.table.clone(),
            });
        }
        // v7.39 (read01 round 62) — functions are keyed by SIGNATURE now. A
        // trigger names its function by NAME (a trigger function takes no
        // arguments), so the existence check goes through the name index.
        if self.functions_named(&def.function).is_empty() {
            return Err(StorageError::Corrupt(format!(
                "trigger {:?} references unknown function {:?}",
                def.name, def.function
            )));
        }
        let dup = self
            .triggers
            .iter()
            .position(|t| t.name == def.name && t.table == def.table);
        match (dup, or_replace) {
            (Some(_), false) => Err(StorageError::Corrupt(format!(
                "trigger {:?} already exists on table {:?}",
                def.name, def.table
            ))),
            (Some(i), true) => {
                self.triggers[i] = def;
                Ok(())
            }
            (None, _) => {
                self.triggers.push(def);
                Ok(())
            }
        }
    }

    /// v7.12.4 — remove a trigger by `(name, table)`. Returns
    /// `true` if one was removed.
    pub fn drop_trigger(&mut self, name: &str, table: &str) -> bool {
        let before = self.triggers.len();
        self.triggers
            .retain(|t| !(t.name == name && t.table == table));
        before != self.triggers.len()
    }

    pub fn create_table(&mut self, schema: TableSchema) -> Result<(), StorageError> {
        if self.by_name.contains_key(&schema.name) {
            return Err(StorageError::DuplicateTable {
                name: schema.name.clone(),
            });
        }
        let idx = self.tables.len();
        let name = schema.name.clone();
        self.tables.push(Table::new(schema));
        self.by_name.insert(name, idx);
        // v7.37.15 (Phase C.1) — stamp the new relation with a stable,
        // monotonic, never-reused RelId. Pre-increment so ids start at
        // 1 (0 = UNASSIGNED); a later DROP TABLE frees the slot but not
        // the id.
        self.next_rel_id += 1;
        let rid = row_header::RelId(self.next_rel_id);
        self.tables[idx].set_rel_id(rid);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Table> {
        let idx = *self.by_name.get(name)?;
        self.tables.get(idx)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Table> {
        let idx = *self.by_name.get(name)?;
        self.tables.get_mut(idx)
    }

    /// v7.37.42 (docker-fair SCALARSQ attack) — resolve a table name to
    /// its insertion-order index ONCE, so callers that need to fetch the
    /// same table many times (per-row PK probes in correlated scalar
    /// subqueries) can avoid the per-call `BTreeMap<String, usize>` string
    /// descent. The returned index is stable for the lifetime of the
    /// catalog snapshot the caller holds (same engine read guard).
    pub fn tables_position_of(&self, name: &str) -> Option<usize> {
        self.by_name.get(name).copied()
    }

    /// Direct positional fetch counterpart to [`tables_position_of`].
    /// `idx` must come from `tables_position_of` against the same catalog
    /// snapshot — out-of-range returns `None`.
    pub fn tables_at(&self, idx: usize) -> Option<&Table> {
        self.tables.get(idx)
    }

    /// v7.34 (crash-recovery P0 #2) — replay a row-level redo log onto
    /// this catalog (the [`RowChange`] physical-redo apply primitive that
    /// row-level WAL recovery will use in place of statement re-execution).
    /// Applies each change in order via the same `Table` mutators the
    /// engine used — no uniqueness/FK/parse/plan: the original execution
    /// already validated, replay trusts and applies. Positions are
    /// physical and only valid when replayed from the matching checkpoint
    /// baseline in original order (see [`RowChange`] docs).
    ///
    /// A change naming an absent table, or whose position is out of range,
    /// is a corrupt/misaligned log and surfaces as an error rather than a
    /// silent skip.
    pub fn apply_redo(&mut self, changes: &[RowChange]) -> Result<(), StorageError> {
        // v7.37.5 (mailrs crash-recovery Ask 3) — true batched replay.
        // Pre-v7.37.5 each `RowChange::Delete` record ran a fresh
        // O(N) PersistentVec rebuild + O(N × indices × log N)
        // `rebuild_indices()` — 5000 records × 100k rows × 13 indices
        // ≈ 27 min on the mailrs prod-shape WAL.
        //
        // The strategy: group consecutive changes by table, and for
        // each run, compose all the row-level mutations through a
        // single "live" tracking vector + a per-table operation log,
        // then apply rows + indices ONCE at the end. The result:
        //  - DELETE blow-up: O(records × rows × indices × log rows)
        //    → O(rows × indices × log rows) — one rebuild per run.
        //  - Row-position semantics preserved: positions in a later
        //    `Delete` / `Update` record reference the layout produced
        //    by every earlier change; we walk the live-vector
        //    forward as each change is processed so positions
        //    translate correctly to the ORIGINAL row index space.
        //
        // For correctness, even with this batching `apply_redo`
        // remains in-order: a single per-table run only batches
        // a contiguous slice of changes targeting that table; a
        // mid-run change targeting a DIFFERENT table forces a
        // flush of the current run.
        let mut runs: alloc::vec::Vec<(String, alloc::vec::Vec<&RowChange>)> =
            alloc::vec::Vec::new();
        for change in changes {
            // v7.39 (flip crash-replay P0) — a replayed tombstone carries
            // the xmax the CRASHED process allocated, but this process's
            // version cursor restarted; without advancing it past every
            // replayed version, `Snapshot::visible`'s "deletion is in the
            // future" branch (xmax > snapshot.version) resurrects every
            // replayed delete. Same recovery contract as the snapshot
            // loader (`observe_persisted_version`, the pg_control-style
            // nextXid recovery).
            if let RowChange::Tombstone { xmax, .. } = change {
                row_header::observe_persisted_version(*xmax);
            }
            let table = match change {
                RowChange::Insert { table, .. }
                | RowChange::Update { table, .. }
                | RowChange::Delete { table, .. }
                | RowChange::Tombstone { table, .. } => table.clone(),
            };
            if runs.last().map(|(t, _)| t.as_str()) != Some(table.as_str()) {
                runs.push((table, alloc::vec::Vec::new()));
            }
            runs.last_mut().unwrap().1.push(change);
        }
        for (table_name, run) in runs {
            self.apply_redo_run_on_table(&table_name, &run)?;
        }
        Ok(())
    }

    /// v7.37.5 — apply a contiguous slice of `RowChange`s all
    /// targeting the same `table_name`. Composes row mutations
    /// through a single live-tracking vector + a single tail
    /// for appended `Insert`s + a single in-place edit set for
    /// `Update`s, then writes the final row layout to
    /// `self.rows` and rebuilds indices ONCE.
    fn apply_redo_run_on_table(
        &mut self,
        table_name: &str,
        run: &[&RowChange],
    ) -> Result<(), StorageError> {
        // Look up the table once; the unchecked unwrap is safe
        // because the caller just resolved `table_name` for each
        // change.
        let table = self.get_mut(table_name).ok_or_else(|| {
            StorageError::Corrupt(alloc::format!("redo: unknown table {table_name:?}"))
        })?;
        // Live-tracking over both pre-existing rows and tail-
        // appended Insert rows. `live[i] = true` initially for
        // every existing row. Appended Inserts extend with `true`.
        // A `Delete` flips entries to `false` (using the position
        // mapping that walks live indices in order). An `Update`
        // edits in place — collected into an overlay map keyed by
        // ORIGINAL row position so later Updates win.
        let original_rows: alloc::vec::Vec<Row<'static>> = table.rows().iter().cloned().collect();
        let mut live: alloc::vec::Vec<bool> = alloc::vec![true; original_rows.len()];
        let mut tail: alloc::vec::Vec<Row<'static>> = alloc::vec::Vec::new();
        // Overlay: index into ORIGINAL row space (existing rows
        // 0..original_rows.len()) or into tail (offset
        // original_rows.len()). Map -> new values.
        let mut overlay: alloc::collections::BTreeMap<usize, alloc::vec::Vec<Value<'static>>> =
            alloc::collections::BTreeMap::new();
        // v7.37.15 (Epic W durable-tombstone slice) — extra bookkeeping
        // ONLY when this run actually carries an in-place `Tombstone`.
        // A tombstone keeps its row physically present but stamps `xmax`
        // on the header; the run finalizer `set_rows_and_rebuild_indices`
        // freezes every header (and reassigns ids), so we must re-stamp
        // in a post-pass keyed by RowId. When the run has no tombstone
        // (every default gate-off replay) this is all skipped and the
        // path below stays byte-for-byte the legacy one.
        let has_tomb = run.iter().any(|c| matches!(c, RowChange::Tombstone { .. }));
        // Ids of the pre-existing rows, snapshotted parallel to
        // `original_rows`, and ids of the tail rows filled from each
        // `Insert`'s carried `rowid`. Together they let a tombstone name
        // the exact row the writer stamped, independent of the ids the
        // finalizer will hand out. (When `!has_tomb`, both stay empty.)
        // v7.39 (flip crash-replay P0) — ids are tracked UNCONDITIONALLY
        // now: the finalizer preserves them so a later WAL record's
        // tombstone can still name rows this record produced.
        let orig_rowids: alloc::vec::Vec<row_header::RowId> =
            table.rowids().iter().copied().collect();
        // Headers snapshotted in lock-step: the finalizer preserves
        // them so earlier records' tombstone stamps survive.
        let orig_headers: alloc::vec::Vec<row_header::RowHeader> =
            table.headers().iter().copied().collect();
        let mut tail_rowids: alloc::vec::Vec<row_header::RowId> = alloc::vec::Vec::new();
        // (RowId, xmax) of every row this run tombstones.
        let mut tomb_targets: alloc::vec::Vec<(row_header::RowId, u64)> = alloc::vec::Vec::new();
        // Helper: given a "current" position (i.e. position in
        // the post-prior-deletes layout), translate to the
        // ABSOLUTE position in the unified live + tail space
        // by walking the live vector + tail. Returns None when
        // the position is out of range.
        fn translate(live: &[bool], tail_len: usize, current_pos: usize) -> Option<usize> {
            // Walk live[..] counting live entries until we hit
            // current_pos. Then if not yet matched, dip into tail.
            let mut seen = 0usize;
            for (i, &alive) in live.iter().enumerate() {
                if alive {
                    if seen == current_pos {
                        return Some(i);
                    }
                    seen += 1;
                }
            }
            // Position lives in tail. tail_len rows in the tail
            // are all live (we haven't deleted any tail rows in
            // this simplification; if we did, we'd extend `live`).
            let off = current_pos - seen;
            if off < tail_len {
                Some(live.len() + off)
            } else {
                None
            }
        }
        for change in run {
            match *change {
                RowChange::Insert { row, rowid, .. } => {
                    // Validate against schema before recording the
                    // change so a corrupt log surfaces as an error
                    // rather than silently mis-applying.
                    if row.len() != table.schema().columns.len() {
                        return Err(StorageError::ArityMismatch {
                            expected: table.schema().columns.len(),
                            actual: row.len(),
                        });
                    }
                    tail.push(row.clone());
                    // Keep the id lock-step with `tail` so a later
                    // tombstone (this run or a later WAL record) can
                    // find the row by the id the writer captured.
                    tail_rowids.push(*rowid);
                }
                RowChange::Update { pos, new_row, .. } => {
                    if new_row.len() != table.schema().columns.len() {
                        return Err(StorageError::ArityMismatch {
                            expected: table.schema().columns.len(),
                            actual: new_row.len(),
                        });
                    }
                    let abs = translate(&live, tail.len(), *pos).ok_or_else(|| {
                        StorageError::Corrupt(alloc::format!(
                            "redo: update_row position {pos} out of bounds in table {table_name:?}",
                        ))
                    })?;
                    // Tail edits are applied directly to `tail`
                    // (we own it); existing-row edits land in
                    // the overlay map keyed by original index.
                    if abs < live.len() {
                        overlay.insert(abs, new_row.clone());
                    } else {
                        tail[abs - live.len()] = Row::new(new_row.clone());
                    }
                }
                RowChange::Delete { positions, .. } => {
                    // De-dup + sort so the translate walk stays
                    // monotone (the second translate doesn't have
                    // to redo work the first one did, in principle;
                    // we keep it simple here and re-walk per
                    // position). Bounds-filter silently mirrors
                    // `Table::delete_rows`.
                    let mut sorted: alloc::vec::Vec<usize> = positions.clone();
                    sorted.sort_unstable();
                    sorted.dedup();
                    // Walk live[] once per Delete record to
                    // translate all positions in this record's
                    // post-prior-deletes layout to absolute
                    // indices. We MUST defer the live[] flip
                    // until after all positions are translated
                    // so two positions in the same record
                    // (e.g. [3, 7]) reference the same layout.
                    let mut to_flip_live: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
                    let mut to_flip_tail: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
                    // Two-pointer walk: live[i] scanned monotonically,
                    // sorted positions consumed in order.
                    let mut seen = 0usize;
                    let mut sp = sorted.iter().peekable();
                    for (i, &alive) in live.iter().enumerate() {
                        if !alive {
                            continue;
                        }
                        while let Some(&&p) = sp.peek() {
                            if seen == p {
                                to_flip_live.push(i);
                                sp.next();
                            } else {
                                break;
                            }
                        }
                        if sp.peek().is_none() {
                            break;
                        }
                        seen += 1;
                    }
                    // Remaining positions fall into the tail.
                    for &p in sp {
                        // p >= seen and refers to the (p - seen)-th
                        // entry in tail. Filter out-of-bounds.
                        let off = p - seen;
                        if off < tail.len() {
                            to_flip_tail.push(off);
                        }
                    }
                    for i in to_flip_live {
                        live[i] = false;
                        // Any pending overlay edit for this
                        // index is moot — the row is gone.
                        overlay.remove(&i);
                    }
                    // Tail deletes: remove in REVERSE order so
                    // shifting indices stay valid.
                    to_flip_tail.sort_unstable();
                    to_flip_tail.dedup();
                    for off in to_flip_tail.into_iter().rev() {
                        tail.remove(off);
                        {
                            // Keep the id vector lock-step with `tail`.
                            tail_rowids.remove(off);
                        }
                        // Re-key tail-relative overlay entries that
                        // were past `off` — in practice tail edits
                        // are applied directly so the overlay map
                        // only holds existing-row keys; nothing to
                        // do here.
                    }
                }
                RowChange::Tombstone { rowids, xmax, .. } => {
                    // An in-place tombstone leaves the row physically
                    // present — it does not touch `live` / `tail` /
                    // `overlay`. Record the (id, xmax) targets; the
                    // post-finalizer pass re-stamps `xmax` onto the
                    // matching row's (otherwise-frozen) header.
                    for rid in rowids {
                        tomb_targets.push((*rid, *xmax));
                    }
                }
            }
        }
        // Compose the final row layout: keep existing rows where
        // live[i] = true, applying overlay edits in place; then
        // append the surviving tail.
        let mut new_rows: PersistentVec<Row> = PersistentVec::new();
        let mut new_hot_bytes: u64 = 0;
        let schema_snapshot = table.schema().clone();
        // Parallel to `new_rows` (only built when `has_tomb`): the RowId
        // of each row in its FINAL slot, so the post-pass can map a
        // tombstone target id → the slot to re-stamp `xmax` on.
        let mut final_rowids: alloc::vec::Vec<row_header::RowId> = alloc::vec::Vec::new();
        let mut final_headers: alloc::vec::Vec<row_header::RowHeader> = alloc::vec::Vec::new();
        for (i, row) in original_rows.into_iter().enumerate() {
            if !live[i] {
                continue;
            }
            let final_row = if let Some(new_values) = overlay.remove(&i) {
                Row::new(new_values)
            } else {
                row
            };
            new_hot_bytes = new_hot_bytes
                .saturating_add(row_body_encoded_len(&final_row, &schema_snapshot) as u64);
            new_rows.push_mut(final_row);
            final_rowids.push(
                orig_rowids
                    .get(i)
                    .copied()
                    .unwrap_or(row_header::RowId::UNASSIGNED),
            );
            final_headers.push(
                orig_headers
                    .get(i)
                    .copied()
                    .unwrap_or_else(row_header::RowHeader::frozen),
            );
        }
        for (off, row) in tail.into_iter().enumerate() {
            new_hot_bytes =
                new_hot_bytes.saturating_add(row_body_encoded_len(&row, &schema_snapshot) as u64);
            new_rows.push_mut(row);
            final_rowids.push(
                tail_rowids
                    .get(off)
                    .copied()
                    .unwrap_or(row_header::RowId::UNASSIGNED),
            );
            final_headers.push(row_header::RowHeader::frozen());
        }
        // v7.39 (flip crash-replay P0) — id-preserving finalizer, so a
        // LATER WAL record's tombstone still resolves rows this record
        // produced (per-statement replay used to reassign ids between
        // records, orphaning every cross-record tombstone target).
        table.set_rows_and_rebuild_indices_with_rowids(
            new_rows,
            new_hot_bytes,
            &final_rowids,
            &final_headers,
        );
        // v7.37.15 (Epic W durable-tombstone slice) — header-preserving
        // re-stamp. `set_rows_and_rebuild_indices` above froze every
        // header, so any row this run tombstoned is currently all-
        // visible again. Re-apply the `xmax` stamp by matching the
        // tombstone's target RowId against the final-slot id map. This
        // is what makes a gate-on DELETE durable across replay without
        // changing the on-disk snapshot format (headers/ids are still
        // NOT serialised — that is the deferred V6 coupling; see below).
        if has_tomb && !tomb_targets.is_empty() {
            let mut id_to_slot: alloc::collections::BTreeMap<row_header::RowId, usize> =
                alloc::collections::BTreeMap::new();
            for (slot, rid) in final_rowids.iter().enumerate() {
                if *rid != row_header::RowId::UNASSIGNED {
                    id_to_slot.insert(*rid, slot);
                }
            }
            let table = self.get_mut(table_name).ok_or_else(|| {
                StorageError::Corrupt(alloc::format!("redo: unknown table {table_name:?}"))
            })?;
            for (rid, xmax) in &tomb_targets {
                match id_to_slot.get(rid) {
                    Some(&slot) => {
                        // First-deleter-wins + bounds handled inside.
                        let _ = table.mark_row_deleted(slot, *xmax);
                    }
                    None => {
                        // The target row was not produced by THIS redo
                        // run and its id was not in the run-start
                        // snapshot — the documented cross-checkpoint
                        // limitation: after a checkpoint restore the
                        // table's ids are reassigned (not yet persisted
                        // in the envelope), so a tombstone naming a
                        // pre-checkpoint row cannot be resolved by id.
                        // Skipping leaves the row visible (identical to
                        // the pre-Epic-W non-durable behaviour); it is
                        // never a correctness regression, only an
                        // unclosed durability gap the V6 envelope slice
                        // closes. Counted for observability.
                        UNRESOLVED_TOMBSTONES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        }
        Ok(())
    }

    fn table_for_redo(&mut self, name: &str) -> Result<&mut Table, StorageError> {
        self.get_mut(name)
            .ok_or_else(|| StorageError::Corrupt(alloc::format!("redo: unknown table {name:?}")))
    }

    /// v7.34 (crash-recovery P0 #2) — enable row-level redo capture on
    /// every table (the engine calls this before a mutating statement
    /// when persistence is on; idempotent, keeps any in-flight capture).
    pub fn enable_redo_all(&mut self) {
        for t in &mut self.tables {
            t.enable_redo();
        }
    }

    /// v7.34 — drain the row-level redo captured across all tables, in
    /// table order then per-table apply order, and stop capturing. The
    /// engine calls this after a successful mutating statement and writes
    /// the returned [`RowChange`]s to the WAL in place of the SQL text.
    pub fn drain_redo(&mut self) -> Vec<RowChange> {
        let mut all = Vec::new();
        for t in &mut self.tables {
            all.extend(t.take_redo());
        }
        all
    }

    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    /// v7.14.0 — remove a table by name. Returns `true` when the
    /// table existed (and is now gone), `false` when it didn't.
    /// Used by `DROP TABLE` from pg_dump / mysqldump preambles
    /// where the dump re-creates schema and starts with
    /// `DROP TABLE IF EXISTS`.
    pub fn drop_table(&mut self, name: &str) -> bool {
        let Some(idx) = self.by_name.remove(name) else {
            return false;
        };
        // swap_remove invalidates the trailing index → rebuild
        // by_name for affected entries.
        self.tables.swap_remove(idx);
        // Re-stamp moved table's index slot in by_name.
        if idx < self.tables.len() {
            let moved_name = self.tables[idx].schema.name.clone();
            self.by_name.insert(moved_name, idx);
        }
        true
    }

    /// v7.16.2 — rename a table (mailrs round-10 A.5). Updates
    /// the schema name, the catalog name → index map, and
    /// rewrites every reference dangling at the table name:
    ///   * every FK on every OTHER table whose `parent_table`
    ///     pointed at the old name now points at the new
    ///     name, so FK enforcement keeps working
    ///   * every trigger watching the table updates its `table`
    ///     field
    /// Returns `Ok` on success; `Err(StorageError::TableNotFound)`
    /// when the old name isn't in the catalog and
    /// `Err(StorageError::DuplicateTable)` when the new name is
    /// already taken.
    pub fn rename_table(&mut self, old: &str, new: &str) -> Result<(), StorageError> {
        if old == new {
            return Ok(());
        }
        if self.by_name.contains_key(new) {
            return Err(StorageError::Corrupt(format!(
                "rename_table: target name {new:?} already exists"
            )));
        }
        let idx = self
            .by_name
            .remove(old)
            .ok_or_else(|| StorageError::TableNotFound { name: old.into() })?;
        self.tables[idx].schema.name = new.to_string();
        self.by_name.insert(new.to_string(), idx);
        for t in &mut self.tables {
            for fk in &mut t.schema.foreign_keys {
                if fk.parent_table == old {
                    fk.parent_table = new.to_string();
                }
            }
        }
        for trig in &mut self.triggers {
            if trig.table == old {
                trig.table = new.to_string();
            }
        }
        Ok(())
    }

    /// v7.16.2 — rename an index by name. Walks every table
    /// since the index lives on its owning table; updates the
    /// name in place. Errors with `IndexNotFound` when no
    /// index matches. mailrs round-10 A.5.
    pub fn rename_index(&mut self, old: &str, new: &str) -> Result<(), StorageError> {
        if old == new {
            return Ok(());
        }
        // Reject the new name if it already exists anywhere.
        for t in &self.tables {
            if t.indices.iter().any(|i| i.name == new) {
                return Err(StorageError::Corrupt(format!(
                    "rename_index: target name {new:?} already exists"
                )));
            }
        }
        for t in &mut self.tables {
            for i in &mut t.indices {
                if i.name == old {
                    i.name = new.to_string();
                    return Ok(());
                }
            }
        }
        Err(StorageError::IndexNotFound { name: old.into() })
    }

    /// v7.14.0 — remove a named index across the catalog.
    /// Returns `true` when found + dropped.
    pub fn drop_named_index(&mut self, name: &str) -> bool {
        for t in &mut self.tables {
            let before = t.indices.len();
            t.indices.retain(|i| i.name != name);
            if t.indices.len() != before {
                return true;
            }
        }
        false
    }

    /// Borrow-free copy of every table's name in catalog order
    /// (= insertion order, matching the on-disk encoding).
    pub fn table_names(&self) -> Vec<String> {
        self.tables.iter().map(|t| t.schema.name.clone()).collect()
    }

    /// v5.1: register a cold-tier segment that already lives in
    /// memory (caller did the file read). Returns the
    /// `segment_id` that `RowLocator::Cold { segment_id, .. }`
    /// will reference — currently this is just the index into
    /// `cold_segments`, but treat it as an opaque token.
    ///
    /// Storage is `no_std`, so file I/O is the caller's
    /// responsibility — `spg-server` reads the file and forwards
    /// the bytes here. The bytes stay resident in the catalog
    /// for the life of the `Catalog`, parsed only once.
    pub fn load_segment_bytes(&mut self, bytes: Vec<u8>) -> Result<u32, StorageError> {
        let id = u32::try_from(self.cold_segments.len()).map_err(|_| {
            StorageError::Corrupt("cold segment count would exceed u32::MAX".into())
        })?;
        let seg = OwnedSegment::from_bytes(bytes)
            .map_err(|e| StorageError::Corrupt(format!("cold segment parse failed: {e}")))?;
        self.cold_segments.push(Some(Arc::new(seg)));
        Ok(id)
    }

    /// v6.7.3 — register a cold-tier segment at a specific id. Used
    /// by the spg-server manifest-boot path so segments whose
    /// neighbouring ids were retired by compaction still get back
    /// the same `segment_id` they had pre-restart (the
    /// `RowLocator::Cold { segment_id }` baked into the BTree-index
    /// snapshot persists across restart and must continue to
    /// resolve).
    ///
    /// Pads the Vec with `None` slots up to `target_id` if needed.
    /// Errors when the target slot is already occupied (would
    /// stomp another segment), the parse fails, or `target_id`
    /// exceeds `u32::MAX`.
    pub fn load_segment_bytes_at(
        &mut self,
        target_id: u32,
        bytes: Vec<u8>,
    ) -> Result<(), StorageError> {
        let seg = OwnedSegment::from_bytes(bytes)
            .map_err(|e| StorageError::Corrupt(format!("cold segment parse failed: {e}")))?;
        let idx = target_id as usize;
        while self.cold_segments.len() <= idx {
            self.cold_segments.push(None);
        }
        if self.cold_segments[idx].is_some() {
            return Err(StorageError::Corrupt(format!(
                "load_segment_bytes_at: segment_id {target_id} already occupied"
            )));
        }
        self.cold_segments[idx] = Some(Arc::new(seg));
        Ok(())
    }

    /// v6.7.3 — retire a cold-tier segment slot (compaction-driven).
    /// The physical file is the caller's concern (typically kept
    /// on disk until the next CHECKPOINT writes a manifest that
    /// no longer lists it); this just flips the in-memory slot
    /// to `None` so later cold lookups for `segment_id` resolve
    /// as "unknown" instead of returning a stale row.
    ///
    /// No-op when the slot is already `None`. Errors only when
    /// `segment_id` is out of bounds.
    pub fn tombstone_segment(&mut self, segment_id: u32) -> Result<(), StorageError> {
        let idx = segment_id as usize;
        if idx >= self.cold_segments.len() {
            return Err(StorageError::Corrupt(format!(
                "tombstone_segment: segment_id {segment_id} out of bounds (len={})",
                self.cold_segments.len()
            )));
        }
        self.cold_segments[idx] = None;
        Ok(())
    }

    /// Number of *active* (non-tombstoned) cold segments.
    #[must_use]
    pub fn cold_segment_count(&self) -> usize {
        self.cold_segments.iter().filter(|s| s.is_some()).count()
    }

    /// v7.37.42 (docker-fair SCALARSQ attack 3) — short-circuit guard
    /// for scan loops that conditionally walk the cold tier. Returns
    /// `false` when the catalog has never loaded a cold segment (or all
    /// segments are tombstoned), so callers can skip the per-table cold
    /// PK-index walk entirely on hot-only databases. O(N segments);
    /// typical N is small (single-digit) so the check is sub-µs.
    #[must_use]
    pub fn has_any_cold_segments(&self) -> bool {
        self.cold_segments.iter().any(Option::is_some)
    }

    /// Slot count including tombstones (= the next id the
    /// no-arg `load_segment_bytes` would allocate).
    #[must_use]
    pub fn cold_segment_slot_count(&self) -> usize {
        self.cold_segments.len()
    }

    /// v6.2.7 — list every *active* cold-tier segment id known to
    /// this catalog (skips compaction tombstones since v6.7.3).
    /// Used by EXPLAIN ANALYZE to annotate scan nodes with the
    /// segments they could have walked.
    #[must_use]
    pub fn cold_segment_ids_global(&self) -> Vec<u32> {
        self.cold_segments
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|_| i as u32))
            .collect()
    }

    /// v5.2.1: sum of `Table::hot_bytes` across every table. The v5.2
    /// freezer compares this against `SPG_HOT_TIER_BYTES` (parsed at
    /// server startup; default 4 GiB) and wakes when the budget is
    /// crossed. Pre-freezer (v5.2.1) this is measurement-only — the
    /// counter exposes whether the budget is being approached without
    /// triggering any demotion.
    #[must_use]
    pub fn hot_tier_bytes(&self) -> u64 {
        self.tables
            .iter()
            .map(Table::hot_bytes)
            .fold(0u64, u64::saturating_add)
    }

    /// v5.2.2: freeze the **first** `max_rows` rows of `table_name`'s
    /// hot tier into a brand-new cold-tier segment. The named `BTree`
    /// index supplies the per-row PK (its column must be an integer
    /// type — v5.2.2 only supports `IndexKey::Int` PKs, matching the
    /// `index_key_as_u64` constraint used by the cold-tier lookup
    /// path). On success returns a [`FreezeReport`] with the
    /// freshly-allocated segment id, the count of rows that moved,
    /// the encoded segment bytes (so the caller can persist them to
    /// disk for later reload via `SPG_PRELOAD_COLD_SEGMENT`), and the
    /// hot-tier byte delta that was reclaimed.
    ///
    /// **Semantics**:
    /// 1. The first `max_rows` rows (by hot-tier position — same as
    ///    insertion order under v4.39 `PersistentVec`) are read.
    /// 2. Rows are sorted ascending by PK and serialised into a new
    ///    segment via [`encode_segment`].
    /// 3. The hot rows are dropped via [`Table::delete_rows`]; the
    ///    `rebuild_indices` it triggers regenerates `Hot` locators
    ///    for every remaining row (their positions shift down by
    ///    `max_rows`). Existing `Cold` locators in this index — from
    ///    a previous freeze — are also rebuilt **but with empty
    ///    payload** since rebuild reads only `self.rows`; this
    ///    routine re-registers them at the end of the call so the
    ///    user-visible state preserves all prior cold locators.
    /// 4. The new segment is loaded into `self.cold_segments` via
    ///    [`Catalog::load_segment_bytes`] (allocating a fresh
    ///    `segment_id`). New `Cold` locators are registered on the
    ///    named index — one per frozen row.
    ///
    /// **v5.2.2 limits** (relaxed in later sub-versions):
    /// - INSERT-only flow: subsequent UPDATE/DELETE on a frozen row
    ///   returns a stale-locator error (no promote-on-write until
    ///   v5.2.3).
    /// - Single-table scope: callers iterate tables themselves.
    /// - All-or-nothing: returns `Err` and leaves catalog unchanged
    ///   if any step fails before the atomic swap point.
    ///
    /// Errors:
    /// - [`StorageError::Corrupt`] for missing table/index, non-`BTree`
    ///   index, non-integer PK column, `max_rows == 0`, or
    ///   `max_rows > row_count`.
    /// - The encoder's [`SegmentError`] surfaces as `Corrupt` (the
    ///   only realistic source is "a single row is larger than the
    ///   page size"; SPG schemas don't hit it in practice).
    pub fn freeze_oldest_to_cold(
        &mut self,
        table_name: &str,
        index_name: &str,
        max_rows: usize,
    ) -> Result<FreezeReport, StorageError> {
        // --- validation phase: never mutates ---------------------
        if max_rows == 0 {
            return Err(StorageError::Corrupt(
                "freeze_oldest_to_cold: max_rows must be > 0".into(),
            ));
        }
        let table = self.get(table_name).ok_or_else(|| {
            StorageError::Corrupt(format!(
                "freeze_oldest_to_cold: table {table_name:?} not found"
            ))
        })?;
        if max_rows > table.rows.len() {
            return Err(StorageError::Corrupt(format!(
                "freeze_oldest_to_cold: max_rows {max_rows} > row_count {}",
                table.rows.len()
            )));
        }
        let idx = table
            .indices
            .iter()
            .find(|i| i.name == index_name)
            .ok_or_else(|| {
                StorageError::Corrupt(format!(
                    "freeze_oldest_to_cold: index {index_name:?} not found on {table_name:?}"
                ))
            })?;
        if !matches!(idx.kind, IndexKind::BTree(_)) {
            return Err(StorageError::Corrupt(format!(
                "freeze_oldest_to_cold: index {index_name:?} is NSW; only BTree indices may freeze"
            )));
        }
        let column_position = idx.column_position;

        // --- segment build phase: reads only --------------------
        let schema = table.schema.clone();
        let mut to_freeze: Vec<(u64, Vec<u8>, IndexKey)> = Vec::with_capacity(max_rows);
        for row_idx in 0..max_rows {
            let row = table.rows.get(row_idx).expect("bounds-checked above");
            let key = IndexKey::from_value(&row.values[column_position]).ok_or_else(|| {
                StorageError::Corrupt(format!(
                    "freeze_oldest_to_cold: row {row_idx} has NULL / non-key value in index column"
                ))
            })?;
            let pk_u64 = index_key_as_u64(&key).ok_or_else(|| {
                StorageError::Corrupt(format!(
                    "freeze_oldest_to_cold: index {index_name:?} column type is non-integer; \
                     v5.2.2 cold tier requires IndexKey::Int (Text PK lands in v5.5+)"
                ))
            })?;
            to_freeze.push((pk_u64, encode_row_body_dense(row, &schema), key));
        }
        // encode_segment requires ascending u64 keys. Sort by PK
        // before encoding; the caller's row-position order is not
        // necessarily PK order (e.g. workloads that insert random
        // PKs).
        to_freeze.sort_by_key(|(k, _, _)| *k);
        // Reject duplicate PKs — encode_segment also rejects them
        // (`SegmentError::UnsortedKey`), but the resulting error
        // message there is misleading. Surface a clearer one.
        for w in to_freeze.windows(2) {
            if w[0].0 == w[1].0 {
                return Err(StorageError::Corrupt(format!(
                    "freeze_oldest_to_cold: duplicate PK {} in freeze batch",
                    w[0].0
                )));
            }
        }
        // Snapshot the (key, locator) pairs that will be registered
        // post-swap. Cloning the IndexKey out before the move makes
        // the registration loop borrow-free.
        let post_swap_keys: Vec<IndexKey> = to_freeze.iter().map(|(_, _, k)| k.clone()).collect();
        // Segment encode is now infallible w.r.t. ordering. Map the
        // `SegmentError` into a `StorageError::Corrupt` so the
        // public surface stays one error type.
        let seg_rows: Vec<(u64, Vec<u8>)> = to_freeze
            .into_iter()
            .map(|(k, body, _)| (k, body))
            .collect();
        let frozen_rows = seg_rows.len();
        let (seg_bytes, _meta) = encode_segment(seg_rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES)
            .map_err(|e| StorageError::Corrupt(format!("freeze_oldest_to_cold: encode: {e}")))?;

        // --- atomic swap phase: mutations only past this point ---
        // v5.2.3 made `Table::rebuild_indices` preserve every Cold
        // locator across the per-table rebuild, so `delete_rows`
        // below no longer wipes prior-freeze cold entries. The pre-
        // v5.2.3 capture-then-re-register that used to live here
        // was removed in v5.3.1 — keeping it would double-count
        // every prior-frozen key's Cold locator on each subsequent
        // freeze.
        let bytes_before = self.get(table_name).expect("just validated").hot_bytes();
        let positions: Vec<usize> = (0..max_rows).collect();
        let t_mut = self
            .get_mut(table_name)
            .expect("just validated; still present");
        let removed = t_mut.delete_rows(&positions);
        debug_assert_eq!(removed, max_rows, "delete_rows count matches request");
        let bytes_after = t_mut.hot_bytes();
        let bytes_freed = bytes_before.saturating_sub(bytes_after);

        let segment_id = self
            .load_segment_bytes(seg_bytes.clone())
            .map_err(|e| StorageError::Corrupt(format!("freeze_oldest_to_cold: load: {e}")))?;
        let new_cold = post_swap_keys.into_iter().map(|k| {
            (
                k,
                RowLocator::Cold {
                    segment_id,
                    page_offset: 0,
                },
            )
        });
        let t_mut = self.get_mut(table_name).expect("still present");
        t_mut.register_cold_locators(index_name, new_cold)?;

        Ok(FreezeReport {
            segment_id,
            frozen_rows,
            bytes_freed,
            segment_bytes: seg_bytes,
        })
    }

    /// v5.1: borrow the cold segment at `segment_id`. Used by the
    /// spg-server preload path to enumerate (key, locator) pairs
    /// after loading a segment, so it can call
    /// [`Table::register_cold_locators`] without re-parsing the
    /// bytes.
    #[must_use]
    pub fn cold_segment(&self, segment_id: u32) -> Option<&OwnedSegment> {
        self.cold_segments
            .get(segment_id as usize)
            .and_then(|s| s.as_deref())
    }

    /// v5.1: resolve a single `RowLocator::Cold` to its underlying
    /// `Row`. Decoupled from [`Catalog::lookup_by_pk`] so callers
    /// iterating a multi-locator slice (e.g. the engine's index
    /// seek path) can dispatch per locator instead of getting back
    /// only the first row for a key. Returns `None` when the
    /// segment isn't registered, the key isn't `u64`-coercible, or
    /// the segment doesn't actually carry the key (bloom or page-
    /// index reject).
    pub fn resolve_cold_locator(
        &self,
        table_name: &str,
        segment_id: u32,
        key: &IndexKey,
    ) -> Option<Row<'static>> {
        let t = self.get(table_name)?;
        let u64_key = index_key_as_u64(key)?;
        let seg = self.cold_segments.get(segment_id as usize)?.as_ref()?;
        let payload = seg.lookup(u64_key)?;
        let (row, _) = decode_row_body_dense(&payload, &t.schema, seg.codec_version()).ok()?;
        // v7.39 (pg_stat blks knife) — one cold-tier "block read".
        self.cold_read_stats
            .cold_reads
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        Some(row)
    }

    /// v5.1: indexed PK lookup that dispatches per locator,
    /// returning the first matching row from either the hot tier
    /// (`Table::rows`) or a registered cold segment.
    ///
    /// The cold path requires the index column to be coercible to
    /// a `u64` (the segment's PK type) and the segment payload to
    /// be a [`encode_row_body_dense`]-encoded row body for the
    /// same schema. v5.1 ships this for BIGINT / INT / SMALLINT
    /// PKs; other types fall through to hot-only behavior.
    ///
    /// Returns `None` if (a) the table or index doesn't exist,
    /// (b) the key isn't in the index at all, or (c) the key was
    /// resolved to a stale locator (Hot index out of range, Cold
    /// segment id unknown, segment lookup miss). Does not surface
    /// segment-decode errors — those would indicate corrupted
    /// cold-tier files and should be caught at
    /// [`Catalog::load_segment_bytes`] time.
    pub fn lookup_by_pk(&self, table: &str, index_name: &str, key: &IndexKey) -> Option<Row<'_>> {
        let t = self.get(table)?;
        let idx = t.indices.iter().find(|i| i.name == index_name)?;
        let locators = idx.lookup_eq(key);
        let cold_u64_key = index_key_as_u64(key);
        for loc in locators {
            match *loc {
                RowLocator::Hot(i) => {
                    if let Some(row) = t.rows.get(i) {
                        return Some(row.clone());
                    }
                }
                RowLocator::Cold {
                    segment_id,
                    page_offset: _,
                } => {
                    let Some(u64_key) = cold_u64_key else {
                        // Key type not coercible to u64 — cold tier
                        // only handles BIGINT/INT/SMALLINT in v5.1.
                        continue;
                    };
                    let Some(seg) = self
                        .cold_segments
                        .get(segment_id as usize)
                        .and_then(|s| s.as_deref())
                    else {
                        // v6.7.3 — `None` slot = compaction
                        // retired this segment; the live locator
                        // on a freshly-compacted index points to
                        // the merged segment_id, so a Cold hit
                        // here against a tombstone means the BTree
                        // entry hasn't been swapped yet (mid-
                        // compaction reader race) or the caller is
                        // looking up a stale snapshot. Skip — the
                        // next locator in the list, if any, is
                        // typically the merged segment.
                        continue;
                    };
                    let Some(payload) = seg.lookup(u64_key) else {
                        continue;
                    };
                    let (row, _) =
                        decode_row_body_dense(&payload, &t.schema, seg.codec_version()).ok()?;
                    return Some(row);
                }
            }
        }
        None
    }

    /// v5.2.3: promote a frozen row back to the hot tier so an
    /// UPDATE / DELETE can mutate it. Reads the cold-tier row body
    /// (decoded from its registered segment), pushes it into
    /// `table.rows` via [`Table::insert`] (which also adds a fresh
    /// `Hot(new_idx)` locator on `index_name`), then retires the
    /// shadowed `Cold` locator via
    /// [`Table::remove_cold_locators_for_key`]. The cold-tier row
    /// in the segment file becomes garbage — recoverable when a
    /// future cold-segment compaction job lands.
    ///
    /// Returns:
    /// - `Ok(Some(new_hot_idx))` when the key resolved through a
    ///   cold locator and the promote completed. `new_hot_idx` is
    ///   the position the row now occupies in `table.rows`.
    /// - `Ok(None)` when the key has no Cold locator on the index
    ///   (already hot, or wasn't present at all). Callers treat this
    ///   as "nothing to do here, fall back to the hot-only path".
    ///
    /// Errors when the table / index doesn't exist, the index isn't
    /// `BTree`, the cold segment is missing / can't decode the row,
    /// or the inferred row body fails `Table::insert` validation.
    pub fn promote_cold_row(
        &mut self,
        table_name: &str,
        index_name: &str,
        key: &IndexKey,
    ) -> Result<Option<usize>, StorageError> {
        let cold_loc = self.find_cold_locator(table_name, index_name, key)?;
        let Some((segment_id, _page_offset)) = cold_loc else {
            return Ok(None);
        };
        let u64_key = index_key_as_u64(key).ok_or_else(|| {
            StorageError::Corrupt(
                "promote_cold_row: key type not coercible to u64 (cold tier requires integer PK)"
                    .into(),
            )
        })?;
        // Read the row body from the segment. Borrow the segment +
        // schema short-term so we can then take `&mut self` for the
        // hot-side insert.
        let schema = self
            .get(table_name)
            .ok_or_else(|| {
                StorageError::Corrupt(format!("promote_cold_row: table {table_name:?} not found"))
            })?
            .schema
            .clone();
        let seg = self
            .cold_segments
            .get(segment_id as usize)
            .and_then(|s| s.as_ref())
            .ok_or_else(|| {
                StorageError::Corrupt(format!(
                    "promote_cold_row: segment {segment_id} not registered on catalog"
                ))
            })?;
        let payload = seg.lookup(u64_key).ok_or_else(|| {
            StorageError::Corrupt(format!(
                "promote_cold_row: key {u64_key} resolves to segment {segment_id} \
                 but the segment's bloom/page lookup didn't return a row"
            ))
        })?;
        let (row, _consumed) = decode_row_body_dense(&payload, &schema, seg.codec_version())?;
        // Insert the promoted row into the hot tier. `Table::insert`
        // appends to `self.rows`, adds a `Hot(new_idx)` locator to
        // every BTree index covering the row's keyed columns, and
        // increments `hot_bytes`.
        let t = self
            .get_mut(table_name)
            .expect("table existed at lookup time");
        t.insert(row)?;
        let new_hot_idx =
            t.rows.len().checked_sub(1).ok_or_else(|| {
                StorageError::Corrupt("promote_cold_row: empty after insert".into())
            })?;
        // The hot insert added Hot(new_idx) alongside the still-
        // present Cold locator. Drop the Cold entry so future
        // lookups return only the fresh hot row.
        t.remove_cold_locators_for_key(index_name, key)?;
        Ok(Some(new_hot_idx))
    }

    /// v5.2.3: shadow a frozen row's index entry. Used by DELETE
    /// when the row to remove lives in a cold-tier segment — the
    /// row body stays in the segment file (becoming garbage) but
    /// every `Cold` locator for `key` on `index_name` is removed
    /// so PK lookups stop returning it.
    ///
    /// Returns the number of cold locators retired (0 when the key
    /// has no cold entries — the DELETE fell on a hot row or a
    /// key that was already absent). Errors when the table /
    /// index doesn't exist or the index isn't `BTree`.
    ///
    /// Cold-segment compaction (which merges shadowed-heavy
    /// segments and reclaims their disk footprint) lands in a
    /// later v5.x sub-version; until then, repeated UPDATE/DELETE
    /// of cold rows can amplify cold-segment disk usage by up to
    /// 1-2× — still well under typical LSM-tree shadowing because
    /// SPG segments are bulk-baked, not write-merged.
    pub fn shadow_cold_row(
        &mut self,
        table_name: &str,
        index_name: &str,
        key: &IndexKey,
    ) -> Result<usize, StorageError> {
        let t = self.get_mut(table_name).ok_or_else(|| {
            StorageError::Corrupt(format!("shadow_cold_row: table {table_name:?} not found"))
        })?;
        t.remove_cold_locators_for_key(index_name, key)
    }

    /// v6.7.4 — read-only slice preparation for the parallel
    /// freezer. Walks rows in `row_range`, builds the
    /// `(pk_u64, encoded_body, IndexKey)` triples that the
    /// coordinator's k-way merge consumes, sorts the slice by
    /// `pk_u64`, and returns a [`FreezeSlice`].
    ///
    /// Caller invariants:
    /// - `row_range.end <= table.rows.len()` (caller's job to
    ///   compute the partition).
    /// - All slices passed to `commit_freeze_slices` must cover a
    ///   contiguous half-open range `[0, total_max_rows)` with no
    ///   gaps and no overlaps. The coordinator validates this
    ///   invariant before committing.
    ///
    /// `&self`-only — multiple workers can run this concurrently
    /// against the same `Catalog` reference under the engine's
    /// write lock (workers don't mutate; the coordinator does).
    pub fn prepare_freeze_slice(
        &self,
        table_name: &str,
        index_name: &str,
        row_range: core::ops::Range<usize>,
    ) -> Result<FreezeSlice, StorageError> {
        let table = self.get(table_name).ok_or_else(|| {
            StorageError::Corrupt(format!(
                "prepare_freeze_slice: table {table_name:?} not found"
            ))
        })?;
        let idx = table
            .indices
            .iter()
            .find(|i| i.name == index_name)
            .ok_or_else(|| {
                StorageError::Corrupt(format!(
                    "prepare_freeze_slice: index {index_name:?} not found on {table_name:?}"
                ))
            })?;
        if !matches!(idx.kind, IndexKind::BTree(_)) {
            return Err(StorageError::Corrupt(format!(
                "prepare_freeze_slice: index {index_name:?} is NSW; only BTree indices may freeze"
            )));
        }
        if row_range.end > table.rows.len() {
            return Err(StorageError::Corrupt(format!(
                "prepare_freeze_slice: row_range end {} > row_count {}",
                row_range.end,
                table.rows.len()
            )));
        }
        let column_position = idx.column_position;
        let schema = table.schema.clone();
        let mut rows: Vec<(u64, Vec<u8>, IndexKey)> = Vec::with_capacity(row_range.len());
        for row_idx in row_range.clone() {
            let row = table.rows.get(row_idx).expect("bounds-checked above");
            let key = IndexKey::from_value(&row.values[column_position]).ok_or_else(|| {
                StorageError::Corrupt(format!(
                    "prepare_freeze_slice: row {row_idx} has NULL / non-key value in index column"
                ))
            })?;
            let pk_u64 = index_key_as_u64(&key).ok_or_else(|| {
                StorageError::Corrupt(format!(
                    "prepare_freeze_slice: index {index_name:?} column type is non-integer; \
                     v5.2.2 cold tier requires IndexKey::Int (Text PK lands in v5.5+)"
                ))
            })?;
            rows.push((pk_u64, encode_row_body_dense(row, &schema), key));
        }
        rows.sort_by_key(|(k, _, _)| *k);
        Ok(FreezeSlice { row_range, rows })
    }

    /// v6.7.4 — coordinator commit step. Merges N
    /// [`FreezeSlice`]s into one segment via the standard
    /// [`encode_segment`] path, atomically swaps the catalog
    /// state (delete the union row range + register Cold
    /// locators + load the segment).
    ///
    /// Validates that the slices cover a contiguous, gap-free,
    /// overlap-free half-open range starting at index 0 (the
    /// freezer always freezes "oldest first" — same semantics as
    /// the single-threaded [`Catalog::freeze_oldest_to_cold`]).
    ///
    /// Empty `slices` → no-op success (returns a zero-row report
    /// without mutating). Total row count = `Σ slice.rows.len()`.
    pub fn commit_freeze_slices(
        &mut self,
        table_name: &str,
        index_name: &str,
        slices: Vec<FreezeSlice>,
    ) -> Result<FreezeReport, StorageError> {
        // --- validation phase: never mutates ---------------------
        let table = self.get(table_name).ok_or_else(|| {
            StorageError::Corrupt(format!(
                "commit_freeze_slices: table {table_name:?} not found"
            ))
        })?;
        let idx = table
            .indices
            .iter()
            .find(|i| i.name == index_name)
            .ok_or_else(|| {
                StorageError::Corrupt(format!(
                    "commit_freeze_slices: index {index_name:?} not found on {table_name:?}"
                ))
            })?;
        if !matches!(idx.kind, IndexKind::BTree(_)) {
            return Err(StorageError::Corrupt(format!(
                "commit_freeze_slices: index {index_name:?} is NSW; only BTree indices may freeze"
            )));
        }
        // Validate slice coverage: contiguous from 0, no gaps, no
        // overlaps. Allow the caller to pass slices in any order —
        // sort by row_range.start first.
        let mut ordered = slices;
        ordered.sort_by_key(|s| s.row_range.start);
        // Drop fully-empty slices that fell out of an uneven
        // partition; they carry no data but contribute to the
        // contiguity check, so keep them in line.
        let mut expected_start = 0usize;
        for s in &ordered {
            if s.row_range.start != expected_start {
                return Err(StorageError::Corrupt(format!(
                    "commit_freeze_slices: gap/overlap at row {}; expected start {}",
                    s.row_range.start, expected_start
                )));
            }
            expected_start = s.row_range.end;
        }
        let max_rows = expected_start;
        if max_rows > table.rows.len() {
            return Err(StorageError::Corrupt(format!(
                "commit_freeze_slices: total row range {} exceeds row_count {}",
                max_rows,
                table.rows.len()
            )));
        }
        if max_rows == 0 {
            return Ok(FreezeReport {
                segment_id: u32::MAX,
                frozen_rows: 0,
                bytes_freed: 0,
                segment_bytes: Vec::new(),
            });
        }

        // --- segment build phase: reads only --------------------
        // K-way merge of already-sorted slices. Each slice's rows
        // are ascending by pk_u64; we keep a per-slice cursor and
        // pull the next-smallest head until every cursor drains.
        let total_rows: usize = ordered.iter().map(|s| s.rows.len()).sum();
        if total_rows != max_rows {
            return Err(StorageError::Corrupt(format!(
                "commit_freeze_slices: total slice rows {total_rows} ≠ row_range coverage {max_rows}"
            )));
        }
        let mut cursors: Vec<usize> = alloc::vec![0; ordered.len()];
        let mut merged: Vec<(u64, Vec<u8>, IndexKey)> = Vec::with_capacity(total_rows);
        loop {
            // Pick the slice whose head row has the smallest key
            // and isn't yet exhausted.
            let mut pick: Option<usize> = None;
            for (i, c) in cursors.iter().enumerate() {
                let slice = &ordered[i];
                if *c >= slice.rows.len() {
                    continue;
                }
                match pick {
                    None => pick = Some(i),
                    Some(j) => {
                        if slice.rows[*c].0 < ordered[j].rows[cursors[j]].0 {
                            pick = Some(i);
                        }
                    }
                }
            }
            let Some(i) = pick else { break };
            let row = ordered[i].rows[cursors[i]].clone();
            cursors[i] += 1;
            merged.push(row);
        }
        // Reject duplicate PKs — same error as the single-threaded
        // path so callers get a uniform surface.
        for w in merged.windows(2) {
            if w[0].0 == w[1].0 {
                return Err(StorageError::Corrupt(format!(
                    "commit_freeze_slices: duplicate PK {} across slices",
                    w[0].0
                )));
            }
        }
        let post_swap_keys: Vec<IndexKey> = merged.iter().map(|(_, _, k)| k.clone()).collect();
        let seg_rows: Vec<(u64, Vec<u8>)> =
            merged.into_iter().map(|(k, body, _)| (k, body)).collect();
        let frozen_rows = seg_rows.len();
        let (seg_bytes, _meta) = encode_segment(seg_rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES)
            .map_err(|e| StorageError::Corrupt(format!("commit_freeze_slices: encode: {e}")))?;

        // --- atomic swap phase: mutations only past this point ---
        let bytes_before = self.get(table_name).expect("just validated").hot_bytes();
        let positions: Vec<usize> = (0..max_rows).collect();
        let t_mut = self
            .get_mut(table_name)
            .expect("just validated; still present");
        let removed = t_mut.delete_rows(&positions);
        debug_assert_eq!(removed, max_rows, "delete_rows count matches request");
        let bytes_after = t_mut.hot_bytes();
        let bytes_freed = bytes_before.saturating_sub(bytes_after);

        let segment_id = self
            .load_segment_bytes(seg_bytes.clone())
            .map_err(|e| StorageError::Corrupt(format!("commit_freeze_slices: load: {e}")))?;
        let new_cold = post_swap_keys.into_iter().map(|k| {
            (
                k,
                RowLocator::Cold {
                    segment_id,
                    page_offset: 0,
                },
            )
        });
        let t_mut = self.get_mut(table_name).expect("still present");
        t_mut.register_cold_locators(index_name, new_cold)?;

        Ok(FreezeReport {
            segment_id,
            frozen_rows,
            bytes_freed,
            segment_bytes: seg_bytes,
        })
    }

    /// v6.7.3 — compact every cold segment on `(table, index)` whose
    /// `OwnedSegment::bytes().len()` is below `target_segment_bytes`
    /// into a single larger merged segment. Rows present in source
    /// segment payloads but no longer referenced by any
    /// `RowLocator::Cold` on the index (DELETE'd + frozen rows
    /// retired via [`Catalog::shadow_cold_row`]) are GC'd in the
    /// merge.
    ///
    /// **Semantics**:
    /// 1. Walk the BTree index to collect every Cold locator that
    ///    targets a small (< threshold) segment. Each such
    ///    `(key, segment_id)` becomes a row in the merged segment;
    ///    payload is looked up from the source segment in-place.
    /// 2. Encode the collected rows into one new segment via
    ///    [`encode_segment`]; register it via
    ///    [`Catalog::load_segment_bytes`] (allocating a fresh
    ///    `merged_segment_id` at the end of `cold_segments`).
    /// 3. Rewrite the BTree index in one pass: every
    ///    `RowLocator::Cold { segment_id ∈ sources }` becomes
    ///    `RowLocator::Cold { segment_id = merged_id, page_offset = 0 }`.
    ///    Hot locators are untouched.
    /// 4. Tombstone every source slot via
    ///    [`Catalog::tombstone_segment`]. Source segment payloads
    ///    are no longer reachable through the catalog; the on-disk
    ///    files are the caller's concern.
    ///
    /// On fewer than 2 candidate segments the catalog is **not**
    /// mutated and a no-op report (`merged_segment_id: None`,
    /// `sources: []`) is returned. This is the routine case — a
    /// freshly-frozen table has at most 1 small segment, no merge
    /// possible.
    ///
    /// Atomicity: every mutating step runs after the read-only
    /// gather phase, so a panic before the merge encode leaves the
    /// catalog unchanged. The mutation block itself (load + rewrite +
    /// tombstone) takes only `&mut self` — callers serialise the
    /// engine write lock outside this function.
    ///
    /// Errors when the table / index doesn't exist, the index isn't
    /// `BTree`, the index column type isn't u64-coercible (cold-tier
    /// pre-condition), or a source segment fails its in-place
    /// row-body lookup (would indicate prior catalog corruption).
    pub fn compact_cold_segments(
        &mut self,
        table_name: &str,
        index_name: &str,
        target_segment_bytes: u64,
    ) -> Result<CompactReport, StorageError> {
        // --- validation phase ----------------------------------
        let t = self.get(table_name).ok_or_else(|| {
            StorageError::Corrupt(format!(
                "compact_cold_segments: table {table_name:?} not found"
            ))
        })?;
        let idx = t
            .indices
            .iter()
            .find(|i| i.name == index_name)
            .ok_or_else(|| {
                StorageError::Corrupt(format!(
                    "compact_cold_segments: index {index_name:?} not found on {table_name:?}"
                ))
            })?;
        let map = match &idx.kind {
            IndexKind::BTree(m) => m,
            IndexKind::Nsw(_)
            | IndexKind::Brin { .. }
            | IndexKind::Gin(_)
            | IndexKind::GinTrgm(_)
            | IndexKind::GinFulltext(_)
            | IndexKind::GinJsonb(_) => {
                return Err(StorageError::Corrupt(format!(
                    "compact_cold_segments: index {index_name:?} is not BTree; \
                     compaction applies only to BTree cold-tier indices"
                )));
            }
        };

        // --- gather phase --------------------------------------
        // Step A: every segment_id this BTree index Cold-references.
        let mut referenced_ids: BTreeSet<u32> = BTreeSet::new();
        for (_key, locators) in map.iter() {
            for loc in locators {
                if let RowLocator::Cold { segment_id, .. } = loc {
                    referenced_ids.insert(*segment_id);
                }
            }
        }
        // Step B: keep only the small + still-active ones.
        let candidate_set: BTreeSet<u32> = referenced_ids
            .into_iter()
            .filter(|id| {
                self.cold_segments
                    .get(*id as usize)
                    .and_then(|s| s.as_deref())
                    .is_some_and(|s| (s.bytes().len() as u64) < target_segment_bytes)
            })
            .collect();
        if candidate_set.len() < 2 {
            return Ok(CompactReport {
                sources: Vec::new(),
                merged_segment_id: None,
                merged_segment_bytes: Vec::new(),
                merged_rows: 0,
                deleted_rows_pruned: 0,
                bytes_reclaimed_estimate: 0,
            });
        }
        // Step C: pre-count source rows for the deleted-pruned metric.
        let mut source_row_count: usize = 0;
        let mut source_byte_total: u64 = 0;
        for &id in &candidate_set {
            let seg = self.cold_segments[id as usize]
                .as_ref()
                .expect("candidate selected only when slot is Some");
            source_row_count = source_row_count.saturating_add(seg.meta().num_rows as usize);
            source_byte_total = source_byte_total.saturating_add(seg.bytes().len() as u64);
        }
        // Step D: collect (key, body) pairs from every live Cold
        // locator pointing at a candidate. dedupe by key — one
        // BTree key resolves to at most one cold payload (the
        // freezer + promote/shadow flow keeps Cold locators
        // unique per key).
        let mut collected: BTreeMap<u64, (Vec<u8>, IndexKey)> = BTreeMap::new();
        for (key, locators) in map.iter() {
            for loc in locators {
                let RowLocator::Cold { segment_id, .. } = loc else {
                    continue;
                };
                if !candidate_set.contains(segment_id) {
                    continue;
                }
                let u64_key = index_key_as_u64(key).ok_or_else(|| {
                    StorageError::Corrupt(format!(
                        "compact_cold_segments: index {index_name:?} has non-integer Cold key; \
                         cold tier requires IndexKey::Int (Text PK lands in v5.5+)"
                    ))
                })?;
                let seg = self.cold_segments[*segment_id as usize]
                    .as_ref()
                    .expect("candidate slot guaranteed Some above");
                let payload = seg.lookup(u64_key).ok_or_else(|| {
                    StorageError::Corrupt(format!(
                        "compact_cold_segments: BTree {index_name:?} points key={u64_key} \
                         at segment {segment_id} but the segment lookup missed"
                    ))
                })?;
                collected.insert(u64_key, (payload, key.clone()));
                break;
            }
        }
        let merged_rows = collected.len();
        let deleted_rows_pruned = source_row_count.saturating_sub(merged_rows);

        // Step E: encode the merged segment. `BTreeMap<u64, _>`
        // iteration is ascending by key, which is what
        // `encode_segment` requires.
        let seg_rows: Vec<(u64, Vec<u8>)> = collected
            .iter()
            .map(|(k, (body, _))| (*k, body.clone()))
            .collect();
        let (seg_bytes, _meta) = encode_segment(seg_rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES)
            .map_err(|e| StorageError::Corrupt(format!("compact_cold_segments: encode: {e}")))?;
        let merged_bytes_len = seg_bytes.len() as u64;

        // --- atomic mutation phase ------------------------------
        let merged_segment_id = self
            .load_segment_bytes(seg_bytes.clone())
            .map_err(|e| StorageError::Corrupt(format!("compact_cold_segments: load: {e}")))?;

        // Rewrite the BTree index: every Cold locator pointing at
        // a candidate source becomes a Cold locator pointing at
        // the merged segment. Use a flat collect-then-replace
        // pattern so we never hold a `&self` borrow across the
        // `&mut self` write.
        let entries: Vec<(IndexKey, Vec<RowLocator>)> = {
            let t = self
                .get(table_name)
                .expect("table existed at the start of this fn");
            let idx = t
                .indices
                .iter()
                .find(|i| i.name == index_name)
                .expect("index existed at the start of this fn");
            let IndexKind::BTree(map) = &idx.kind else {
                unreachable!("validated above");
            };
            map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        let t_mut = self
            .get_mut(table_name)
            .expect("table existed at the start of this fn");
        let idx_mut = t_mut
            .indices
            .iter_mut()
            .find(|i| i.name == index_name)
            .expect("index existed at the start of this fn");
        let IndexKind::BTree(map_mut) = &mut idx_mut.kind else {
            unreachable!("validated above");
        };
        for (key, locators) in entries {
            let mut new_locs: Vec<RowLocator> = Vec::with_capacity(locators.len());
            let mut changed = false;
            for loc in &locators {
                match *loc {
                    RowLocator::Cold {
                        segment_id,
                        page_offset: _,
                    } if candidate_set.contains(&segment_id) => {
                        let replacement = RowLocator::Cold {
                            segment_id: merged_segment_id,
                            page_offset: 0,
                        };
                        if !new_locs.contains(&replacement) {
                            new_locs.push(replacement);
                        }
                        changed = true;
                    }
                    other => new_locs.push(other),
                }
            }
            if changed {
                map_mut.insert_mut(key, new_locs);
            }
        }

        // Tombstone every source slot. Last step — failures here
        // would leave the segment double-referenced in both
        // memory + manifest, but `tombstone_segment` only errors
        // on out-of-bounds, which we've already validated.
        for &id in &candidate_set {
            self.tombstone_segment(id)?;
        }

        let bytes_reclaimed_estimate = source_byte_total.saturating_sub(merged_bytes_len);
        Ok(CompactReport {
            sources: candidate_set.into_iter().collect(),
            merged_segment_id: Some(merged_segment_id),
            merged_segment_bytes: seg_bytes,
            merged_rows,
            deleted_rows_pruned,
            bytes_reclaimed_estimate,
        })
    }

    /// Internal helper: scan `(table, index)` for a `Cold` locator
    /// keyed by `key`. Returns `Ok(Some((segment_id, page_offset)))`
    /// when found, `Ok(None)` when the key has only hot entries
    /// or no entries at all, `Err` on the same input-validation
    /// errors as the public `promote_cold_row` / `shadow_cold_row`.
    fn find_cold_locator(
        &self,
        table_name: &str,
        index_name: &str,
        key: &IndexKey,
    ) -> Result<Option<(u32, u32)>, StorageError> {
        let t = self.get(table_name).ok_or_else(|| {
            StorageError::Corrupt(format!("find_cold_locator: table {table_name:?} not found"))
        })?;
        let idx = t
            .indices
            .iter()
            .find(|i| i.name == index_name)
            .ok_or_else(|| {
                StorageError::Corrupt(format!(
                    "find_cold_locator: index {index_name:?} not found on {table_name:?}"
                ))
            })?;
        if !matches!(idx.kind, IndexKind::BTree(_)) {
            return Err(StorageError::Corrupt(format!(
                "find_cold_locator: index {index_name:?} is NSW; promote-on-write only applies to BTree indices"
            )));
        }
        for loc in idx.lookup_eq(key) {
            if let RowLocator::Cold {
                segment_id,
                page_offset,
            } = *loc
            {
                return Ok(Some((segment_id, page_offset)));
            }
        }
        Ok(None)
    }
}

/// Coerce an [`IndexKey`] to the `u64` that v5.1 cold-tier
/// segments use as their on-disk PK. Returns `None` for keys that
/// aren't representable as `u64` — Text PKs need a hash mapping
/// the segment writer baked in (deferred to v5.2+), Bool PKs are
/// almost never wide enough to be sharded into a cold tier.
fn index_key_as_u64(key: &IndexKey) -> Option<u64> {
    match key {
        // Reinterpret the i64 bit pattern as u64. Cold-tier segments
        // are sorted by this u64 view, so the chosen interpretation
        // only has to match between insert (bake_segment / freezer)
        // and lookup — using cast_unsigned keeps both sides honest
        // and silences clippy::cast_sign_loss.
        IndexKey::Int(n) => Some(n.cast_unsigned()),
        // Text / Bool / Uuid PKs aren't representable as u64 and so
        // can't participate in the u64-sorted cold-tier segment
        // PK layout. Same deferral story as Text — lookup falls
        // through the in-memory btree.
        IndexKey::Text(_) | IndexKey::Bool(_) | IndexKey::Uuid(_) => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StorageError {
    DuplicateTable {
        name: String,
    },
    TableNotFound {
        name: String,
    },
    ArityMismatch {
        expected: usize,
        actual: usize,
    },
    TypeMismatch {
        column: String,
        expected: DataType,
        actual: DataType,
        position: usize,
    },
    NullInNotNull {
        column: String,
    },
    /// Index with this name already exists on the table.
    DuplicateIndex {
        name: String,
    },
    /// Column referenced by an index doesn't exist on the table.
    ColumnNotFound {
        column: String,
    },
    /// On-disk format failed to parse — corrupted file, wrong magic, truncated
    /// payload, or unknown tag bytes.
    Corrupt(String),
    /// v6.0.4 — ALTER INDEX targeted an index name that doesn't
    /// exist on any table in this catalog.
    IndexNotFound {
        name: String,
    },
    /// v6.0.4 — operation requested isn't supported on this index
    /// kind / column type (e.g. ALTER INDEX REBUILD on a `BTree`
    /// index, or REBUILD WITH (encoding=…) on a non-vector column).
    Unsupported(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // v7.39 (read01 round 47) — PG's 42P07 wording.
            Self::DuplicateTable { name } => write!(f, "relation \"{name}\" already exists"),
            // v7.39 (read01 round 47) — PG's wording for a missing relation
            // (42P01). DROP TABLE says "table" and raises its own error at
            // the engine; every other path (SELECT / ALTER / …) says
            // "relation", which is what this carries.
            Self::TableNotFound { name } => write!(f, "relation \"{name}\" does not exist"),
            Self::ArityMismatch { expected, actual } => write!(
                f,
                "row arity mismatch: expected {expected} columns, got {actual}"
            ),
            Self::TypeMismatch {
                column,
                expected,
                actual,
                position,
            } => write!(
                f,
                "type mismatch in column {column:?} (position {position}): expected {expected}, got {actual}"
            ),
            Self::NullInNotNull { column } => {
                // v7.39 (SQLSTATE fidelity) — PG's 23502 phrasing (the
                // relation-qualified long form is added by engine call
                // sites that know the table name).
                write!(f, "null value in column \"{column}\" violates not-null constraint")
            }
            // v7.39 (read01 round 47) — an index is a relation to PG (42P07).
            Self::DuplicateIndex { name } => write!(f, "relation \"{name}\" already exists"),
            Self::ColumnNotFound { column } => write!(f, "column not found: {column}"),
            Self::Corrupt(detail) => write!(f, "corrupt on-disk format: {detail}"),
            Self::IndexNotFound { name } => write!(f, "index not found: {name}"),
            Self::Unsupported(detail) => write!(f, "unsupported: {detail}"),
        }
    }
}

impl ColumnSchema {
    pub fn new(name: impl Into<String>, ty: DataType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            ty,
            nullable,
            default: None,
            runtime_default: None,
            auto_increment: false,
            user_enum_type: None,
            user_domain_type: None,
            user_composite_type: None,
            acl: Vec::new(),
            on_update_runtime: None,
            collation: Collation::Binary,
            is_unsigned: false,
            inline_enum_variants: None,
            inline_set_variants: None,
            generated_stored_expr: None,
            identity_always: false,
            default_text: None,
        }
    }

    /// Builder-style helper to attach a default value to an otherwise
    /// plain column schema. Used by the engine when CREATE TABLE
    /// specifies `column TYPE DEFAULT <expr>`.
    #[must_use]
    pub fn with_default(mut self, default: Value<'static>) -> Self {
        self.default = Some(default);
        self
    }

    /// v7.9.21 — builder for runtime-evaluated defaults
    /// (`DEFAULT now()`, `DEFAULT CURRENT_TIMESTAMP`, …).
    /// `expr` is the Expr's `Display` form, re-parsed by the
    /// engine at each INSERT.
    #[must_use]
    pub fn with_runtime_default(mut self, expr: impl Into<String>) -> Self {
        self.runtime_default = Some(expr.into());
        self
    }

    /// Builder-style helper to mark a column as `AUTO_INCREMENT`.
    #[must_use]
    pub const fn with_auto_increment(mut self) -> Self {
        self.auto_increment = true;
        self
    }
}

impl TableSchema {
    pub fn new(name: impl Into<String>, columns: Vec<ColumnSchema>) -> Self {
        Self {
            name: name.into(),
            columns,
            hot_tier_bytes: None,
            foreign_keys: Vec::new(),
            uniqueness_constraints: Vec::new(),
            checks: Vec::new(),
            partition_role: None,
            policies: Vec::new(),
            row_security: false,
            force_row_security: false,
            owner: None,
            acl: Vec::new(),
        }
    }
}

// =========================================================================
// Persistent binary format for the catalog.
//
// Layout (little-endian throughout):
//
//   [magic "SPGDB001" 8 bytes][version u8]
//   [table_count u32]
//   for each table:
//       [name_len u16][name bytes]
//       [col_count u16]
//       for each col:
//           [name_len u16][name bytes]
//           [type_tag u8 + optional payload]
//               1=Int 2=BigInt 3=Float 4=Text 5=Bool
//               6=Vector(u32 dim)
//               7=SmallInt
//               8=Varchar(u32 max)
//               9=Char(u32 size)
//               10=Numeric(u8 precision, u8 scale)
//               11=Date
//               12=Timestamp
//           [nullable u8]   0/1
//           [default_tag u8] 0=none 1=value (followed by [value_tag u8] + bytes)
//       [row_count u32]
//       for each row, for each col, one [value_tag u8] + value bytes:
//           tag 0 (Null)     → no body
//           tag 1 (Int)      → i32 LE
//           tag 2 (BigInt)   → i64 LE
//           tag 3 (Float)    → f64 LE
//           tag 4 (Text)     → u16 LE len + UTF-8 bytes
//           tag 5 (Bool)     → u8 0/1
//           tag 6 (Vector)   → u32 LE dim + dim×f32 LE
//           tag 7 (SmallInt) → i16 LE
//           tag 8 (Numeric)  → i128 LE (16 bytes) + u8 scale
//           tag 9 (Date)     → i32 LE (days since Unix epoch)
//           tag 10 (Timestamp) → i64 LE (microseconds since Unix epoch)
//
// Bumped to version 3 when NUMERIC was added; to version 4 when
// AUTO_INCREMENT (per-column flag) + NSW index `kind` byte landed;
// to version 5 when DATE / TIMESTAMP were added; to version 6 when
// NSW graph topology started travelling on disk (v2.7); to version 7
// when the NSW topology became multi-layer HNSW (v2.13); to version 8
// when row encoding switched to schema-driven dense layout (v3.0.2 —
// per-row NULL bitmap + per-column fixed-width body, no per-cell type
// tag).
// =========================================================================

const FILE_MAGIC: &[u8; 8] = b"SPGDB001";
/// Current catalog snapshot format version emitted by [`Catalog::serialize`].
///
/// v9 (v5.2) extends v8 by serialising `BTree` index entries directly — every
/// `(IndexKey, Vec<RowLocator>)` pair travels on disk with the v5.1
/// `RowLocator::write_le` tag-prefixed codec. v8 `BTree` indices stored no
/// entries at all (the map was rebuilt from `Table::rows` on load); v9
/// preserves on-disk Cold locators so freezer-produced cold-tier index
/// entries survive a catalog snapshot round-trip. v8 readers are accepted
/// by version dispatch in [`Catalog::deserialize`] — every entry decodes
/// as `RowLocator::Hot(_)` via `add_index` rebuild, identical to v5.1
/// behaviour.
/// v6.7.2 — bumped from 10 to 11 to append per-table
/// `hot_tier_bytes: Option<u64>` after the per-table indices
/// section. v10 catalogs (v6.7.1) load with `hot_tier_bytes =
/// None` for every table (the deserialiser short-circuits when
/// version < 11). v11 snapshots written by a pre-v6.7.2 binary
/// fail loudly at the version check, matching the v6.1.2 /
/// v6.1.4 / v6.2.0 / v6.7.1 envelope-bump upgrade fences.
///
/// v6.8.0 — bumped from 11 to 12: per-index
/// `included_columns: Vec<u16>` appended at the tail of each
/// index payload. v11 (= v6.7.2) catalogs load with
/// `included_columns = Vec::new()` for every index — same
/// "older readers, append-only extension" pattern as the v6.7.2
/// hot_tier_bytes byte.
/// v7.13.0 — bumped from 22 to 23. mailrs round-5 G3 / G10.
/// Per-table appendix gains two new sections:
///   * `checks: Vec<String>` — CHECK predicate sources (Display
///     form of the AST Expr); re-parsed on INSERT/UPDATE to
///     enforce against candidate rows. Same persistence pattern
///     as `Index::partial_predicate`.
///   * Per `UniquenessConstraint`: trailing `nulls_not_distinct:
///     u8` flag for PG 15+ `UNIQUE NULLS NOT DISTINCT (cols)`
///     semantics.
/// v22 catalogs deserialise with empty `checks` and every UC
/// at `nulls_not_distinct = false`.
/// v24 introduces:
///   * Index kind tag 4 = trigram-GIN (`gin_trgm_ops`-flavoured
///     `USING gin` over a TEXT/VARCHAR column). Payload shape is
///     identical to tag-3 GIN (String → Vec<RowLocator>); the
///     keys are PG-compatible 3-byte trigram shingles instead of
///     tsvector lexemes. v23 catalogs deserialise unchanged — no
///     v23 writer ever emitted tag 4.
/// v25 introduces:
///   * Per `TriggerDef`: trailing `enabled: u8` flag (mailrs
///     round-9 A.2.b — `ALTER TABLE … { ENABLE | DISABLE }
///     TRIGGER …`). v24 catalogs deserialise with every trigger
///     `enabled = true`, matching pre-v7.16.1 behaviour.
/// v26 introduces (v7.17.0 Phase 1.1):
///   * Trailing SEQUENCE catalog block after triggers. Encoded
///     as `u32 count` followed by per-sequence:
///     `name`, `data_type: u8` (0=SmallInt,1=Int,2=BigInt),
///     `start i64`, `increment i64`, `min_value i64`,
///     `max_value i64`, `cache i64`, `cycle u8`,
///     `owned_by_tag u8` (0=NONE, 1=Column → `table`,`column`),
///     `last_value i64`, `is_called u8`. v25-and-below catalogs
///     deserialise with an empty sequences map.
/// v27 introduces (v7.17.0 Phase 1.2):
///   * Trailing VIEW catalog block after sequences. Encoded as
///     `u32 count` followed by per-view:
///     `name`, `column_count u16`, then column names, then
///     `body` long-string. v26-and-below catalogs deserialise
///     with an empty views map.
/// v28 introduces (v7.17.0 Phase 1.3):
///   * Trailing MATERIALIZED VIEW source registry block after
///     views. Encoded as `u32 count` followed by per-entry:
///     `name`, `body` long-string. The materialised rows live
///     as a regular Table of the same name (already covered by
///     the pre-existing tables block). v27-and-below catalogs
///     deserialise with an empty map.
/// v29 introduces (v7.17.0 Phase 1.4):
///   * Per-table user_enum_type appendix (after the CHECK
///     appendix). Layout: `u16 count` followed by per-binding
///     `[u16 col_pos][str enum_name]`. Only columns whose
///     `user_enum_type` is Some land here; the catalog stays
///     compact for the common no-enum case.
///   * Trailing ENUM types catalog block after materialized
///     views. Encoded as `u32 count` followed by per-entry:
///     `name`, `u16 label_count`, then `label_count` short
///     strings. v28-and-below catalogs deserialise with an
///     empty enum_types map and every column's
///     `user_enum_type = None`.
/// v30 introduces (v7.17.0 Phase 1.5):
///   * Per-table user_domain_type appendix (after the
///     user_enum_type appendix). Same shape as the enum one.
///   * Trailing DOMAIN types catalog block after the enum
///     block. Encoded as `u32 count` followed by per-entry:
///     `name`, `data_type` byte, `nullable u8`,
///     `default_present u8` + optional default string,
///     `u16 check_count` then `check_count` Display-form
///     CHECK strings. v29-and-below catalogs deserialise with
///     an empty domain_types map and `user_domain_type = None`.
/// v31 introduces (v7.17.0 Phase 1.6):
///   * Trailing user-schemas block after the DOMAIN block.
///     Encoded as `u32 count` followed by `count` schema-name
///     short strings. Built-in schemas (`public`, `pg_catalog`,
///     `information_schema`) are NOT serialised — they're
///     hardcoded in `is_builtin_schema`. v30-and-below catalogs
///     deserialise with an empty user-schemas set.
/// v32 introduces (v7.17.0 Phase 2.1):
///   * Per-table on_update_runtime appendix (after the
///     user_domain_type appendix). Layout: `u16 count` followed
///     by per-binding `[u16 col_pos][str expr_src]`. Only
///     columns whose `on_update_runtime` is Some land here;
///     the catalog stays compact when no MySQL-shaped table
///     uses the attribute. v31-and-below catalogs deserialise
///     with every column's `on_update_runtime = None`.
/// v33 introduces (v7.17.0 Phase 2.2):
///   * Index kind tag 5 = fulltext-GIN (MySQL `FULLTEXT KEY`
///     surface over a TEXT / VARCHAR column). Payload shape is
///     identical to tag-3 / tag-4 GIN (`String → Vec<RowLocator>`);
///     the keys are lower-cased word lexemes (same rule as
///     `to_tsvector('simple', text)`). v32 catalogs deserialise
///     unchanged — no v32 writer ever emitted tag 5, and FULLTEXT
///     KEY was silently dropped pre-v7.17 so no rebuild shim is
///     needed for round-tripped catalogs.
/// v34 introduces (v7.17.0 Phase 2.5):
///   * Per-table collation appendix (after the on_update_runtime
///     appendix). Sparse layout: only columns whose `collation`
///     is non-Binary land here. `u16 count` then per-binding
///     `[u16 col_pos][u8 collation_tag]` where the tag matches
///     `Collation::TAG_*`. Snapshots written by v33-and-below
///     readers deserialise every column with `collation =
///     Binary`, preserving the prior byte-wise compare
///     semantics. Unknown tags read back as Binary too — keeps
///     a forward-compat path if a future v35 adds variants
///     and someone rolls back to a v34 reader.
/// v35 introduces (v7.17.0 Phase 4.4):
///   * Per-table is_unsigned appendix (after the collation
///     appendix). Sparse layout: only `is_unsigned = true`
///     columns land. `u16 count` then per-binding `[u16 col_pos]`.
///     v34-and-below catalogs deserialise every column as
///     `is_unsigned = false`, preserving the prior silent-
///     accept behaviour for negative inserts on UNSIGNED columns.
/// v46 introduces (v7.23, mailrs round-14):
///   * Escaped short-string codec — `write_str` lengths >= 0xFFFF
///     emit `[u16 0xFFFF][u32 real_len]` so TEXT cells (mail bodies,
///     document text) above 64 KiB encode instead of panicking.
///     One-way upgrade: v45-and-below readers reject v46 catalogs
///     loudly via the version gate; v46 readers decode v45 catalogs
///     with the plain-u16 rules (0xFFFF is a legitimate length
///     there).
/// v47 introduces (v7.27, mailrs round-21):
///   * Escaped lengths for the REMAINING u16-length cell payloads —
///     BYTEA cells, TEXT[] elements, tsvector lexemes and tsquery
///     terms — the same `[u16 0xFFFF][u32 real_len]` escape v46
///     gave short strings. Round-14 fixed TEXT and missed these;
///     round-21 fired the BYTEA twin during a production migration.
///     One-way upgrade, same posture as v46.
/// v48 introduces (v7.37.5 β-P2, sentori cutover window):
///   * `INTERVAL` becomes a real column type. Catalog tag 34 in
///     `write_data_type`; per-row body is a fixed 16 bytes
///     (i64 micros + i32 days + i32 months, LE, PG-byte-equal
///     field order). The runtime-only days collapse is gone —
///     `'1 day'` and `'24 hours'` are stored distinctly. One-way
///     upgrade: v47 catalogs without INTERVAL columns deserialise
///     identically; v47 readers fed a v48 catalog that contains
///     INTERVAL hit the explicit "unknown data type tag: 34"
///     fence in `read_data_type`.
/// v49 introduces (v7.37.6-B, sentori Epic 2 P0):
///   * Per-table partition role appendix(declarative
///     `PARTITION BY RANGE` parent / range child / DEFAULT
///     child)。Layout, written **after** the inline_set_variants
///     appendix and **before** the per-table block close:
///       `[u8 role_tag]`
///         0 = `None`(普通表,后向兼容默认)
///         1 = `Parent`:  `[u8 kind_tag (0=Range)]`
///                        `[u16 key_col_count]` `(× u16 col_pos)`
///                        `[u16 tmpl_count]` `(× str source)`
///         2 = `Range`:   `[str parent_name]` `[Bound]` `[Bound]`
///         3 = `Default`: `[str parent_name]`
///     `PartitionBound` codec:
///       `[u8 bound_tag]` 0=MinValue 1=MaxValue 2=TimestampTz(`[i64 LE micros]`)
///     v48-and-below readers stop after the inline_set_variants
///     block — they don't see this appendix and deserialise every
///     table with `partition_role = None`. v49 writers always emit
///     `[0]` for plain tables, so the encoding stays one-byte-cheap.
/// v50 introduces (v7.37.7, sentori Epic 3 P1):
///   * Per-table `generated_stored_expr` appendix(stored generated
///     columns — `GENERATED ALWAYS AS (<expr>) STORED`)。Layout,
///     written **after** the partition_role appendix and before
///     the per-table block close:
///       `[u16 binding_count]`
///       `binding_count × { [u16 col_pos][str expr_source] }`
///     Sparse — only generated columns land here, so plain-shape
///     catalogs stay byte-for-byte identical save for the new
///     u16 zero count. v49-and-below readers stop after the
///     partition_role appendix; v50 readers default every column
///     to `generated_stored_expr = None` when this block is absent.
/// v51 introduces (v7.37.8, sentori Epic 5 P2):
///   * Per-index tag byte 6 = `GinJsonb`(real posting-list GIN
///     over a JSONB column). Payload shape mirrors tag-3 / 4 / 5:
///     `[u32 posting_list_count]` then `(str token, u32 locator_count,
///     locators …)` per posting list. Same `write_str` /
///     `RowLocator::write_le` codec as the rest of the GIN family.
///     v50 catalogs never wrote tag 6(the same DDL loaded as a
///     BTree fallback); v51 readers see tag 6 explicitly and dispatch
///     into `IndexKind::GinJsonb`.
/// v52 introduces (v7.37.42-T2 ζ-B composite + domain metasystem):
///   * Trailing COMPOSITE-types catalog block after the
///     user-schemas block. Encoded as `u32 count` followed by
///     per-entry: `name`, `u16 field_count`, then `field_count`
///     `[str field_name][data_type]` pairs (`write_data_type` is
///     reused). v51-and-below catalogs deserialise with an empty
///     composite_types map; v52 readers tolerate v51 catalogs by
///     stopping at the schema block (no composite block present
///     ⇒ empty map). Composite types are referenced by columns
///     via `ColumnSchema.user_composite_type`, mirroring the
///     `user_enum_type` / `user_domain_type` pattern. The block
///     lands here (not as a per-table appendix) so dropping the
///     composite type registers globally and DROP TYPE can find it
///     without a table scan.
/// v53 introduces (v7.37.16 Epic W — cross-checkpoint tombstone
///   durability):
///   * Trailing per-table MVCC appendix carrying, for every row,
///     its `RowHeader` (`xmin:u64`, `xmax:u64`, `flags:u8`) and its
///     stable `RowId` (`u64`), followed by the relation's
///     `next_rowid:u64`. Layout per table (after the v50
///     generated_stored_expr block, before the table loop closes):
///       `[u32 row_count]` (== `Table::rows().len()`, cross-check)
///       per row in physical order:
///         `[u64 xmin][u64 xmax][u8 flags][u64 rowid]`
///       `[u64 next_rowid]`
///     v52-and-below catalogs never wrote this block; their reader
///     stops after the last per-table appendix and
///     `deserialize_rows` leaves every row `RowHeader::frozen()`
///     with dense 1..=N ids — the exact pre-v53 contract. A v53
///     reader instead reconstructs headers + ids VERBATIM, so a
///     tombstone-redo naming a row inserted before the last
///     checkpoint resolves by `RowId` across the base-snapshot
///     boundary (closing the coupling the Epic W WAL slices deferred
///     to this format bump). Because the reader routes on `version`,
///     the block is strictly backward-compatible: old images load
///     byte-for-byte as before. `SPG_MVCC_INPLACE` is unaffected —
///     a gate-off database's rows are all frozen/alive, so
///     persisting + restoring their headers is observationally a
///     no-op.
/// v7.38 (read01 P5.05) — v54 appends a CRC32C over the whole preceding
/// image so a corrupted `base.spg` is caught on load instead of silently
/// deserialising garbage. Older images (v8..=53) carry no trailer and load
/// unchanged.
const FILE_VERSION: u8 = 68;
/// First version that appends the trailing CRC32C integrity trailer.
const FILE_VERSION_CRC_TRAILER: u8 = 54;
/// Oldest format version [`Catalog::deserialize`] still accepts. v8 is the
/// v3.0.2 dense-row layout; pre-v8 catalogs require an offline migration.
const MIN_SUPPORTED_FILE_VERSION: u8 = 8;

// IndexKey wire format (v9):
//   tag 0 = Int  → [i64 LE]
//   tag 1 = Text → [u16 LE len + UTF-8 bytes] (via write_str / read_str)
//   tag 2 = Bool → [u8 0/1]
const INDEX_KEY_TAG_INT: u8 = 0;
const INDEX_KEY_TAG_TEXT: u8 = 1;
const INDEX_KEY_TAG_BOOL: u8 = 2;
/// v7.17.0 — `IndexKey::Uuid([u8; 16])`. Body = raw 16 bytes
/// (RFC 4122 byte order). Persisted only in FILE_VERSION 36+
/// catalogs.
const INDEX_KEY_TAG_UUID: u8 = 3;

impl Catalog {
    /// Serialize the whole catalog (schema + every row) into a self-contained
    /// byte buffer. Format is documented above the impl block.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(FILE_MAGIC);
        out.push(FILE_VERSION);
        write_u32(
            &mut out,
            u32::try_from(self.tables.len()).expect("≤ 4G tables"),
        );
        for t in &self.tables {
            write_str(&mut out, &t.schema.name);
            write_u16(
                &mut out,
                u16::try_from(t.schema.columns.len()).expect("≤ 65k columns/table"),
            );
            for c in &t.schema.columns {
                write_str(&mut out, &c.name);
                write_data_type(&mut out, c.ty);
                out.push(u8::from(c.nullable));
                match &c.default {
                    None => out.push(0),
                    Some(v) => {
                        out.push(1);
                        write_value(&mut out, v);
                    }
                }
                out.push(u8::from(c.auto_increment));
            }
            write_u32(
                &mut out,
                u32::try_from(t.rows.len()).expect("≤ 4G rows/table"),
            );
            // v3.0.2 dense row encoding (FILE_VERSION 8): per-row NULL
            // bitmap, then tightly-packed bodies. Identical wire format
            // as before — extracted into `encode_row_body_dense` so cold-
            // tier segments (v5.1+) can share the encoding.
            for row in &t.rows {
                out.extend_from_slice(&encode_row_body_dense(row, &t.schema));
            }
            // Index definitions. Per-index payload:
            //   [name][col_pos u16][kind u8]
            //     kind 0 = B-tree           (no params — rebuilt on load)
            //     kind 1 = NSW graph        (u16 M + serialized graph)
            // For NSW the graph topology travels on disk so startup
            // doesn't re-run the O(n²M) rebuild — see v2.7 notes.
            write_u16(
                &mut out,
                u16::try_from(t.indices.len()).expect("≤ 65k indices/table"),
            );
            for idx in &t.indices {
                write_str(&mut out, &idx.name);
                write_u16(
                    &mut out,
                    u16::try_from(idx.column_position).expect("≤ 65k columns/table"),
                );
                match &idx.kind {
                    IndexKind::BTree(map) => {
                        out.push(0);
                        // v9: serialise the full PB map. Each entry's
                        // RowLocator list travels with the tag-prefixed
                        // codec from `row_locator::write_le`, so freezer-
                        // produced Cold locators survive a snapshot
                        // round-trip. v8 BTree wrote nothing here and
                        // rebuilt from rows — v9 readers tolerate v8 by
                        // version dispatch in `Catalog::deserialize`.
                        write_u32(
                            &mut out,
                            u32::try_from(map.len()).expect("≤ 4G index entries/index"),
                        );
                        for (key, locators) in map {
                            write_index_key(&mut out, key);
                            write_u32(
                                &mut out,
                                u32::try_from(locators.len()).expect("≤ 4G locators/key"),
                            );
                            for loc in locators {
                                loc.write_le(&mut out);
                            }
                        }
                    }
                    IndexKind::Nsw(g) => {
                        out.push(1);
                        write_u16(&mut out, u16::try_from(g.m).expect("≤ 65k NSW neighbours"));
                        write_nsw_graph(&mut out, g);
                    }
                    IndexKind::Brin { column_type } => {
                        // v6.7.1 — tag byte 2 = BRIN. Payload is the
                        // column type code (1 byte mapping to the
                        // shared DataType numeric encoding); no
                        // further data — BRIN summaries live in
                        // cold segments, not the catalog.
                        out.push(2);
                        write_data_type(&mut out, *column_type);
                    }
                    IndexKind::Gin(map) => {
                        // v7.12.3 — tag byte 3 = GIN. Payload mirrors
                        // the BTree encoding but with String (lexeme
                        // word) keys instead of IndexKey. Tag-prefixed
                        // RowLocator codec so freezer-produced Cold
                        // locators survive snapshot round-trip.
                        // FILE_VERSION 21+; v20 catalogs never wrote a
                        // GIN index (the AM degraded to BTree fallback
                        // pre-v7.12.3), so no migration shim is needed.
                        out.push(3);
                        write_u32(
                            &mut out,
                            u32::try_from(map.len()).expect("≤ 4G GIN posting lists"),
                        );
                        for (word, locators) in map {
                            write_str(&mut out, word);
                            write_u32(
                                &mut out,
                                u32::try_from(locators.len()).expect("≤ 4G locators/posting list"),
                            );
                            for loc in locators {
                                loc.write_le(&mut out);
                            }
                        }
                    }
                    IndexKind::GinTrgm(map) => {
                        // v7.15.0 — tag byte 4 = GinTrgm
                        // (`gin_trgm_ops` GIN over a TEXT column).
                        // Payload shape is identical to tag-3 GIN —
                        // `String → Vec<RowLocator>` posting lists.
                        // The String keys are 3-byte trigrams instead
                        // of tsvector lexemes; the deserializer
                        // dispatches on the tag, not the key shape.
                        // FILE_VERSION 24+; v23 catalogs never wrote
                        // a trigram-GIN.
                        out.push(4);
                        write_u32(
                            &mut out,
                            u32::try_from(map.len()).expect("≤ 4G trigram-GIN posting lists"),
                        );
                        for (tri, locators) in map {
                            write_str(&mut out, tri);
                            write_u32(
                                &mut out,
                                u32::try_from(locators.len()).expect("≤ 4G locators/posting list"),
                            );
                            for loc in locators {
                                loc.write_le(&mut out);
                            }
                        }
                    }
                    IndexKind::GinFulltext(map) => {
                        // v7.17.0 Phase 2.2 — tag byte 5 =
                        // GinFulltext (MySQL `FULLTEXT KEY` GIN
                        // over a TEXT/VARCHAR column). Payload
                        // shape mirrors tag-3 / tag-4 GIN —
                        // `String → Vec<RowLocator>` posting
                        // lists keyed by lower-cased word
                        // lexemes. FILE_VERSION 33+; v32 catalogs
                        // never wrote a fulltext-GIN (FULLTEXT
                        // KEY was silently dropped pre-v7.17).
                        out.push(5);
                        write_u32(
                            &mut out,
                            u32::try_from(map.len()).expect("≤ 4G fulltext-GIN posting lists"),
                        );
                        for (lex, locators) in map {
                            write_str(&mut out, lex);
                            write_u32(
                                &mut out,
                                u32::try_from(locators.len()).expect("≤ 4G locators/posting list"),
                            );
                            for loc in locators {
                                loc.write_le(&mut out);
                            }
                        }
                    }
                    IndexKind::GinJsonb(map) => {
                        // v7.37.8 — tag byte 6 = GinJsonb
                        // (real posting-list GIN over a JSONB
                        // column; sentori Epic 5 P2). Payload
                        // shape mirrors tag-3 / 4 / 5 — keys are
                        // the canonical `(path, leaf)` tokens
                        // from `jsonb_gin::extract_tokens`.
                        // FILE_VERSION 51+; v50 catalogs never
                        // wrote a JSONB-GIN (the same DDL loaded
                        // as a BTree fallback).
                        out.push(6);
                        write_u32(
                            &mut out,
                            u32::try_from(map.len()).expect("≤ 4G JSONB-GIN posting lists"),
                        );
                        for (token, locators) in map {
                            write_str(&mut out, token);
                            write_u32(
                                &mut out,
                                u32::try_from(locators.len()).expect("≤ 4G locators/posting list"),
                            );
                            for loc in locators {
                                loc.write_le(&mut out);
                            }
                        }
                    }
                }
                // v6.8.0 — included_columns appendix per index.
                // Layout: [u16 num_included][num × u16 column_position].
                // v11 readers stop before this u16 (deserialise loop
                // gated on version >= 12); v12+ readers always
                // consume it. Empty Vec serialises as a bare 0u16.
                write_u16(
                    &mut out,
                    u16::try_from(idx.included_columns.len()).expect("≤ 65k INCLUDE columns/index"),
                );
                for col_pos in &idx.included_columns {
                    write_u16(
                        &mut out,
                        u16::try_from(*col_pos).expect("≤ 65k columns/table"),
                    );
                }
                // v6.8.1 — partial_predicate appendix per index.
                // Layout: [u8 has_pred][u16 LE len][bytes (if has_pred)].
                // Same v12 gate as included_columns.
                match &idx.partial_predicate {
                    None => out.push(0),
                    Some(pred) => {
                        out.push(1);
                        write_str(&mut out, pred);
                    }
                }
                // v6.8.2 — expression appendix. Same shape as
                // partial_predicate.
                match &idx.expression {
                    None => out.push(0),
                    Some(expr) => {
                        out.push(1);
                        write_str(&mut out, expr);
                    }
                }
                // v7.9.29 — is_unique appendix (FILE_VERSION 16+).
                // Single byte 0/1. v15-and-below readers stop before
                // this byte; v16 readers always consume it. mailrs K1.
                out.push(u8::from(idx.is_unique));
                // v7.9.29 — extra_column_positions appendix.
                // Layout: [u16 count][count × u16 column_position].
                write_u16(
                    &mut out,
                    u16::try_from(idx.extra_column_positions.len())
                        .expect("≤ 65k extra cols / index"),
                );
                for cp in &idx.extra_column_positions {
                    write_u16(&mut out, u16::try_from(*cp).expect("≤ 65k columns/table"));
                }
                // v7.39 (read01 round 52) — nulls_not_distinct (FILE_VERSION
                // 62+). Appended at the end of the per-index block so the v16
                // layout above is untouched; v61-and-below readers stop before
                // this byte and default the flag to false (NULLS DISTINCT).
                out.push(u8::from(idx.nulls_not_distinct));
            }
            // v6.7.2 — per-table hot_tier_bytes Option<u64>.
            // Layout: [u8 has_value][u64 LE value (if has_value)].
            // v10 readers stop before this byte (deserialise loop
            // gated on version >= 11); v11+ readers always
            // consume it.
            match t.schema.hot_tier_bytes {
                None => out.push(0),
                Some(n) => {
                    out.push(1);
                    out.extend_from_slice(&n.to_le_bytes());
                }
            }
            // v7.6.1 — FOREIGN KEY appendix (catalog FILE_VERSION 13+).
            // Layout: [u16 LE fk_count]
            //   per fk:
            //     [u8 has_name] [str name (if has_name)]
            //     [u16 LE local_arity] [u16 LE local_pos]*arity
            //     [str parent_table]
            //     [u16 LE parent_arity] [u16 LE parent_pos]*arity
            //     [u8 on_delete_tag] [u8 on_update_tag]
            // Older catalogs (v12 and below) skip this block entirely;
            // their reader stops before this byte.
            write_u16(
                &mut out,
                u16::try_from(t.schema.foreign_keys.len()).expect("≤ 65k FKs/table"),
            );
            for fk in &t.schema.foreign_keys {
                match &fk.name {
                    None => out.push(0),
                    Some(n) => {
                        out.push(1);
                        write_str(&mut out, n);
                    }
                }
                write_u16(
                    &mut out,
                    u16::try_from(fk.local_columns.len()).expect("≤ 65k FK columns"),
                );
                for &p in &fk.local_columns {
                    write_u16(&mut out, u16::try_from(p).expect("≤ 65k columns/table"));
                }
                write_str(&mut out, &fk.parent_table);
                write_u16(
                    &mut out,
                    u16::try_from(fk.parent_columns.len()).expect("≤ 65k FK parent columns"),
                );
                for &p in &fk.parent_columns {
                    write_u16(&mut out, u16::try_from(p).expect("≤ 65k columns/table"));
                }
                out.push(fk.on_delete.tag());
                out.push(fk.on_update.tag());
                // v7.38 (read01, T29) — MATCH type tag (FILE_VERSION 55+).
                out.push(fk.match_type.tag());
            }
            // v7.9.19 — UniquenessConstraint appendix (catalog
            // FILE_VERSION 15+). Layout per table after the FK
            // block:
            //   [u16 count]
            //     per constraint:
            //       [u8 is_primary_key]
            //       [u16 arity][u16 col_pos]*arity
            // Older catalogs (v14 and below) skip this block.
            write_u16(
                &mut out,
                u16::try_from(t.schema.uniqueness_constraints.len())
                    .expect("≤ 65k uniqueness constraints/table"),
            );
            for uc in &t.schema.uniqueness_constraints {
                out.push(u8::from(uc.is_primary_key));
                write_u16(
                    &mut out,
                    u16::try_from(uc.columns.len()).expect("≤ 65k cols in uniqueness constraint"),
                );
                for &p in &uc.columns {
                    write_u16(&mut out, u16::try_from(p).expect("≤ 65k columns/table"));
                }
                // v7.13.0 — `nulls_not_distinct` flag
                // (FILE_VERSION 23+). Always written by writers at
                // version 23+; deserialise gates on `version >= 23`
                // so v22-and-below catalogs round-trip cleanly.
                out.push(u8::from(uc.nulls_not_distinct));
            }
            // v7.9.21 — runtime_default appendix per table.
            // Layout: [u16 count] then for each:
            //   [u16 col_pos][str expr]
            // Only columns whose runtime_default is Some land here;
            // catalog stays compact for the common literal-default
            // case.
            let mut rt_defaults: Vec<(usize, &str)> = Vec::new();
            for (i, c) in t.schema.columns.iter().enumerate() {
                if let Some(e) = &c.runtime_default {
                    rt_defaults.push((i, e.as_str()));
                }
            }
            write_u16(
                &mut out,
                u16::try_from(rt_defaults.len()).expect("≤ 65k runtime defaults/table"),
            );
            for (pos, expr) in rt_defaults {
                write_u16(&mut out, u16::try_from(pos).expect("≤ 65k columns/table"));
                write_str(&mut out, expr);
            }
            // v7.13.0 — CHECK constraint appendix per table.
            // Layout: [u16 count] then `count` Display-form
            // expression strings. Re-parsed on every INSERT/UPDATE
            // by the engine. FILE_VERSION 23+ only; v22 readers
            // never reach this block because the writer also moves
            // to v23 in lock-step.
            write_u16(
                &mut out,
                u16::try_from(t.schema.checks.len()).expect("≤ 65k CHECK constraints/table"),
            );
            for c in &t.schema.checks {
                // v7.39 (read01 round 48) — the expr stays in this v23
                // appendix (byte layout unchanged for old readers); the
                // name rides the v60 constraint-name appendix at the tail.
                write_str(&mut out, c.expr.as_str());
            }
            // v7.17.0 Phase 1.4 — per-table user_enum_type
            // appendix. Layout: [u16 count] then
            // [u16 col_pos][str enum_name] per binding. Only
            // columns whose user_enum_type is Some land here.
            let mut enum_bindings: Vec<(usize, &str)> = Vec::new();
            for (i, c) in t.schema.columns.iter().enumerate() {
                if let Some(e) = &c.user_enum_type {
                    enum_bindings.push((i, e.as_str()));
                }
            }
            write_u16(
                &mut out,
                u16::try_from(enum_bindings.len()).expect("≤ 65k enum-typed columns/table"),
            );
            for (pos, ename) in enum_bindings {
                write_u16(&mut out, u16::try_from(pos).expect("≤ 65k columns/table"));
                write_str(&mut out, ename);
            }
            // v7.17.0 Phase 1.5 — per-table user_domain_type
            // appendix. Same layout as the enum one. v29-and-
            // below readers stop after the enum appendix.
            let mut domain_bindings: Vec<(usize, &str)> = Vec::new();
            for (i, c) in t.schema.columns.iter().enumerate() {
                if let Some(d) = &c.user_domain_type {
                    domain_bindings.push((i, d.as_str()));
                }
            }
            write_u16(
                &mut out,
                u16::try_from(domain_bindings.len()).expect("≤ 65k domain-typed columns/table"),
            );
            for (pos, dname) in domain_bindings {
                write_u16(&mut out, u16::try_from(pos).expect("≤ 65k columns/table"));
                write_str(&mut out, dname);
            }
            // v7.17.0 Phase 2.1 — per-table on_update_runtime
            // appendix. Sparse: only ON UPDATE-bound columns.
            let mut on_update_bindings: Vec<(usize, &str)> = Vec::new();
            for (i, c) in t.schema.columns.iter().enumerate() {
                if let Some(e) = &c.on_update_runtime {
                    on_update_bindings.push((i, e.as_str()));
                }
            }
            write_u16(
                &mut out,
                u16::try_from(on_update_bindings.len()).expect("≤ 65k ON UPDATE columns/table"),
            );
            for (pos, expr_src) in on_update_bindings {
                write_u16(&mut out, u16::try_from(pos).expect("≤ 65k columns/table"));
                write_str(&mut out, expr_src);
            }
            // v7.17.0 Phase 2.5 — per-table collation appendix.
            // Sparse: only non-Binary columns land. Layout:
            // `[u16 count][u16 col_pos][u8 tag] × count`.
            let mut coll_bindings: Vec<(usize, u8)> = Vec::new();
            for (i, c) in t.schema.columns.iter().enumerate() {
                let tag = match c.collation {
                    Collation::Binary => continue,
                    Collation::CaseInsensitive => Collation::TAG_CASE_INSENSITIVE,
                };
                coll_bindings.push((i, tag));
            }
            write_u16(
                &mut out,
                u16::try_from(coll_bindings.len()).expect("≤ 65k collation bindings/table"),
            );
            for (pos, tag) in coll_bindings {
                write_u16(&mut out, u16::try_from(pos).expect("≤ 65k columns/table"));
                out.push(tag);
            }
            // v7.17.0 Phase 4.4 — per-table is_unsigned appendix.
            // Sparse: only UNSIGNED columns land. Layout:
            // `[u16 count][u16 col_pos] × count`.
            let mut unsigned_bindings: Vec<usize> = Vec::new();
            for (i, c) in t.schema.columns.iter().enumerate() {
                if c.is_unsigned {
                    unsigned_bindings.push(i);
                }
            }
            write_u16(
                &mut out,
                u16::try_from(unsigned_bindings.len()).expect("≤ 65k UNSIGNED columns/table"),
            );
            for pos in unsigned_bindings {
                write_u16(&mut out, u16::try_from(pos).expect("≤ 65k columns/table"));
            }
            // v7.17.0 Phase 3.P0-36 — per-table inline_enum_variants
            // appendix. Sparse: only ENUM columns land. Layout:
            // `[u16 count] then per binding [u16 col_pos]
            // [u16 variant_count] then variant strings`.
            // FILE_VERSION 41+; v40 readers never reach this block.
            let mut enum_inline_bindings: Vec<(usize, &[String])> = Vec::new();
            for (i, c) in t.schema.columns.iter().enumerate() {
                if let Some(vs) = &c.inline_enum_variants {
                    enum_inline_bindings.push((i, vs.as_slice()));
                }
            }
            write_u16(
                &mut out,
                u16::try_from(enum_inline_bindings.len()).expect("≤ 65k inline-ENUM columns/table"),
            );
            for (pos, variants) in enum_inline_bindings {
                write_u16(&mut out, u16::try_from(pos).expect("≤ 65k columns/table"));
                write_u16(
                    &mut out,
                    u16::try_from(variants.len()).expect("≤ 65k variants/ENUM"),
                );
                for v in variants {
                    write_str(&mut out, v.as_str());
                }
            }
            // v7.17.0 Phase 3.P0-37 — per-table inline_set_variants
            // appendix. Same layout as the inline ENUM block.
            // FILE_VERSION 42+; v41 readers never reach this block.
            let mut set_inline_bindings: Vec<(usize, &[String])> = Vec::new();
            for (i, c) in t.schema.columns.iter().enumerate() {
                if let Some(vs) = &c.inline_set_variants {
                    set_inline_bindings.push((i, vs.as_slice()));
                }
            }
            write_u16(
                &mut out,
                u16::try_from(set_inline_bindings.len()).expect("≤ 65k inline-SET columns/table"),
            );
            for (pos, variants) in set_inline_bindings {
                write_u16(&mut out, u16::try_from(pos).expect("≤ 65k columns/table"));
                write_u16(
                    &mut out,
                    u16::try_from(variants.len()).expect("≤ 65k variants/SET"),
                );
                for v in variants {
                    write_str(&mut out, v.as_str());
                }
            }
            // v7.37.6-B — partition role appendix(FILE_VERSION 49+)。
            // Layout 详见 FILE_VERSION 49 docstring。普通表 = 单字节 0。
            write_partition_role(&mut out, t.schema.partition_role.as_ref());
            // v7.37.7 — per-table generated_stored_expr appendix
            // (FILE_VERSION 50+). Sparse: only columns whose
            // generated_stored_expr is Some land here.
            let mut gen_bindings: Vec<(usize, &str)> = Vec::new();
            for (i, c) in t.schema.columns.iter().enumerate() {
                if let Some(src) = &c.generated_stored_expr {
                    gen_bindings.push((i, src.as_str()));
                }
            }
            write_u16(
                &mut out,
                u16::try_from(gen_bindings.len()).expect("≤ 65k GENERATED STORED columns/table"),
            );
            for (pos, src) in gen_bindings {
                write_u16(&mut out, u16::try_from(pos).expect("≤ 65k columns/table"));
                write_str(&mut out, src);
            }
            // v7.38 (read01) — per-table default_text appendix
            // (FILE_VERSION 58+). Sparse: only columns whose default_text
            // is Some land here. Mirrors the generated_stored_expr shape.
            let mut default_texts: Vec<(usize, &str)> = Vec::new();
            for (i, c) in t.schema.columns.iter().enumerate() {
                if let Some(src) = &c.default_text {
                    default_texts.push((i, src.as_str()));
                }
            }
            write_u16(
                &mut out,
                u16::try_from(default_texts.len()).expect("≤ 65k defaulted columns/table"),
            );
            for (pos, src) in default_texts {
                write_u16(&mut out, u16::try_from(pos).expect("≤ 65k columns/table"));
                write_str(&mut out, src);
            }
            // v7.39 (RLS) — per-table policy appendix + the two RLS flags
            // (FILE_VERSION 59+). Written after the default_text block and
            // before the MVCC row appendix, so a v58 reader stops before it.
            // Layout: [u8 row_security][u8 force] [u16 policy_count] then per
            // policy: [str name][u8 cmd][u8 permissive][u16 role_count]
            // (role_count × str) [u8 has_using](+str)[u8 has_check](+str).
            out.push(u8::from(t.schema.row_security));
            out.push(u8::from(t.schema.force_row_security));
            write_u16(
                &mut out,
                u16::try_from(t.schema.policies.len()).expect("≤ 65k policies/table"),
            );
            for p in &t.schema.policies {
                write_str(&mut out, &p.name);
                out.push(p.cmd.to_wire_byte());
                out.push(u8::from(p.permissive));
                write_u16(
                    &mut out,
                    u16::try_from(p.roles.len()).expect("≤ 65k roles/policy"),
                );
                for r in &p.roles {
                    write_str(&mut out, r);
                }
                match &p.using_expr {
                    Some(s) => {
                        out.push(1);
                        write_str(&mut out, s);
                    }
                    None => out.push(0),
                }
                match &p.with_check_expr {
                    Some(s) => {
                        out.push(1);
                        write_str(&mut out, s);
                    }
                    None => out.push(0),
                }
            }
            // v7.37.16 (Epic W) — per-row MVCC header + stable RowId
            // appendix (FILE_VERSION 53+). Persists xmin/xmax/flags +
            // RowId for every row so a tombstone naming a pre-checkpoint
            // row survives a serialize→deserialize base restore
            // (cross-checkpoint tombstone durability). `headers` /
            // `rowids` are lock-step parallel to `rows` (invariant held
            // at every mutation boundary), so the count is `rows.len()`
            // and the zipped walk visits them in physical row order —
            // the same order the rows block above was written in. v52
            // readers never reach this block (the writer also moves to
            // v53 in lock-step); a v53 reader restores headers + ids
            // verbatim instead of freezing + dense-assigning.
            debug_assert_eq!(
                t.rows.len(),
                t.headers.len(),
                "headers must be lock-step with rows at serialize"
            );
            debug_assert_eq!(
                t.rows.len(),
                t.rowids.len(),
                "rowids must be lock-step with rows at serialize"
            );
            write_u32(
                &mut out,
                u32::try_from(t.rows.len()).expect("≤ 4G rows/table"),
            );
            for (h, rid) in t.headers.iter().zip(t.rowids.iter()) {
                out.extend_from_slice(&h.xmin.to_le_bytes());
                out.extend_from_slice(&h.xmax.to_le_bytes());
                out.push(h.flags);
                out.extend_from_slice(&rid.0.to_le_bytes());
            }
            out.extend_from_slice(&t.next_rowid.to_le_bytes());
            // v7.39 (read01 round 48) — constraint-name appendix
            // (FILE_VERSION 60+). Index-aligned to the CHECK and
            // uniqueness-constraint appendices written above, so the
            // existing byte layouts stay untouched and a v59 catalog still
            // decodes (its constraints just come back unnamed).
            // Layout: [u16 check_count] then per check
            //         [u8 has_name] ([str name] when has_name)
            //         [u16 uc_count] then per uc the same pair.
            write_u16(
                &mut out,
                u16::try_from(t.schema.checks.len()).expect("≤ 65k CHECK constraints/table"),
            );
            for c in &t.schema.checks {
                match &c.name {
                    Some(n) => {
                        out.push(1);
                        write_str(&mut out, n);
                    }
                    None => out.push(0),
                }
            }
            write_u16(
                &mut out,
                u16::try_from(t.schema.uniqueness_constraints.len())
                    .expect("≤ 65k uniqueness constraints/table"),
            );
            for uc in &t.schema.uniqueness_constraints {
                match &uc.name {
                    Some(n) => {
                        out.push(1);
                        write_str(&mut out, n);
                    }
                    None => out.push(0),
                }
            }
            // v7.39 (read01 round 56) — user_composite_type appendix
            // (FILE_VERSION 63+). Sparse, at the very end of the per-table
            // block: only composite-typed columns land here, so a v62 reader
            // stops before it and its composite columns stay plain JSON.
            let mut comp_bindings: Vec<(usize, &str)> = Vec::new();
            for (i, c) in t.schema.columns.iter().enumerate() {
                if let Some(n) = &c.user_composite_type {
                    comp_bindings.push((i, n.as_str()));
                }
            }
            write_u16(
                &mut out,
                u16::try_from(comp_bindings.len())
                    .expect("≤ 65k composite-typed columns/table"),
            );
            for (pos, n) in comp_bindings {
                write_u16(&mut out, u16::try_from(pos).expect("≤ 65k columns/table"));
                write_str(&mut out, n);
            }
            // v7.39 (read01 round 57) — owner + ACL appendix (FILE_VERSION
            // 64+), at the very end of the per-table block so a v63 reader
            // stops before it (its tables then read back owner-less, i.e.
            // owned by the login role, with no grants — which is exactly what
            // they were).
            match &t.schema.owner {
                Some(o) => {
                    out.push(1);
                    write_str(&mut out, o);
                }
                None => out.push(0),
            }
            write_u16(
                &mut out,
                u16::try_from(t.schema.acl.len()).expect("≤ 65k aclitems/table"),
            );
            for a in &t.schema.acl {
                write_str(&mut out, &a.grantee);
                write_u16(&mut out, a.privs);
                write_u16(&mut out, a.grantable);
                write_str(&mut out, &a.grantor);
            }
            // v7.39 (read01 round 59) — COLUMN acl appendix (FILE_VERSION 65+),
            // sparse: only columns that carry a grant land here, so a v64 reader
            // stops before it and its columns read back un-granted, which is
            // what they were.
            let granted: Vec<(usize, &ColumnSchema)> = t
                .schema
                .columns
                .iter()
                .enumerate()
                .filter(|(_, c)| !c.acl.is_empty())
                .collect();
            write_u16(
                &mut out,
                u16::try_from(granted.len()).expect("≤ 65k granted columns/table"),
            );
            for (pos, c) in granted {
                write_u16(&mut out, u16::try_from(pos).expect("≤ 65k columns/table"));
                write_u16(
                    &mut out,
                    u16::try_from(c.acl.len()).expect("≤ 65k aclitems/column"),
                );
                for a in &c.acl {
                    write_str(&mut out, &a.grantee);
                    write_u16(&mut out, a.privs);
                    write_u16(&mut out, a.grantable);
                    write_str(&mut out, &a.grantor);
                }
            }
        }
        // v7.12.4 — catalog-wide appendix: user-defined functions
        // then triggers. FILE_VERSION 22+ only. v21 and earlier
        // readers stop after the last table; v22 readers always
        // consume two `u32` counts (possibly zero).
        //
        // Function entry layout:
        //   [str name] [str args_repr] [str returns]
        //   [str language] [str body]
        // Trigger entry layout:
        //   [str name] [str table] [str timing]
        //   [u16 event_count] (event_count × str)
        //   [str for_each] [str function]
        write_u32(
            &mut out,
            u32::try_from(self.functions.len()).expect("≤ 4G functions"),
        );
        for fd in self.functions.values() {
            write_str(&mut out, &fd.name);
            write_str(&mut out, &fd.args_repr);
            write_str(&mut out, &fd.returns);
            write_str(&mut out, &fd.language);
            write_str_long(&mut out, &fd.body);
        }
        write_u32(
            &mut out,
            u32::try_from(self.triggers.len()).expect("≤ 4G triggers"),
        );
        for td in &self.triggers {
            write_str(&mut out, &td.name);
            write_str(&mut out, &td.table);
            write_str(&mut out, &td.timing);
            write_u16(
                &mut out,
                u16::try_from(td.events.len()).expect("≤ 65k events / trigger"),
            );
            for ev in &td.events {
                write_str(&mut out, ev);
            }
            write_str(&mut out, &td.for_each);
            write_str(&mut out, &td.function);
            // v7.13.0 — `UPDATE OF cols` filter
            // (FILE_VERSION 23+). v22 readers omit; v23 writers
            // always emit (possibly zero).
            write_u16(
                &mut out,
                u16::try_from(td.update_columns.len()).expect("≤ 65k cols / trigger"),
            );
            for c in &td.update_columns {
                write_str(&mut out, c);
            }
            // v7.16.1 — TriggerDef.enabled (FILE_VERSION 25+).
            out.push(u8::from(td.enabled));
        }
        // v7.17.0 Phase 1.1 — SEQUENCE catalog block (FILE_VERSION 26+).
        write_u32(
            &mut out,
            u32::try_from(self.sequences.len()).expect("≤ 4G sequences"),
        );
        for seq in self.sequences.values() {
            write_str(&mut out, &seq.name);
            out.push(match seq.data_type {
                SequenceDataType::SmallInt => 0,
                SequenceDataType::Int => 1,
                SequenceDataType::BigInt => 2,
            });
            out.extend_from_slice(&seq.start.to_le_bytes());
            out.extend_from_slice(&seq.increment.to_le_bytes());
            out.extend_from_slice(&seq.min_value.to_le_bytes());
            out.extend_from_slice(&seq.max_value.to_le_bytes());
            out.extend_from_slice(&seq.cache.to_le_bytes());
            out.push(u8::from(seq.cycle));
            match &seq.owned_by {
                None => out.push(0),
                Some((table, column)) => {
                    out.push(1);
                    write_str(&mut out, table);
                    write_str(&mut out, column);
                }
            }
            out.extend_from_slice(&seq.last_value.to_le_bytes());
            out.push(u8::from(seq.is_called));
        }
        // v7.17.0 Phase 1.2 — VIEW catalog block (FILE_VERSION 27+).
        write_u32(
            &mut out,
            u32::try_from(self.views.len()).expect("≤ 4G views"),
        );
        for view in self.views.values() {
            write_str(&mut out, &view.name);
            write_u16(
                &mut out,
                u16::try_from(view.columns.len()).expect("≤ 65k cols / view"),
            );
            for c in &view.columns {
                write_str(&mut out, c);
            }
            write_str_long(&mut out, &view.body);
        }
        // v7.17.0 Phase 1.3 — MATERIALIZED VIEW source registry
        // (FILE_VERSION 28+). The backing rows live as a regular
        // table of the same name already in the tables block.
        write_u32(
            &mut out,
            u32::try_from(self.materialized_views.len()).expect("≤ 4G materialized views"),
        );
        for (name, body) in &self.materialized_views {
            write_str(&mut out, name);
            write_str_long(&mut out, body);
        }
        // v7.17.0 Phase 1.4 — ENUM types catalog block
        // (FILE_VERSION 29+).
        write_u32(
            &mut out,
            u32::try_from(self.enum_types.len()).expect("≤ 4G enum types"),
        );
        for e in self.enum_types.values() {
            write_str(&mut out, &e.name);
            write_u16(
                &mut out,
                u16::try_from(e.labels.len()).expect("≤ 65k labels / enum"),
            );
            for l in &e.labels {
                write_str(&mut out, l);
            }
        }
        // v7.17.0 Phase 1.5 — DOMAIN types catalog block
        // (FILE_VERSION 30+).
        write_u32(
            &mut out,
            u32::try_from(self.domain_types.len()).expect("≤ 4G domain types"),
        );
        for d in self.domain_types.values() {
            write_str(&mut out, &d.name);
            write_data_type(&mut out, d.base_type);
            out.push(u8::from(d.nullable));
            match &d.default {
                None => out.push(0),
                Some(s) => {
                    out.push(1);
                    write_str(&mut out, s);
                }
            }
            write_u16(
                &mut out,
                u16::try_from(d.checks.len()).expect("≤ 65k CHECKs / domain"),
            );
            for c in &d.checks {
                write_str(&mut out, c);
            }
        }
        // v7.17.0 Phase 1.6 — user-schemas registry
        // (FILE_VERSION 31+). Built-ins are hardcoded in
        // `is_builtin_schema` and not persisted.
        write_u32(
            &mut out,
            u32::try_from(self.schemas.len()).expect("≤ 4G schemas"),
        );
        for name in &self.schemas {
            write_str(&mut out, name);
        }
        // v7.37.42-T2 ζ-B — COMPOSITE types catalog block
        // (FILE_VERSION 52+). Each entry: name, u16 field_count,
        // then field_count `[str field_name][data_type]` pairs.
        write_u32(
            &mut out,
            u32::try_from(self.composite_types.len()).expect("≤ 4G composite types"),
        );
        for c in self.composite_types.values() {
            write_str(&mut out, &c.name);
            write_u16(
                &mut out,
                u16::try_from(c.fields.len()).expect("≤ 65k fields / composite"),
            );
            for (fname, fty) in &c.fields {
                write_str(&mut out, fname);
                write_data_type(&mut out, *fty);
            }
        }
        // v7.39 (read01 round 50) — COMMENT store (FILE_VERSION 61+).
        // Catalog-wide, written last (before the CRC trailer) so every older
        // reader stops before it. Layout: [u32 count] then [str key][str text].
        write_u32(
            &mut out,
            u32::try_from(self.comments.len()).expect("≤ 4G comments"),
        );
        for (k, v) in &self.comments {
            write_str(&mut out, k);
            write_str_long(&mut out, v);
        }
        // v7.39 (read01 round 60) — non-table ACLs (FILE_VERSION 66+), catalog-
        // wide and written last so a v65 reader stops before them. The sequence
        // block itself sits mid-image and cannot grow without breaking older
        // readers, so a sequence's owner + ACL rides here, keyed by name.
        let acl_out = |out: &mut Vec<u8>, acl: &[AclItem]| {
            write_u16(out, u16::try_from(acl.len()).expect("≤ 65k aclitems"));
            for a in acl {
                write_str(out, &a.grantee);
                write_u16(out, a.privs);
                write_u16(out, a.grantable);
                write_str(out, &a.grantor);
            }
        };
        let owned: Vec<&SequenceDef> = self
            .sequences
            .values()
            .filter(|s| s.owner.is_some() || !s.acl.is_empty())
            .collect();
        write_u32(
            &mut out,
            u32::try_from(owned.len()).expect("≤ 4G sequences"),
        );
        for seq in owned {
            write_str(&mut out, &seq.name);
            match &seq.owner {
                Some(o) => {
                    out.push(1);
                    write_str(&mut out, o);
                }
                None => out.push(0),
            }
            acl_out(&mut out, &seq.acl);
        }
        acl_out(&mut out, &self.schema_acl);
        acl_out(&mut out, &self.database_acl);
        // v7.39 (read01 round 61) — FUNCTION owner + ACL (FILE_VERSION 67+).
        // The function block sits mid-image like the sequence one, so this
        // rides the catalog-wide tail too, keyed by name.
        let fns: Vec<&FunctionDef> = self
            .functions
            .values()
            .filter(|f| f.owner.is_some() || !f.acl.is_empty())
            .collect();
        write_u32(&mut out, u32::try_from(fns.len()).expect("≤ 4G functions"));
        for f in fns {
            // v7.39 (read01 round 62) — keyed by SIGNATURE now: two overloads
            // have two ACLs.
            write_str(&mut out, &function_signature_key(&f.name, &f.args_repr));
            match &f.owner {
                Some(o) => {
                    out.push(1);
                    write_str(&mut out, o);
                }
                None => out.push(0),
            }
            acl_out(&mut out, &f.acl);
        }
        // v7.38 (read01 P5.05) — CRC32C trailer over the whole image so a
        // corrupted snapshot is rejected on load. FILE_VERSION is >= the
        // trailer version, so this always runs for freshly-written images.
        let crc = spg_crypto::crc32c::crc32c(&out);
        write_u32(&mut out, crc);
        out
    }

    /// Deserialize a previously-serialized catalog. Rejects bad magic, version
    /// mismatch, unknown tags, truncation, and trailing bytes.
    pub fn deserialize(buf: &[u8]) -> Result<Self, StorageError> {
        let mut cur = Cursor::new(buf);
        let magic = cur.take(8)?;
        if magic != FILE_MAGIC {
            return Err(StorageError::Corrupt(format!(
                "bad magic: expected SPGDB001, got {magic:?}"
            )));
        }
        let version = cur.read_u8()?;
        if !(MIN_SUPPORTED_FILE_VERSION..=FILE_VERSION).contains(&version) {
            return Err(StorageError::Corrupt(format!(
                "unsupported file version: {version} (supported: {MIN_SUPPORTED_FILE_VERSION}..={FILE_VERSION})"
            )));
        }
        // v7.23/v7.27 — escape decoding is version-gated (see
        // STR_LEN_ESCAPE / Cursor::codec_version).
        cur.codec_version = version;
        let table_count = cur.read_u32()? as usize;
        let mut cat = Self::new();
        for _ in 0..table_count {
            deserialize_table(&mut cur, &mut cat, version)?;
        }
        // v7.37.15 (Phase C.1) — stamp dense stable RelIds on load.
        // Pre-V6 envelopes carry no ids; a dense 1..=N assignment is
        // sufficient while RelId is process-local bookkeeping (the V6
        // envelope, Phase C.6, will round-trip real ids). Sets the
        // allocator above the loaded ids so a post-load CREATE TABLE
        // never collides.
        for (i, t) in cat.tables.iter_mut().enumerate() {
            t.set_rel_id(row_header::RelId((i as u64) + 1));
        }
        cat.next_rel_id = cat.tables.len() as u64;
        // v7.12.4 — catalog-wide function + trigger appendix.
        // FILE_VERSION 22+ only; v21 and earlier catalogs stop
        // after the last table.
        if version >= 22 {
            let fn_count = cur.read_u32()? as usize;
            for _ in 0..fn_count {
                let name = cur.read_str()?;
                let args_repr = cur.read_str()?;
                let returns = cur.read_str()?;
                let language = cur.read_str()?;
                let body = cur.read_str_long()?;
                let key = function_signature_key(&name, &args_repr);
                cat.functions.insert(
                    key,
                    FunctionDef {
                        name,
                        args_repr,
                        returns,
                        language,
                        body,
                        owner: None,
                        acl: Vec::new(),
                    },
                );
            }
            let trg_count = cur.read_u32()? as usize;
            for _ in 0..trg_count {
                let name = cur.read_str()?;
                let table = cur.read_str()?;
                let timing = cur.read_str()?;
                let ev_count = cur.read_u16()? as usize;
                let mut events = Vec::with_capacity(ev_count);
                for _ in 0..ev_count {
                    events.push(cur.read_str()?);
                }
                let for_each = cur.read_str()?;
                let function = cur.read_str()?;
                // v7.13.0 — trailing `UPDATE OF cols` filter
                // (FILE_VERSION 23+ only; v22 catalogs omit and
                // deserialise with an empty vec).
                let update_columns = if version >= 23 {
                    let n = cur.read_u16()? as usize;
                    let mut cols = Vec::with_capacity(n);
                    for _ in 0..n {
                        cols.push(cur.read_str()?);
                    }
                    cols
                } else {
                    Vec::new()
                };
                // v7.16.1 — TriggerDef.enabled (FILE_VERSION 25+).
                // v24-and-below catalogs deserialise with `true`
                // — pre-v7.16.1 every trigger always fired.
                let enabled = if version >= 25 {
                    cur.read_u8()? != 0
                } else {
                    true
                };
                cat.triggers.push(TriggerDef {
                    name,
                    table,
                    timing,
                    events,
                    for_each,
                    function,
                    update_columns,
                    enabled,
                });
            }
        }
        // v7.17.0 Phase 1.1 — SEQUENCE block (FILE_VERSION 26+).
        // v25-and-below catalogs omit; we leave the map empty.
        if version >= 26 {
            let seq_count = cur.read_u32()? as usize;
            for _ in 0..seq_count {
                let name = cur.read_str()?;
                let data_type = match cur.read_u8()? {
                    0 => SequenceDataType::SmallInt,
                    1 => SequenceDataType::Int,
                    2 => SequenceDataType::BigInt,
                    other => {
                        return Err(StorageError::Corrupt(format!(
                            "unknown SEQUENCE data-type tag {other}"
                        )));
                    }
                };
                let start = cur.read_i64()?;
                let increment = cur.read_i64()?;
                let min_value = cur.read_i64()?;
                let max_value = cur.read_i64()?;
                let cache = cur.read_i64()?;
                let cycle = cur.read_u8()? != 0;
                let owned_by = match cur.read_u8()? {
                    0 => None,
                    1 => {
                        let t = cur.read_str()?;
                        let c = cur.read_str()?;
                        Some((t, c))
                    }
                    other => {
                        return Err(StorageError::Corrupt(format!(
                            "unknown SEQUENCE owned-by tag {other}"
                        )));
                    }
                };
                let last_value = cur.read_i64()?;
                let is_called = cur.read_u8()? != 0;
                cat.sequences.insert(
                    name.clone(),
                    SequenceDef {
                        name,
                        data_type,
                        start,
                        increment,
                        min_value,
                        max_value,
                        cache,
                        cycle,
                        owned_by,
                        last_value,
                        is_called,
                        owner: None,
                        acl: Vec::new(),
                    },
                );
            }
        }
        // v7.17.0 Phase 1.2 — VIEW block (FILE_VERSION 27+).
        // v26-and-below catalogs omit; we leave the map empty.
        if version >= 27 {
            let view_count = cur.read_u32()? as usize;
            for _ in 0..view_count {
                let name = cur.read_str()?;
                let col_count = cur.read_u16()? as usize;
                let mut columns = Vec::with_capacity(col_count);
                for _ in 0..col_count {
                    columns.push(cur.read_str()?);
                }
                let body = cur.read_str_long()?;
                cat.views.insert(
                    name.clone(),
                    ViewDef {
                        name,
                        columns,
                        body,
                    },
                );
            }
        }
        // v7.17.0 Phase 1.3 — MATERIALIZED VIEW source registry
        // (FILE_VERSION 28+). v27-and-below catalogs omit.
        if version >= 28 {
            let mv_count = cur.read_u32()? as usize;
            for _ in 0..mv_count {
                let name = cur.read_str()?;
                let body = cur.read_str_long()?;
                cat.materialized_views.insert(name, body);
            }
        }
        // v7.17.0 Phase 1.4 — ENUM types catalog block
        // (FILE_VERSION 29+).
        if version >= 29 {
            let etype_count = cur.read_u32()? as usize;
            for _ in 0..etype_count {
                let name = cur.read_str()?;
                let label_count = cur.read_u16()? as usize;
                let mut labels = Vec::with_capacity(label_count);
                for _ in 0..label_count {
                    labels.push(cur.read_str()?);
                }
                cat.enum_types
                    .insert(name.clone(), EnumDef { name, labels });
            }
        }
        // v7.17.0 Phase 1.5 — DOMAIN types catalog block
        // (FILE_VERSION 30+).
        if version >= 30 {
            let dtype_count = cur.read_u32()? as usize;
            for _ in 0..dtype_count {
                let name = cur.read_str()?;
                let base_type = cur.read_data_type()?;
                let nullable = cur.read_u8()? != 0;
                let default = match cur.read_u8()? {
                    0 => None,
                    1 => Some(cur.read_str()?),
                    other => {
                        return Err(StorageError::Corrupt(format!(
                            "unknown DOMAIN default tag {other}"
                        )));
                    }
                };
                let check_count = cur.read_u16()? as usize;
                let mut checks = Vec::with_capacity(check_count);
                for _ in 0..check_count {
                    checks.push(cur.read_str()?);
                }
                cat.domain_types.insert(
                    name.clone(),
                    DomainDef {
                        name,
                        base_type,
                        nullable,
                        default,
                        checks,
                    },
                );
            }
        }
        // v7.17.0 Phase 1.6 — user-schemas registry
        // (FILE_VERSION 31+).
        if version >= 31 {
            let sch_count = cur.read_u32()? as usize;
            for _ in 0..sch_count {
                let name = cur.read_str()?;
                cat.schemas.insert(name);
            }
        }
        // v7.37.42-T2 ζ-B — COMPOSITE types catalog block
        // (FILE_VERSION 52+). v51-and-below readers stop at the
        // user-schemas block; v52 readers fed a v51 catalog see no
        // composite block and default to an empty map.
        if version >= 52 {
            let ctype_count = cur.read_u32()? as usize;
            for _ in 0..ctype_count {
                let name = cur.read_str()?;
                let field_count = cur.read_u16()? as usize;
                let mut fields = Vec::with_capacity(field_count);
                for _ in 0..field_count {
                    let fname = cur.read_str()?;
                    let fty = cur.read_data_type()?;
                    fields.push((fname, fty));
                }
                cat.composite_types
                    .insert(name.clone(), CompositeDef { name, fields });
            }
        }
        // v7.39 (read01 round 50) — COMMENT store (FILE_VERSION 61+).
        if version >= 61 {
            let comment_count = cur.read_u32()? as usize;
            for _ in 0..comment_count {
                let key = cur.read_str()?;
                let text = cur.read_str_long()?;
                cat.comments.insert(key, text);
            }
        }
        // v7.39 (read01 round 60) — non-table ACLs (FILE_VERSION 66+).
        if version >= 66 {
            let read_acl = |cur: &mut Cursor| -> Result<Vec<AclItem>, StorageError> {
                let n = cur.read_u16()? as usize;
                let mut acl = Vec::with_capacity(n);
                for _ in 0..n {
                    let grantee = cur.read_str()?;
                    let privs = cur.read_u16()?;
                    let grantable = cur.read_u16()?;
                    let grantor = cur.read_str()?;
                    acl.push(AclItem {
                        grantee,
                        privs,
                        grantable,
                        grantor,
                    });
                }
                Ok(acl)
            };
            let seq_count = cur.read_u32()? as usize;
            for _ in 0..seq_count {
                let name = cur.read_str()?;
                let owner = if cur.read_u8()? == 1 {
                    Some(cur.read_str()?)
                } else {
                    None
                };
                let acl = read_acl(&mut cur)?;
                if let Some(seq) = cat.sequences.get_mut(&name) {
                    seq.owner = owner;
                    seq.acl = acl;
                }
            }
            cat.schema_acl = read_acl(&mut cur)?;
            cat.database_acl = read_acl(&mut cur)?;
            // v7.39 (read01 round 61) — FUNCTION owner + ACL (v67+; keyed by
            // signature from v68, when overloads became possible).
            if version >= 67 {
                let fn_count = cur.read_u32()? as usize;
                for _ in 0..fn_count {
                    let name = cur.read_str()?;
                    let owner = if cur.read_u8()? == 1 {
                        Some(cur.read_str()?)
                    } else {
                        None
                    };
                    let acl = read_acl(&mut cur)?;
                    if let Some(f) = cat.functions.get_mut(&name) {
                        f.owner = owner;
                        f.acl = acl;
                    }
                }
            }
        }
        // v7.38 (read01 P5.05) — v54+ images end with a CRC32C over every
        // preceding byte; verify it before accepting the snapshot. Older
        // images have no trailer and fall through to the trailing-byte check.
        if version >= FILE_VERSION_CRC_TRAILER {
            let crc_start = cur.pos;
            let stored = cur.read_u32()?;
            let computed = spg_crypto::crc32c::crc32c(&buf[..crc_start]);
            if computed != stored {
                return Err(StorageError::Corrupt(format!(
                    "base snapshot CRC mismatch: computed {computed:#010x}, stored {stored:#010x}"
                )));
            }
        }
        if cur.pos < buf.len() {
            return Err(StorageError::Corrupt(format!(
                "trailing bytes: {} unread",
                buf.len() - cur.pos
            )));
        }
        Ok(cat)
    }
}

#[cfg(test)]
mod tests;
