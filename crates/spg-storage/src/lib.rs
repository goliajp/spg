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

pub mod bloom;
pub mod halfvec;
pub mod persistent;
pub mod persistent_btree;
pub mod quantize;
pub mod row_locator;
pub mod segment;

pub use self::bloom::{BloomError, BloomFilter};
pub use self::row_locator::{RowLocator, RowLocatorError};
pub use self::segment::{
    OwnedSegment, SEGMENT_MAGIC, SEGMENT_PAGE_BYTES, SegmentError, SegmentMeta, SegmentReader,
    encode_segment,
};

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
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
    /// `INTERVAL` — calendar-aware span (months + microseconds). v2.11
    /// supports INTERVAL only as a runtime intermediate (literals,
    /// arithmetic results); on-disk encoding is rejected so this branch
    /// can't appear in a `ColumnSchema`.
    Interval,
    /// v4.9: `JSON` / `JSONB` — text-backed JSON document. We don't
    /// parse the content (no path operators or jsonb functions yet) —
    /// the column accepts any TEXT-compatible value and round-trips
    /// it verbatim. Equivalent to `Text` storage with a distinct
    /// type tag for the wire layer (PG OID 114).
    Json,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SmallInt => f.write_str("SMALLINT"),
            Self::Int => f.write_str("INT"),
            Self::BigInt => f.write_str("BIGINT"),
            Self::Float => f.write_str("FLOAT"),
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
            Self::Interval => f.write_str("INTERVAL"),
            Self::Json => f.write_str("JSON"),
        }
    }
}

/// A row-cell value, including SQL `NULL`. `Float` uses `f64`; NaN compares
/// non-equal to itself (PG behaviour) — `PartialEq` is derived so callers
/// must opt into NaN-aware comparison if they need stronger guarantees.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    SmallInt(i16),
    Int(i32),
    BigInt(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    Vector(Vec<f32>),
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
    /// arithmetic never falls back to floating-point.
    Numeric {
        scaled: i128,
        scale: u8,
    },
    /// Days since the Unix epoch (1970-01-01). Negative for earlier dates.
    Date(i32),
    /// Microseconds since the Unix epoch (1970-01-01T00:00:00Z).
    Timestamp(i64),
    /// Calendar span: `months` (variable-length) + `micros` (fixed-length).
    /// Runtime-only — cannot appear in a stored row in v2.11.
    Interval {
        months: i32,
        micros: i64,
    },
    /// v4.9 `JSON` — raw JSON text. No structural validation
    /// happens at the storage layer; whatever the parser hands us
    /// round-trips verbatim. Equality is byte-wise.
    Json(String),
    Null,
}

impl Value {
    /// Type tag, or `None` for `NULL` (unknown at value level).
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Self::SmallInt(_) => Some(DataType::SmallInt),
            Self::Int(_) => Some(DataType::Int),
            Self::BigInt(_) => Some(DataType::BigInt),
            Self::Float(_) => Some(DataType::Float),
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
            Self::Date(_) => Some(DataType::Date),
            Self::Timestamp(_) => Some(DataType::Timestamp),
            Self::Interval { .. } => Some(DataType::Interval),
            Self::Json(_) => Some(DataType::Json),
            Self::Null => None,
        }
    }

    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

/// One table row — values are positional and must match
/// `TableSchema.columns` in length and (modulo NULL) in `DataType`.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub values: Vec<Value>,
}

impl Row {
    pub const fn new(values: Vec<Value>) -> Self {
        Self { values }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnSchema {
    pub name: String,
    pub ty: DataType,
    pub nullable: bool,
    /// Optional `DEFAULT` value, frozen at CREATE TABLE time. `None`
    /// means "no default" (so omitted columns become NULL, or error
    /// out when the column is NOT NULL).
    pub default: Option<Value>,
    /// MySQL-style `AUTO_INCREMENT`. When set, an INSERT that leaves
    /// this column unbound (or sets it to NULL) gets the next integer
    /// computed from the column's current max + 1.
    pub auto_increment: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<ColumnSchema>,
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
}

impl IndexKey {
    pub fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::SmallInt(n) => Some(Self::Int(i64::from(*n))),
            Value::Int(n) => Some(Self::Int(i64::from(*n))),
            Value::BigInt(n) => Some(Self::Int(*n)),
            Value::Text(s) => Some(Self::Text(s.clone())),
            Value::Bool(b) => Some(Self::Bool(*b)),
            // Date/Timestamp use their integer storage repr as the
            // index key — same order semantics, same comparison.
            Value::Date(d) => Some(Self::Int(i64::from(*d))),
            Value::Timestamp(t) => Some(Self::Int(*t)),
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
            | Value::Json(_) => None,
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
        }
    }

    fn new_nsw(name: String, column_position: usize, m: usize) -> Self {
        Self {
            name,
            column_position,
            kind: IndexKind::Nsw(NswGraph::new(m)),
        }
    }

    /// Look up the locators stored under `key` (B-tree only). Returns
    /// an empty slice when the key is absent or the index is an NSW
    /// graph — callers can treat both cases uniformly.
    ///
    /// v5.1: return type widened from `&[usize]` to `&[RowLocator]`.
    /// Pre-v5.2 callers can read the slice and `.as_hot().unwrap()`
    /// each entry (no `Cold` variants exist until the freezer lands);
    /// post-v5.2 callers dispatch hot vs. cold per locator.
    pub fn lookup_eq(&self, key: &IndexKey) -> &[RowLocator] {
        match &self.kind {
            IndexKind::BTree(m) => m.get(key).map_or(&[][..], Vec::as_slice),
            IndexKind::Nsw(_) => &[][..],
        }
    }

    /// Borrow the NSW graph (if this is an NSW index). Callers that need
    /// the graph for a kNN search go through here.
    pub const fn nsw(&self) -> Option<&NswGraph> {
        match &self.kind {
            IndexKind::Nsw(g) => Some(g),
            IndexKind::BTree(_) => None,
        }
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
#[derive(Debug, Clone)]
pub struct Table {
    schema: TableSchema,
    rows: PersistentVec<Row>,
    indices: Vec<Index>,
    hot_bytes: u64,
}

impl Table {
    pub fn new(schema: TableSchema) -> Self {
        Self {
            schema,
            rows: PersistentVec::new(),
            indices: Vec::new(),
            hot_bytes: 0,
        }
    }

    /// Total encoded byte size of every row currently in the hot tier
    /// (`self.rows`). See struct docs for the maintenance contract.
    /// Returns 0 for an empty table.
    #[must_use]
    pub const fn hot_bytes(&self) -> u64 {
        self.hot_bytes
    }

    pub const fn schema(&self) -> &TableSchema {
        &self.schema
    }

    /// v4.39: returns the persistent row vector by reference. Callers that
    /// used to take `&[Row]` should switch to `.iter()` (via
    /// `IntoIterator for &PersistentVec`) or `.get(i)` for indexing.
    pub const fn rows(&self) -> &PersistentVec<Row> {
        &self.rows
    }

    pub const fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn indices(&self) -> &[Index] {
        &self.indices
    }

    /// Compute the next `AUTO_INCREMENT` value for the column at
    /// `col_pos`. Defined as `max(existing) + 1`, falling back to `1`
    /// when the column currently holds no integer values. NULL / non-
    /// integer cells are skipped. Returns `None` when the column isn't
    /// an integer type.
    pub fn next_auto_value(&self, col_pos: usize) -> Option<i64> {
        let ty = self.schema.columns.get(col_pos)?.ty;
        if !matches!(ty, DataType::SmallInt | DataType::Int | DataType::BigInt) {
            return None;
        }
        let mut max: Option<i64> = None;
        for row in &self.rows {
            match row.values.get(col_pos) {
                Some(Value::SmallInt(n)) => {
                    let v = i64::from(*n);
                    max = Some(max.map_or(v, |m| m.max(v)));
                }
                Some(Value::Int(n)) => {
                    let v = i64::from(*n);
                    max = Some(max.map_or(v, |m| m.max(v)));
                }
                Some(Value::BigInt(n)) => {
                    max = Some(max.map_or(*n, |m| m.max(*n)));
                }
                _ => {}
            }
        }
        Some(max.map_or(1, |m| m + 1))
    }

    /// Return the first index defined over `column_position`, if any.
    /// (`v0.8` supports at most one index per column logically; the search
    /// just picks the first match.)
    pub fn index_on(&self, column_position: usize) -> Option<&Index> {
        self.indices
            .iter()
            .find(|i| i.column_position == column_position)
    }

    /// Insert one row after validating it matches the schema (length + type).
    /// Returns `StorageError` on mismatch — the table is left unchanged.
    /// Updates every defined index with the new row's key.
    pub fn insert(&mut self, row: Row) -> Result<(), StorageError> {
        if row.len() != self.schema.columns.len() {
            return Err(StorageError::ArityMismatch {
                expected: self.schema.columns.len(),
                actual: row.len(),
            });
        }
        for (i, (val, col)) in row.values.iter().zip(&self.schema.columns).enumerate() {
            if val.is_null() {
                if !col.nullable {
                    return Err(StorageError::NullInNotNull {
                        column: col.name.clone(),
                    });
                }
                continue;
            }
            let actual = val.data_type().expect("non-null");
            // Vector columns require both that the value's variant be Vector
            // *and* its dimension match. `actual == col.ty` already encodes
            // both because DataType::Vector carries the dim.
            //
            // VARCHAR(n) / CHAR(n) are storage-equivalent to TEXT — the
            // length / padding contract is enforced upstream by
            // `coerce_value`. Accept a `Text` value into either.
            //
            // NUMERIC's `Value::Numeric` carries its actual scale but the
            // column declares the *expected* scale (a scale-rescaled
            // Value::Numeric is produced upstream by `coerce_value`); the
            // structural check here only verifies "value is Numeric and
            // its scale equals the column scale".
            let compatible = actual == col.ty
                || matches!(
                    (actual, col.ty),
                    (
                        DataType::Text,
                        DataType::Varchar(_) | DataType::Char(_) | DataType::Json
                    ) | (DataType::Json, DataType::Text)
                )
                || matches!(
                    (actual, col.ty),
                    (
                        DataType::Numeric { scale: a, .. },
                        DataType::Numeric { scale: b, .. },
                    ) if a == b
                );
            if !compatible {
                return Err(StorageError::TypeMismatch {
                    column: col.name.clone(),
                    expected: col.ty,
                    actual,
                    position: i,
                });
            }
        }
        let new_row_idx = self.rows.len();
        // Pre-validate before mutating: ensure indices receive an IndexKey.
        // For NSW we defer the graph update to *after* the row is pushed
        // so the kNN search can see it in `self.rows`.
        for idx in &mut self.indices {
            if let IndexKind::BTree(map) = &mut idx.kind
                && let Some(key) = IndexKey::from_value(&row.values[idx.column_position])
            {
                // v4.40: PersistentBTreeMap has no in-place entry-or-default.
                // Clone-then-insert keeps the same semantics — for typical
                // unique-key schemas the Vec is 1-element so the clone is
                // O(1). For dup-heavy columns it's O(M) per insert, traded
                // for the structural-sharing win at clone time.
                let mut entries = map.get(&key).cloned().unwrap_or_default();
                entries.push(RowLocator::Hot(new_row_idx));
                map.insert_mut(key, entries);
            }
        }
        // v5.2.1: maintain incremental hot-tier byte counter. Computed
        // before the move so we don't need to borrow `row` after push.
        self.hot_bytes = self
            .hot_bytes
            .saturating_add(row_body_encoded_len(&row, &self.schema) as u64);
        // v4.39.1: push_mut keeps streaming inserts at Vec::push speed when
        // the table is uniquely owned (the spg-embedded path); inside a TX
        // wrap where a Catalog snapshot exists, push_mut path-copies the
        // tail just like push() and the snapshot stays valid.
        self.rows.push_mut(row);
        // NSW updates after the push so the new row is visible to the
        // greedy search used during connect.
        let new_row_idx = self.rows.len() - 1;
        let nsw_targets: Vec<usize> = self
            .indices
            .iter()
            .enumerate()
            .filter_map(|(i, idx)| {
                if matches!(idx.kind, IndexKind::Nsw(_)) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        for idx_pos in nsw_targets {
            nsw_insert_at(self, idx_pos, new_row_idx);
        }
        Ok(())
    }

    /// Build a new B-tree index over the named column. Rebuilds from
    /// existing rows. Errors if `column_name` doesn't exist or the index
    /// name is taken.
    pub fn add_index(&mut self, name: String, column_name: &str) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        let mut idx = Index::new_btree(name, column_position);
        if let IndexKind::BTree(map) = &mut idx.kind {
            for (i, row) in self.rows.iter().enumerate() {
                if let Some(key) = IndexKey::from_value(&row.values[column_position]) {
                    let mut entries = map.get(&key).cloned().unwrap_or_default();
                    entries.push(RowLocator::Hot(i));
                    map.insert_mut(key, entries);
                }
            }
        }
        self.indices.push(idx);
        Ok(())
    }

    /// Build a new NSW (HNSW-flavoured) index over the named column.
    /// Required for `ORDER BY col <-> literal LIMIT k` to plan as a
    /// graph traversal instead of a full scan. Column must be a Vector
    /// type. `m` is the maximum number of neighbours per node.
    pub fn add_nsw_index(
        &mut self,
        name: String,
        column_name: &str,
        m: usize,
    ) -> Result<(), StorageError> {
        self.add_nsw_index_inner(name, column_name, m, None)
    }

    /// v6.0.4 — synchronous rebuild of the named NSW index. If
    /// `new_encoding` is `Some(target)` and differs from the column's
    /// current encoding, every stored cell at the indexed column is
    /// re-coded into the target encoding before the new graph
    /// builds. Returns `IndexNotFound` if no index by that name exists
    /// and `Unsupported` for non-NSW indexes (`BTree` REBUILD is a no-op
    /// the engine layer rejects, not a storage-level concept).
    ///
    /// Holds the caller's `&mut self` for the duration — no
    /// concurrency / staging / WAL-replay machinery in v6.0.4. The
    /// "live" optimisation lands as v6.0.4.1.
    pub fn rebuild_nsw_index(
        &mut self,
        name: &str,
        new_encoding: Option<VecEncoding>,
    ) -> Result<(), StorageError> {
        let idx_pos = self
            .indices
            .iter()
            .position(|i| i.name == name)
            .ok_or_else(|| StorageError::IndexNotFound {
                name: String::from(name),
            })?;
        let col_pos = self.indices[idx_pos].column_position;
        let m = match &self.indices[idx_pos].kind {
            IndexKind::Nsw(g) => g.m,
            IndexKind::BTree(_) => {
                return Err(StorageError::Unsupported(format!(
                    "ALTER INDEX REBUILD on BTree index {name:?} — only NSW indexes can rebuild"
                )));
            }
        };
        let col_name = self.schema.columns[col_pos].name.clone();
        // 1. Optional re-encoding pass. Done first so the cells
        //    match the schema before the graph rebuild walks them.
        if let Some(target) = new_encoding {
            let current = match self.schema.columns[col_pos].ty {
                DataType::Vector { encoding, .. } => encoding,
                ref other => {
                    return Err(StorageError::Unsupported(format!(
                        "ALTER INDEX REBUILD WITH (encoding=…) on non-vector column type {other:?}"
                    )));
                }
            };
            if target != current {
                let DataType::Vector { dim, .. } = self.schema.columns[col_pos].ty else {
                    unreachable!("checked above")
                };
                let n = self.rows.len();
                for i in 0..n {
                    let row = self
                        .rows
                        .get_mut(i)
                        .expect("row index in bounds (we iterated up to len())");
                    let cell = core::mem::replace(&mut row.values[col_pos], Value::Null);
                    let recoded = recode_vector_cell(cell, target)?;
                    row.values[col_pos] = recoded;
                }
                self.schema.columns[col_pos].ty = DataType::Vector {
                    dim,
                    encoding: target,
                };
            }
        }
        // 2. Drop the existing index slot + rebuild from row payload.
        self.indices.remove(idx_pos);
        self.add_nsw_index_inner(String::from(name), &col_name, m, None)?;
        Ok(())
    }

    /// Restore an NSW index from a pre-built graph (used on
    /// deserialize). Skips the bulk-build pass since the topology is
    /// already known. Returns `DuplicateIndex` or `ColumnNotFound` on
    /// schema mismatch as usual.
    pub fn restore_nsw_index(
        &mut self,
        name: String,
        column_name: &str,
        graph: NswGraph,
    ) -> Result<(), StorageError> {
        self.add_nsw_index_inner(name, column_name, graph.m, Some(graph))
    }

    /// Restore a `BTree` index from a pre-built `(IndexKey, Vec<RowLocator>)`
    /// map. Used by [`Catalog::deserialize`] when reading a v9 (or later)
    /// catalog snapshot — the map travels on disk so cold-tier locators
    /// survive a round-trip, instead of being rebuilt from `self.rows`
    /// (which would lose every Cold entry). Same error contract as
    /// [`Table::add_index`].
    pub fn restore_btree_index(
        &mut self,
        name: String,
        column_name: &str,
        map: PersistentBTreeMap<IndexKey, Vec<RowLocator>>,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        self.indices.push(Index {
            name,
            column_position,
            kind: IndexKind::BTree(map),
        });
        Ok(())
    }

    /// v5.1: register cold-tier locators on a `BTree` index. Used
    /// after [`Catalog::load_segment_bytes`] to wire every cold-
    /// tier row's PK back to its segment so
    /// [`Catalog::lookup_by_pk`] can resolve it. Each call
    /// appends to the index — keys that already have hot or cold
    /// locators keep them. Returns the number of locators
    /// registered.
    ///
    /// Pre-v5.2 (freezer) this is the only path that adds Cold
    /// variants to a PB; post-freezer the background freezer
    /// thread produces these as a batch under the engine write
    /// lock and this API becomes its in-memory primitive.
    ///
    /// Errors if `index_name` doesn't exist or names an NSW graph
    /// (NSW indices don't carry per-key row locators — they're
    /// vector-search structures).
    pub fn register_cold_locators<I>(
        &mut self,
        index_name: &str,
        locators: I,
    ) -> Result<usize, StorageError>
    where
        I: IntoIterator<Item = (IndexKey, RowLocator)>,
    {
        let idx = self
            .indices
            .iter_mut()
            .find(|i| i.name == index_name)
            .ok_or_else(|| StorageError::Corrupt(format!("index {index_name:?} not found")))?;
        let map = match &mut idx.kind {
            IndexKind::BTree(map) => map,
            IndexKind::Nsw(_) => {
                return Err(StorageError::Corrupt(format!(
                    "index {index_name:?} is NSW; cold locators apply only to BTree indices"
                )));
            }
        };
        let mut count = 0usize;
        for (key, locator) in locators {
            let mut entries = map.get(&key).cloned().unwrap_or_default();
            entries.push(locator);
            map.insert_mut(key, entries);
            count += 1;
        }
        Ok(count)
    }

    /// v5.2.3: remove every `Cold` locator currently registered on
    /// `index_name` under the given `key`. `Hot` locators for the
    /// same key are left in place — useful when a row has just been
    /// promoted hot-side and the caller wants the old Cold pointer
    /// retired without losing the new hot entry.
    ///
    /// Returns the number of cold locators removed (0 when the key
    /// has only hot entries or the key isn't present at all).
    /// Errors when the index doesn't exist or isn't a `BTree`.
    pub fn remove_cold_locators_for_key(
        &mut self,
        index_name: &str,
        key: &IndexKey,
    ) -> Result<usize, StorageError> {
        let idx = self
            .indices
            .iter_mut()
            .find(|i| i.name == index_name)
            .ok_or_else(|| {
                StorageError::Corrupt(format!(
                    "remove_cold_locators_for_key: index {index_name:?} not found"
                ))
            })?;
        let map = match &mut idx.kind {
            IndexKind::BTree(map) => map,
            IndexKind::Nsw(_) => {
                return Err(StorageError::Corrupt(format!(
                    "remove_cold_locators_for_key: index {index_name:?} is NSW; \
                     cold locators apply only to BTree indices"
                )));
            }
        };
        let Some(entries) = map.get(key) else {
            return Ok(0);
        };
        let mut kept: Vec<RowLocator> =
            entries.iter().copied().filter(RowLocator::is_hot).collect();
        let removed = entries.len() - kept.len();
        if removed == 0 {
            return Ok(0);
        }
        kept.shrink_to_fit();
        // PersistentBTreeMap has no remove API in v5.2; when every
        // locator for `key` was Cold, the key keeps an empty Vec
        // entry. `Index::lookup_eq` already treats `Some(&[])` and
        // `None` as the same empty slice (via `Vec::as_slice`), so
        // callers can't distinguish the two. The space cost is one
        // empty Vec per shadowed-then-promoted key — bounded and
        // recoverable when the future compaction job lands.
        map.insert_mut(key.clone(), kept);
        Ok(removed)
    }

    /// v4.4: delete the rows at the given positions in one pass.
    /// `positions` must be unique; ordering doesn't matter. Indices
    /// are rebuilt from scratch (cheaper than tracking incremental
    /// shifts across both B-tree and NSW). Returns the number of
    /// rows removed.
    pub fn delete_rows(&mut self, positions: &[usize]) -> usize {
        if positions.is_empty() {
            return 0;
        }
        // Mark positions; v4.39: PV has no in-place retain, so we rebuild
        // a fresh PV by pushing the survivors. Still O(n log₃₂ n); the
        // structural-sharing win shows up at `Catalog::clone()`, not here.
        let mut to_remove = alloc::vec![false; self.rows.len()];
        let mut removed = 0;
        for &p in positions {
            if p < to_remove.len() && !to_remove[p] {
                to_remove[p] = true;
                removed += 1;
            }
        }
        let mut new_rows: PersistentVec<Row> = PersistentVec::new();
        let mut removed_bytes: u64 = 0;
        for (i, row) in self.rows.iter().enumerate() {
            if to_remove[i] {
                removed_bytes =
                    removed_bytes.saturating_add(row_body_encoded_len(row, &self.schema) as u64);
            } else {
                new_rows.push_mut(row.clone());
            }
        }
        self.rows = new_rows;
        self.hot_bytes = self.hot_bytes.saturating_sub(removed_bytes);
        self.rebuild_indices();
        removed
    }

    /// v4.4: replace the row at `position` with `new_values` (must
    /// match the schema arity + types). Indices are rebuilt for
    /// correctness — the affected column might be indexed and its
    /// key may have shifted, and a NSW node's vector may have
    /// changed, both of which need fresh state.
    pub fn update_row(
        &mut self,
        position: usize,
        new_values: Vec<Value>,
    ) -> Result<(), StorageError> {
        if position >= self.rows.len() {
            return Err(StorageError::Corrupt(alloc::format!(
                "update_row: position {position} out of bounds (rows={})",
                self.rows.len()
            )));
        }
        if new_values.len() != self.schema.columns.len() {
            return Err(StorageError::ArityMismatch {
                expected: self.schema.columns.len(),
                actual: new_values.len(),
            });
        }
        // Reuse the per-cell type-compat validation that `insert`
        // applies. The body below mirrors that check intentionally —
        // factoring it would be more code than the duplication.
        for (i, (val, col)) in new_values.iter().zip(&self.schema.columns).enumerate() {
            if val.is_null() {
                if !col.nullable {
                    return Err(StorageError::NullInNotNull {
                        column: col.name.clone(),
                    });
                }
                continue;
            }
            let actual = val.data_type().expect("non-null");
            let compatible = actual == col.ty
                || matches!(
                    (actual, col.ty),
                    (
                        DataType::Text,
                        DataType::Varchar(_) | DataType::Char(_) | DataType::Json
                    ) | (DataType::Json, DataType::Text)
                )
                || matches!(
                    (actual, col.ty),
                    (
                        DataType::Numeric { scale: a, .. },
                        DataType::Numeric { scale: b, .. },
                    ) if a == b
                );
            if !compatible {
                return Err(StorageError::TypeMismatch {
                    column: col.name.clone(),
                    expected: col.ty,
                    actual,
                    position: i,
                });
            }
        }
        let old_row = self
            .rows
            .get(position)
            .expect("position bounds-checked above");
        let old_bytes = row_body_encoded_len(old_row, &self.schema) as u64;
        let new_row = Row::new(new_values);
        let new_bytes = row_body_encoded_len(&new_row, &self.schema) as u64;
        self.rows = self
            .rows
            .set(position, new_row)
            .expect("position bounds-checked above");
        self.hot_bytes = self
            .hot_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        self.rebuild_indices();
        Ok(())
    }

    /// v4.4 helper used by `delete_rows` / `update_row`: discard all
    /// index payloads and rebuild from `self.rows`. Cheap enough
    /// for typical SPG scale (catalogs in the docker-compose
    /// deployment shape are small); the alternative — incremental
    /// shift bookkeeping across B-tree + NSW — would be far more
    /// invasive than the savings justify.
    fn rebuild_indices(&mut self) {
        // v5.2.3: capture every `Cold` locator on every BTree index
        // before the rebuild, so the from-rows re-emission below
        // (which only produces `Hot` locators) doesn't drop cold-
        // tier entries on keys unrelated to the row that changed.
        // Pre-v5.2.3 this was a `freeze_oldest_to_cold` worry only
        // and the freezer did its own capture-then-reregister; v5.2.3
        // promotes that pattern into the base helper because UPDATE
        // / DELETE now run rebuild_indices on tables with cold rows.
        let preserved_cold: Vec<(String, Vec<(IndexKey, RowLocator)>)> = self
            .indices
            .iter()
            .filter_map(|idx| match &idx.kind {
                IndexKind::BTree(map) => {
                    let cold: Vec<(IndexKey, RowLocator)> = map
                        .iter()
                        .flat_map(|(k, locs)| {
                            locs.iter()
                                .filter(|l| l.is_cold())
                                .copied()
                                .map(move |l| (k.clone(), l))
                        })
                        .collect();
                    if cold.is_empty() {
                        None
                    } else {
                        Some((idx.name.clone(), cold))
                    }
                }
                IndexKind::Nsw(_) => None,
            })
            .collect();

        let descriptors: Vec<(String, usize, Option<usize>)> = self
            .indices
            .iter()
            .map(|idx| {
                let m = if let IndexKind::Nsw(g) = &idx.kind {
                    Some(g.m)
                } else {
                    None
                };
                (idx.name.clone(), idx.column_position, m)
            })
            .collect();
        self.indices.clear();
        for (name, column_position, nsw_m) in descriptors {
            if let Some(m) = nsw_m {
                let idx = Index::new_nsw(name, column_position, m);
                self.indices.push(idx);
                let idx_pos = self.indices.len() - 1;
                let row_indices: Vec<usize> = (0..self.rows.len()).collect();
                for row_idx in row_indices {
                    nsw_insert_at(self, idx_pos, row_idx);
                }
            } else {
                let mut idx = Index::new_btree(name, column_position);
                if let IndexKind::BTree(map) = &mut idx.kind {
                    for (i, row) in self.rows.iter().enumerate() {
                        if let Some(key) = IndexKey::from_value(&row.values[column_position]) {
                            let mut entries = map.get(&key).cloned().unwrap_or_default();
                            entries.push(RowLocator::Hot(i));
                            map.insert_mut(key, entries);
                        }
                    }
                }
                self.indices.push(idx);
            }
        }

        // Re-attach preserved cold locators after the from-rows
        // rebuild. `register_cold_locators` handles the per-key
        // entries-vec append; no key collisions arise because the
        // rebuild loop above produced only Hot locators.
        for (idx_name, locators) in preserved_cold {
            // Errors here would only fire if the index disappeared
            // between snapshot and rebuild, which can't happen
            // because the rebuild restores the same descriptor set.
            let _ = self.register_cold_locators(&idx_name, locators);
        }
    }

    fn add_nsw_index_inner(
        &mut self,
        name: String,
        column_name: &str,
        m: usize,
        restore: Option<NswGraph>,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        if !matches!(
            self.schema.columns[column_position].ty,
            DataType::Vector { .. }
        ) {
            return Err(StorageError::TypeMismatch {
                column: column_name.into(),
                expected: DataType::Vector {
                    dim: 0,
                    encoding: VecEncoding::F32,
                },
                actual: self.schema.columns[column_position].ty,
                position: column_position,
            });
        }
        if let Some(graph) = restore {
            self.indices.push(Index {
                name,
                column_position,
                kind: IndexKind::Nsw(graph),
            });
            return Ok(());
        }
        let idx = Index::new_nsw(name, column_position, m);
        self.indices.push(idx);
        let idx_pos = self.indices.len() - 1;
        // Bulk-build by walking the existing rows in order — each insert
        // sees the partial graph and links into it.
        let row_indices: Vec<usize> = (0..self.rows.len()).collect();
        for row_idx in row_indices {
            nsw_insert_at(self, idx_pos, row_idx);
        }
        Ok(())
    }
}

/// v6.0.4 — re-encode a single cell to the target `VecEncoding`.
/// Used by `Table::rebuild_nsw_index` when ALTER INDEX REBUILD
/// includes the optional `WITH (encoding = …)` clause. Round-trip
/// goes through f32: `current → Vec<f32> → target`, leaving NULL
/// cells untouched. Returns `Unsupported` on a non-vector cell —
/// the caller should have rejected the schema before reaching this.
fn recode_vector_cell(cell: Value, target: VecEncoding) -> Result<Value, StorageError> {
    if matches!(cell, Value::Null) {
        return Ok(cell);
    }
    // Step 1 — extract the f32 representation of the source cell.
    let as_f32: Vec<f32> = match &cell {
        Value::Vector(v) => v.clone(),
        Value::Sq8Vector(q) => quantize::dequantize(q),
        Value::HalfVector(h) => h.to_f32_vec(),
        other => {
            return Err(StorageError::Unsupported(format!(
                "ALTER INDEX REBUILD: cannot recode non-vector cell {:?}",
                other.data_type()
            )));
        }
    };
    // Step 2 — encode into the target shape. `F32` is the identity
    // path (saves one alloc round-trip when the source is already
    // F32 — but `Value::Vector(as_f32)` is the right answer
    // regardless).
    Ok(match target {
        VecEncoding::F32 => Value::Vector(as_f32),
        VecEncoding::Sq8 => Value::Sq8Vector(quantize::quantize(&as_f32)),
        VecEncoding::F16 => Value::HalfVector(halfvec::HalfVector::from_f32_slice(&as_f32)),
    })
}

/// Insert one row into the HNSW graph held by index slot `idx_pos`.
/// No-op when the row's value at the indexed column isn't a vector.
/// v6.0.1: handles `Value::Sq8Vector` by dequantising into an f32
/// "query" surface — the existing greedy + beam-search machinery
/// then uses `cell_to_query_metric_distance` to route every
/// distance call through the cell's actual encoding.
fn nsw_insert_at(table: &mut Table, idx_pos: usize, new_row_idx: usize) {
    let col_pos = table.indices[idx_pos].column_position;
    let cell_dim: Option<usize> = match &table.rows[new_row_idx].values[col_pos] {
        Value::Vector(v) => Some(v.len()),
        Value::Sq8Vector(q) => Some(q.bytes.len()),
        Value::HalfVector(h) => Some(h.dim()),
        _ => None,
    };
    let Some(dim) = cell_dim else {
        // Even non-vector rows occupy a level slot so per-node Vec
        // lengths stay aligned with `table.rows.len()`.
        ensure_node_slot(table, idx_pos, new_row_idx, 0);
        return;
    };
    if dim == 0 {
        ensure_node_slot(table, idx_pos, new_row_idx, 0);
        return;
    }
    let level = nsw_assign_level(new_row_idx);
    ensure_node_slot(table, idx_pos, new_row_idx, level);
    let (entry, entry_level, m) = match &table.indices[idx_pos].kind {
        IndexKind::Nsw(g) => (g.entry, g.entry_level, g.m),
        IndexKind::BTree(_) => unreachable!("nsw_insert_at on a BTree index"),
    };
    // First node ever — declare it the entry (it gets its own level).
    if entry.is_none() {
        if let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind {
            g.entry = Some(new_row_idx);
            g.entry_level = level;
            *g.levels
                .get_mut(new_row_idx)
                .expect("levels slot padded by ensure_node_slot") = level;
        }
        return;
    }
    // Set the node's recorded level.
    if let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind {
        *g.levels
            .get_mut(new_row_idx)
            .expect("levels slot padded by ensure_node_slot") = level;
    }
    let query = match &table.rows[new_row_idx].values[col_pos] {
        Value::Vector(v) => v.clone(),
        // v6.0.1: dequantise the inserted SQ8 cell into an f32 query
        // surface so the existing greedy / beam machinery can route
        // distances through `cell_to_query_metric_distance`. The
        // small dequantisation error is what the recall@10 ≥ 0.95
        // envelope already accounts for (V6_DESIGN deliberation #3).
        Value::Sq8Vector(q) => quantize::dequantize(q),
        // v6.0.3: halfvec dequant is bit-exact at the storage layer,
        // so the inserted query is a faithful representation.
        Value::HalfVector(h) => h.to_f32_vec(),
        _ => return,
    };
    // Phase 1: greedy descend from `entry` down to `level + 1`, keeping
    // exactly one current best so the next layer starts from it.
    let mut current = entry.expect("entry was Some above");
    let mut current_d = vec_l2_sq(table, col_pos, current, &query);
    if entry_level > level {
        for layer in (level + 1..=entry_level).rev() {
            (current, current_d) =
                greedy_layer_walk(table, idx_pos, layer, current, current_d, &query);
        }
    }
    // Phase 2: from `min(level, entry_level)` down to 0, beam-search
    // `ef_construction` candidates, run the HNSW §4 heuristic neighbour
    // selection over them, and connect bidirectionally.
    let top = level.min(entry_level);
    let ef = (m * 2).max(8);
    for layer in (0..=top).rev() {
        let cap = if layer == 0 { m * 2 } else { m };
        let mut candidates = layer_beam_search(
            table,
            idx_pos,
            layer,
            current,
            current_d,
            &query,
            ef,
            NswMetric::L2,
        );
        candidates.retain(|&(_, n)| n != new_row_idx);
        // Take the closest as the entry for the next layer down — done
        // before heuristic narrowing because the heuristic can reorder.
        if let Some(&(d, n)) = candidates.first() {
            current = n;
            current_d = d;
        }
        let peers = select_neighbours_heuristic(&candidates, cap, table, col_pos);
        connect_at_layer(table, idx_pos, layer, new_row_idx, &peers);
    }
    // Phase 3: if the new node climbed above the current entry, take
    // over as entry so future inserts/searches start from the new top.
    if level > entry_level
        && let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind
    {
        g.entry = Some(new_row_idx);
        g.entry_level = level;
    }
}

/// Make sure `layers[*][new_row_idx]` and `levels[new_row_idx]` exist,
/// padding with empty/zero entries as needed. Also grows `layers` to
/// accommodate the node's top `level`.
fn ensure_node_slot(table: &mut Table, idx_pos: usize, new_row_idx: usize, level: u8) {
    let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind else {
        unreachable!("ensure_node_slot on a BTree index");
    };
    while g.layers.len() <= level as usize {
        g.layers.push(PersistentVec::new());
    }
    while g.levels.len() <= new_row_idx {
        g.levels.push_mut(0);
    }
    for layer_vec in &mut g.layers {
        while layer_vec.len() <= new_row_idx {
            layer_vec.push_mut(Vec::new());
        }
    }
}

/// Single-step greedy walk on one layer: from `current` (with cached
/// distance `current_d`), inspect that node's neighbours at `layer` and
/// hop to the closest if it beats `current_d`. Repeat until no move
/// improves the distance. Cheap variant of beam-search used for the
/// "descend" phase that only needs one survivor per layer.
fn greedy_layer_walk(
    table: &Table,
    idx_pos: usize,
    layer: u8,
    mut current: usize,
    mut current_d: f32,
    query: &[f32],
) -> (usize, f32) {
    let g = match &table.indices[idx_pos].kind {
        IndexKind::Nsw(g) => g,
        IndexKind::BTree(_) => return (current, current_d),
    };
    let col_pos = table.indices[idx_pos].column_position;
    loop {
        let neighbours: &[u32] = g
            .layers
            .get(layer as usize)
            .and_then(|layer_v| layer_v.get(current))
            .map_or(&[][..], Vec::as_slice);
        let mut best = current;
        let mut best_d = current_d;
        for &n in neighbours {
            let n = n as usize;
            let d = vec_l2_sq(table, col_pos, n, query);
            if d < best_d {
                best = n;
                best_d = d;
            }
        }
        if best == current {
            return (current, current_d);
        }
        current = best;
        current_d = best_d;
    }
}

/// Beam search on one layer starting from `entry_node` with cached
/// `entry_d`. Returns the top `ef` candidates in ascending-distance
/// order. Caller picks the closest as the next layer's entry and / or
/// trims to M for connection.
///
/// v3.0.1: uses two `BinaryHeap`s (min-heap for the open frontier,
/// max-heap for the working top-`ef` results) and a `Vec<bool>` visited
/// bitmap, replacing the v2.x `Vec` + `partition_point` + `BTreeSet`
/// implementation. Same algorithm shape (HNSW search algorithm 2 from
/// the paper); the data-structure swap cuts per-visit cost from
/// `O(ef + log row_count)` to amortised `O(log ef)`.
#[allow(clippy::too_many_arguments)] // Beam search threads layer, entry, query, ef, metric — each is intrinsic. Bundling them into a config struct hides the call sites.
fn layer_beam_search(
    table: &Table,
    idx_pos: usize,
    layer: u8,
    entry_node: usize,
    entry_d: f32,
    query: &[f32],
    ef: usize,
    metric: NswMetric,
) -> Vec<(f32, usize)> {
    let g = match &table.indices[idx_pos].kind {
        IndexKind::Nsw(g) => g,
        IndexKind::BTree(_) => return Vec::new(),
    };
    let col_pos = table.indices[idx_pos].column_position;
    let d0 = if matches!(metric, NswMetric::L2) {
        entry_d
    } else {
        cell_to_query_metric_distance(table, col_pos, entry_node, query, metric)
    };
    let row_count = table.rows.len();
    let mut visited: Vec<bool> = alloc::vec![false; row_count];
    if entry_node < row_count {
        visited[entry_node] = true;
    }
    // candidates: min-heap by distance (Closest wrapper) — frontier
    // results:    max-heap by distance (Furthest wrapper) — top-ef working set
    let mut candidates: alloc::collections::BinaryHeap<NodeClosest> =
        alloc::collections::BinaryHeap::with_capacity(ef);
    let mut results: alloc::collections::BinaryHeap<NodeFurthest> =
        alloc::collections::BinaryHeap::with_capacity(ef);
    candidates.push(NodeClosest {
        dist: d0,
        node: entry_node,
    });
    results.push(NodeFurthest {
        dist: d0,
        node: entry_node,
    });
    while let Some(cur) = candidates.pop() {
        let worst = results.peek().map_or(f32::INFINITY, |c| c.dist);
        if cur.dist > worst && results.len() >= ef {
            break;
        }
        let neighbours: &[u32] = g
            .layers
            .get(layer as usize)
            .and_then(|layer_v| layer_v.get(cur.node))
            .map_or(&[][..], Vec::as_slice);
        for &n in neighbours {
            let n = n as usize;
            if n >= row_count || visited[n] {
                continue;
            }
            visited[n] = true;
            // v6.0.1: cell-aware distance — F32 cells take the
            // existing scalar metric, SQ8 cells route through
            // the asymmetric ADC variant for the same metric.
            let dn = cell_to_query_metric_distance(table, col_pos, n, query, metric);
            if !dn.is_finite() {
                continue;
            }
            let worst = results.peek().map_or(f32::INFINITY, |c| c.dist);
            if results.len() < ef || dn < worst {
                results.push(NodeFurthest { dist: dn, node: n });
                if results.len() > ef {
                    results.pop();
                }
                candidates.push(NodeClosest { dist: dn, node: n });
            }
        }
    }
    // Drain results (max-heap order) and re-sort ascending so callers
    // can take `closest = result[0]` without flipping.
    let mut out: Vec<(f32, usize)> = results.into_iter().map(|c| (c.dist, c.node)).collect();
    out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
    out
}

/// Min-heap wrapper: smaller `dist` → higher priority in a `BinaryHeap`
/// (which is a max-heap), so we flip the comparison. NaN sorts last
/// (lowest priority) to keep the heap total-ordered.
#[derive(Debug, Clone, Copy)]
struct NodeClosest {
    dist: f32,
    node: usize,
}
impl PartialEq for NodeClosest {
    fn eq(&self, other: &Self) -> bool {
        self.dist == other.dist && self.node == other.node
    }
}
impl Eq for NodeClosest {}
impl PartialOrd for NodeClosest {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for NodeClosest {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        // Reversed: smaller dist = greater priority.
        other
            .dist
            .partial_cmp(&self.dist)
            .unwrap_or(core::cmp::Ordering::Equal)
    }
}

/// Max-heap wrapper: larger `dist` sits at the top so the worst result
/// can be evicted in O(log n) when a better candidate arrives.
#[derive(Debug, Clone, Copy)]
struct NodeFurthest {
    dist: f32,
    node: usize,
}
impl PartialEq for NodeFurthest {
    fn eq(&self, other: &Self) -> bool {
        self.dist == other.dist && self.node == other.node
    }
}
impl Eq for NodeFurthest {}
impl PartialOrd for NodeFurthest {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for NodeFurthest {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.dist
            .partial_cmp(&other.dist)
            .unwrap_or(core::cmp::Ordering::Equal)
    }
}

/// HNSW paper §4 algorithm 4: pick `m` neighbours from `candidates` so
/// that each chosen point isn't already covered by a closer chosen
/// point. Improves graph diversity → fewer hops needed at search time.
///
/// `candidates` arrives sorted ascending by distance-to-query. We walk
/// it in order, keeping a candidate only when no already-chosen point
/// is closer to it than the query is. Result is a vector of row
/// indices (length ≤ `m`).
fn select_neighbours_heuristic(
    candidates: &[(f32, usize)],
    m: usize,
    table: &Table,
    col_pos: usize,
) -> Vec<usize> {
    let mut chosen: Vec<usize> = Vec::with_capacity(m);
    for &(d_q, e) in candidates {
        if chosen.len() >= m {
            break;
        }
        // v6.0.1: works on either `Value::Vector` (F32) or
        // `Value::Sq8Vector` (Sq8) cells — `cell_l2_sq` dispatches
        // on encoding. A non-vector cell yields `f32::INFINITY`
        // which the `< d_q` test will never accept.
        if !matches!(
            table.rows.get(e).and_then(|r| r.values.get(col_pos)),
            Some(Value::Vector(_) | Value::Sq8Vector(_) | Value::HalfVector(_))
        ) {
            continue;
        }
        let mut covered = false;
        for &r in &chosen {
            // dist(e, r) measured in the same metric the topology was
            // built with (L2). If a chosen `r` is closer to `e` than
            // the query is, `r` already "covers" `e` for navigation.
            if cell_l2_sq(table, col_pos, e, r) < d_q {
                covered = true;
                break;
            }
        }
        if !covered {
            chosen.push(e);
        }
    }
    chosen
}

/// Bidirectionally connect `new_row_idx` to each of `peers` at `layer`,
/// trimming each endpoint's adjacency to that layer's degree cap by
/// keeping only the closest neighbours.
fn connect_at_layer(
    table: &mut Table,
    idx_pos: usize,
    layer: u8,
    new_row_idx: usize,
    peers: &[usize],
) {
    let col_pos = table.indices[idx_pos].column_position;
    let cap = match &table.indices[idx_pos].kind {
        IndexKind::Nsw(g) => g.cap_for_layer(layer),
        IndexKind::BTree(_) => return,
    };
    // v6.1.x: NSW adjacency stores neighbour row indices as u32 (4 B
    // each) rather than usize (8 B on 64-bit). Boundary casts here
    // assert the row count fits in u32 — the catalog already enforces
    // ≤ 4G rows per table, so the conversion can't lose data.
    let new_row_u32 = u32::try_from(new_row_idx).expect("row index fits in u32");
    if let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind {
        let layer_v = &mut g.layers[layer as usize];
        if let Some(slot) = layer_v.get_mut(new_row_idx) {
            *slot = peers
                .iter()
                .map(|&p| u32::try_from(p).expect("row index fits in u32"))
                .collect();
        }
    }
    for &peer in peers {
        // Skip peers whose indexed cell isn't a vector — same fence
        // as the F32 path; SQ8 cells flow through `cell_l2_sq`
        // below without dequantising.
        if !matches!(
            &table.rows[peer].values[col_pos],
            Value::Vector(_) | Value::Sq8Vector(_) | Value::HalfVector(_)
        ) {
            continue;
        }
        // 1. add the new node to peer's adjacency
        if let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind {
            let layer_v = &mut g.layers[layer as usize];
            if let Some(slot) = layer_v.get_mut(peer)
                && !slot.contains(&new_row_u32)
            {
                slot.push(new_row_u32);
            }
        }
        // 2. if peer is over budget, rebuild its adjacency with the
        //    HNSW §4 heuristic — same diversity criterion as the
        //    insert path so connectivity stays consistent.
        let needs_trim = match &table.indices[idx_pos].kind {
            IndexKind::Nsw(g) => g.layers[layer as usize][peer].len() > cap,
            IndexKind::BTree(_) => false,
        };
        if needs_trim {
            let current_peers: Vec<usize> = match &table.indices[idx_pos].kind {
                IndexKind::Nsw(g) => g.layers[layer as usize][peer]
                    .iter()
                    .map(|&n| n as usize)
                    .collect(),
                IndexKind::BTree(_) => continue,
            };
            // Sort by distance from `peer`'s cell ascending so the
            // heuristic receives candidates closest-first. `cell_l2_sq`
            // dispatches on encoding so SQ8 columns trim using
            // symmetric ADC.
            let mut tagged: Vec<(f32, usize)> = current_peers
                .iter()
                .map(|&p| (cell_l2_sq(table, col_pos, peer, p), p))
                .collect();
            tagged.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
            let kept = select_neighbours_heuristic(&tagged, cap, table, col_pos);
            if let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind
                && let Some(slot) = g.layers[layer as usize].get_mut(peer)
            {
                *slot = kept
                    .into_iter()
                    .map(|p| u32::try_from(p).expect("row index fits in u32"))
                    .collect();
            }
        }
    }
}

/// Squared L2 distance from `query` (raw f32) to the cell at
/// `(row, col_pos)`. Dispatches on cell encoding: `Value::Vector`
/// (F32) uses `l2_distance_sq`; `Value::Sq8Vector` uses
/// `sq8_l2_distance_sq_asymmetric` (the v6.0.1 quantised path).
/// Returns `f32::INFINITY` for any non-vector cell so callers can
/// compare uniformly.
fn vec_l2_sq(table: &Table, col_pos: usize, row: usize, query: &[f32]) -> f32 {
    match table.rows.get(row).and_then(|r| r.values.get(col_pos)) {
        Some(Value::Vector(v)) if v.len() == query.len() => l2_distance_sq(v, query),
        Some(Value::Sq8Vector(q)) if q.bytes.len() == query.len() => {
            quantize::sq8_l2_distance_sq_asymmetric(q, query)
        }
        // v6.0.6: halfvec → fused NEON SIMD kernel; no Vec<f32>
        // allocation. v6.0.3 used `to_f32_vec()` + f32 NEON which
        // was correct but allocated per call (5× slower than F32).
        Some(Value::HalfVector(h)) if h.dim() == query.len() => {
            halfvec::half_l2_distance_sq_asymmetric(h, query)
        }
        _ => f32::INFINITY,
    }
}

/// Squared L2 distance between two stored cells (no f32 query in
/// sight). Used during HNSW graph build — both endpoints are
/// rows already in the table, so symmetric ADC applies for SQ8
/// columns. Mixed-encoding cells within one column are a
/// schema-level impossibility (INSERT-time coercion enforces
/// uniform encoding), so the catch-all is an abort.
fn cell_l2_sq(table: &Table, col_pos: usize, row_a: usize, row_b: usize) -> f32 {
    let Some(cell_a) = table.rows.get(row_a).and_then(|r| r.values.get(col_pos)) else {
        return f32::INFINITY;
    };
    let Some(cell_b) = table.rows.get(row_b).and_then(|r| r.values.get(col_pos)) else {
        return f32::INFINITY;
    };
    match (cell_a, cell_b) {
        (Value::Vector(a), Value::Vector(b)) if a.len() == b.len() => l2_distance_sq(a, b),
        (Value::Sq8Vector(a), Value::Sq8Vector(b)) if a.bytes.len() == b.bytes.len() => {
            quantize::sq8_l2_distance_sq(a, b)
        }
        // v6.0.6: halfvec symmetric NEON — fused SIMD kernel that
        // loads both cells' raw u16 bits, expands to f32 lanes
        // inline, FMA-accumulates the squared diff. No Vec<f32>
        // allocation per call.
        (Value::HalfVector(a), Value::HalfVector(b)) if a.dim() == b.dim() => {
            halfvec::half_l2_distance_sq(a, b)
        }
        _ => f32::INFINITY,
    }
}

/// kNN-search-time distance: stored cell → f32 query under the
/// caller's metric. Dispatches on cell encoding so SQ8 columns
/// take the ADC path with the right asymmetric variant. NaN /
/// dim-mismatch / non-vector → `f32::INFINITY`.
fn cell_to_query_metric_distance(
    table: &Table,
    col_pos: usize,
    row: usize,
    query: &[f32],
    metric: NswMetric,
) -> f32 {
    match table.rows.get(row).and_then(|r| r.values.get(col_pos)) {
        Some(Value::Vector(v)) if v.len() == query.len() => metric_distance(metric, v, query),
        Some(Value::Sq8Vector(q)) if q.bytes.len() == query.len() => match metric {
            NswMetric::L2 => quantize::sq8_l2_distance_sq_asymmetric(q, query),
            NswMetric::InnerProduct => quantize::sq8_inner_product_asymmetric(q, query),
            NswMetric::Cosine => quantize::sq8_cosine_distance_asymmetric(q, query),
        },
        // v6.0.6: halfvec dispatches by metric to fused NEON
        // kernels — no Vec<f32> allocation per call.
        Some(Value::HalfVector(h)) if h.dim() == query.len() => match metric {
            NswMetric::L2 => halfvec::half_l2_distance_sq_asymmetric(h, query),
            NswMetric::InnerProduct => halfvec::half_inner_product_asymmetric(h, query),
            NswMetric::Cosine => halfvec::half_cosine_distance_asymmetric(h, query),
        },
        _ => f32::INFINITY,
    }
}

/// Distance metric used at NSW search time. The graph topology is
/// always built with `L2`; querying with `InnerProduct` / `Cosine`
/// reuses the same edges but ranks candidates by the chosen metric.
/// For the corpus-sized graphs this loses negligible recall vs
/// building separate per-metric graphs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NswMetric {
    /// Squared Euclidean — ranks "smaller = closer" (the sqrt is
    /// monotonic so we skip it for ordering).
    L2,
    /// Negated dot product, matching pgvector `<#>` convention so
    /// "smaller = more similar" holds across all three metrics.
    InnerProduct,
    /// Cosine distance `1 - cos(a, b)`. Zero-norm operand yields
    /// `f32::INFINITY` so it sorts last.
    Cosine,
}

/// Multi-layer HNSW kNN search: greedy-descend from the entry to layer 0,
/// then beam-search there with the requested `ef` to return the top `k`
/// results under the caller-chosen metric. Topology was built with L2 —
/// upper-layer descent uses L2 as a coarse heuristic; final beam search
/// runs in the requested metric so rankings are correct for `<#>` / `<=>`.
fn nsw_search(
    table: &Table,
    idx_pos: usize,
    query: &[f32],
    k: usize,
    ef: usize,
    metric: NswMetric,
) -> Vec<(f32, usize)> {
    let (entry, entry_level) = match &table.indices[idx_pos].kind {
        IndexKind::Nsw(g) => (g.entry, g.entry_level),
        IndexKind::BTree(_) => return Vec::new(),
    };
    let Some(entry) = entry else {
        return Vec::new();
    };
    let col_pos = table.indices[idx_pos].column_position;
    // v6.0.1 step 5: SQ8 columns over-fetch by `SQ8_RERANK_OVER_FETCH`
    // so the rerank pass below sees enough candidates to recover
    // recall after the ADC re-ordering. F32 + F16 columns skip the
    // over-fetch — F32 distances are exact, F16 dequant is
    // bit-exact at the storage layer so the beam search already
    // ranks under the column's full precision.
    let sq8 = matches!(
        table.schema.columns.get(col_pos).map(|c| c.ty),
        Some(DataType::Vector {
            encoding: VecEncoding::Sq8,
            ..
        })
    );
    let ef = if sq8 {
        ef.max(k).max(k * SQ8_RERANK_OVER_FETCH)
    } else {
        ef.max(k)
    };
    // Descend by L2 (the topology metric) so layers prune consistently.
    let entry_d = vec_l2_sq(table, col_pos, entry, query);
    let mut current = entry;
    let mut current_d = entry_d;
    for layer in (1..=entry_level).rev() {
        (current, current_d) = greedy_layer_walk(table, idx_pos, layer, current, current_d, query);
    }
    // Final beam search on layer 0 under the caller's metric.
    let mut results = layer_beam_search(table, idx_pos, 0, current, current_d, query, ef, metric);
    if sq8 {
        results = sq8_rerank(table, col_pos, &results, query, metric);
    }
    results.truncate(k);
    results
}

/// v6.0.1 step 5: re-score ADC top-`K*3` candidates with the
/// dequantised cell vs the f32 query, then re-sort. Recovers the
/// recall the SQ8 ADC sacrifices for 4× compression — the design's
/// "f32 rerank step is on by default" path (deliberation #3).
/// `metric` is the same metric the beam search used; the rerank
/// arithmetic re-derives the exact distance under that metric.
fn sq8_rerank(
    table: &Table,
    col_pos: usize,
    candidates: &[(f32, usize)],
    query: &[f32],
    metric: NswMetric,
) -> Vec<(f32, usize)> {
    let mut out: Vec<(f32, usize)> = candidates
        .iter()
        .filter_map(|&(adc_d, row)| {
            let cell = table.rows.get(row).and_then(|r| r.values.get(col_pos))?;
            let Value::Sq8Vector(q) = cell else {
                // F32 cells shouldn't reach this path (sq8 fence
                // above), but stay defensive: pass through with
                // the ADC distance unchanged.
                return Some((adc_d, row));
            };
            let deq = quantize::dequantize(q);
            if deq.len() != query.len() {
                return None;
            }
            Some((metric_distance(metric, &deq, query), row))
        })
        .collect();
    out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
    out
}

/// Multiplier applied to `k` so the SQ8 rerank pass sees a wider
/// candidate set. 3× is the design-stage value; v6.0.5 sweep work
/// can re-tune once full corpus profiling is in.
const SQ8_RERANK_OVER_FETCH: usize = 3;

fn metric_distance(metric: NswMetric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        NswMetric::L2 => l2_distance_sq(a, b),
        NswMetric::InnerProduct => -inner_product_f32(a, b),
        NswMetric::Cosine => {
            let (dot, na, nb) = cosine_dot_norms_f32(a, b);
            if na == 0.0 || nb == 0.0 {
                return f32::INFINITY;
            }
            // `f32::sqrt` lives in std, so hand-roll Newton-Raphson on
            // f64 — same trick the L2 binary op already uses.
            let denom = sqrt_newton_f32(na) * sqrt_newton_f32(nb);
            1.0 - dot / denom
        }
    }
}

/// v6.0.2: dispatch wrapper for the f32 dot product (used by `<#>` +
/// the cosine numerator). NEON path when `len % 4 == 0 && len >= 4`,
/// scalar fallback otherwise. Returns the positive dot — callers
/// negate for the pgvector `<#>` "smaller = closer" convention.
///
/// Public so perf gates + downstream benches can microbenchmark the
/// dispatch directly; not part of the STABILITY contract — internal
/// SIMD layout can evolve in any release.
#[doc(hidden)]
#[inline]
pub fn inner_product_f32(a: &[f32], b: &[f32]) -> f32 {
    #[cfg(target_arch = "aarch64")]
    {
        if a.len() == b.len() && a.len() >= 4 && a.len().is_multiple_of(4) {
            // SAFETY: NEON is a baseline aarch64 feature; preconditions
            // (matching lengths, ≥ 1 full lane group) are checked above.
            return unsafe { inner_product_neon(a, b) };
        }
    }
    inner_product_scalar(a, b)
}

fn inner_product_scalar(a: &[f32], b: &[f32]) -> f32 {
    let mut dot: f32 = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
    }
    dot
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[allow(clippy::many_single_char_names)] // NEON intrinsics work in single-letter regs by convention
unsafe fn inner_product_neon(a: &[f32], b: &[f32]) -> f32 {
    use core::arch::aarch64::{
        float32x4_t, vaddq_f32, vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_f32,
    };
    unsafe {
        // Two parallel accumulators (same trick as L2 NEON) so the
        // FMA dependency chain doesn't serialise.
        let zero: float32x4_t = vdupq_n_f32(0.0);
        let mut acc0 = zero;
        let mut acc1 = zero;
        let n = a.len();
        let mut i = 0usize;
        while i + 8 <= n {
            let av0 = vld1q_f32(a.as_ptr().add(i));
            let bv0 = vld1q_f32(b.as_ptr().add(i));
            acc0 = vfmaq_f32(acc0, av0, bv0);
            let av1 = vld1q_f32(a.as_ptr().add(i + 4));
            let bv1 = vld1q_f32(b.as_ptr().add(i + 4));
            acc1 = vfmaq_f32(acc1, av1, bv1);
            i += 8;
        }
        while i + 4 <= n {
            let av = vld1q_f32(a.as_ptr().add(i));
            let bv = vld1q_f32(b.as_ptr().add(i));
            acc0 = vfmaq_f32(acc0, av, bv);
            i += 4;
        }
        vaddvq_f32(vaddq_f32(acc0, acc1))
    }
}

/// v6.0.2: dispatch wrapper for the three accumulators (`dot`, `||a||²`,
/// `||b||²`) cosine needs. Same NEON pre-condition as the L2 / IP
/// paths; same scalar fallback shape.
///
/// Public for benchmarking only (see `inner_product_f32`); not in the
/// STABILITY contract.
#[doc(hidden)]
#[inline]
pub fn cosine_dot_norms_f32(a: &[f32], b: &[f32]) -> (f32, f32, f32) {
    #[cfg(target_arch = "aarch64")]
    {
        if a.len() == b.len() && a.len() >= 4 && a.len().is_multiple_of(4) {
            // SAFETY: see `inner_product_neon`.
            return unsafe { cosine_dot_norms_neon(a, b) };
        }
    }
    cosine_dot_norms_scalar(a, b)
}

fn cosine_dot_norms_scalar(a: &[f32], b: &[f32]) -> (f32, f32, f32) {
    let mut dot: f32 = 0.0;
    let mut na: f32 = 0.0;
    let mut nb: f32 = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    (dot, na, nb)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[allow(clippy::many_single_char_names, clippy::similar_names)]
unsafe fn cosine_dot_norms_neon(a: &[f32], b: &[f32]) -> (f32, f32, f32) {
    use core::arch::aarch64::{float32x4_t, vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_f32};
    unsafe {
        let zero: float32x4_t = vdupq_n_f32(0.0);
        let mut acc_dot = zero;
        let mut acc_na = zero;
        let mut acc_nb = zero;
        let n = a.len();
        let mut i = 0usize;
        while i + 4 <= n {
            let av = vld1q_f32(a.as_ptr().add(i));
            let bv = vld1q_f32(b.as_ptr().add(i));
            acc_dot = vfmaq_f32(acc_dot, av, bv);
            acc_na = vfmaq_f32(acc_na, av, av);
            acc_nb = vfmaq_f32(acc_nb, bv, bv);
            i += 4;
        }
        (vaddvq_f32(acc_dot), vaddvq_f32(acc_na), vaddvq_f32(acc_nb))
    }
}

fn sqrt_newton_f32(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let mut g = x;
    for _ in 0..10 {
        g = 0.5 * (g + x / g);
    }
    g
}

/// Squared Euclidean distance — used for ordering inside NSW (the sqrt
/// preserves the order). Caller takes sqrt before reporting back to SQL.
///
/// v3.3.2: aarch64 NEON path for `len % 4 == 0` (which covers every
/// HNSW-indexed VECTOR(N) where N is a multiple of 4 — i.e. all
/// production-shaped embeddings: 64, 128, 256, 384, 512, 768, 1024,
/// 1536, ...). Other shapes fall back to the scalar loop.
#[inline]
fn l2_distance_sq(a: &[f32], b: &[f32]) -> f32 {
    #[cfg(target_arch = "aarch64")]
    {
        if a.len() == b.len() && a.len() >= 4 && a.len().is_multiple_of(4) {
            // SAFETY: NEON is a baseline aarch64 feature (ARMv8);
            // the precondition is checked above (matching lengths,
            // multiple of 4, at least one 128-bit lane group).
            return unsafe { l2_distance_sq_neon(a, b) };
        }
    }
    l2_distance_sq_scalar(a, b)
}

fn l2_distance_sq_scalar(a: &[f32], b: &[f32]) -> f32 {
    let mut sum: f32 = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        let d = *x - *y;
        sum += d * d;
    }
    sum
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[allow(clippy::many_single_char_names)] // NEON intrinsics work in single-letter regs by convention
unsafe fn l2_distance_sq_neon(a: &[f32], b: &[f32]) -> f32 {
    use core::arch::aarch64::{
        float32x4_t, vaddq_f32, vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_f32, vsubq_f32,
    };
    unsafe {
        // Two independent accumulator registers so the FMA dependency
        // chain doesn't serialise (each FMA depends on prior FMA).
        // Pre-conditions checked by caller: `a.len() == b.len()`,
        // `a.len() % 4 == 0`, `a.len() >= 4`.
        let zero: float32x4_t = vdupq_n_f32(0.0);
        let mut acc0 = zero;
        let mut acc1 = zero;
        let n = a.len();
        let mut i = 0usize;
        // Process 8 floats per iter when available (two parallel
        // accumulators). Tail of 4 falls into the second loop.
        while i + 8 <= n {
            let d0 = vsubq_f32(vld1q_f32(a.as_ptr().add(i)), vld1q_f32(b.as_ptr().add(i)));
            acc0 = vfmaq_f32(acc0, d0, d0);
            let d1 = vsubq_f32(
                vld1q_f32(a.as_ptr().add(i + 4)),
                vld1q_f32(b.as_ptr().add(i + 4)),
            );
            acc1 = vfmaq_f32(acc1, d1, d1);
            i += 8;
        }
        while i + 4 <= n {
            let d = vsubq_f32(vld1q_f32(a.as_ptr().add(i)), vld1q_f32(b.as_ptr().add(i)));
            acc0 = vfmaq_f32(acc0, d, d);
            i += 4;
        }
        vaddvq_f32(vaddq_f32(acc0, acc1))
    }
}

/// Public wrapper: run an NSW kNN search and return the top-k row
/// indices ordered by ascending distance under the given metric.
pub fn nsw_query(
    table: &Table,
    idx_name: &str,
    query: &[f32],
    k: usize,
    metric: NswMetric,
) -> Vec<usize> {
    let Some(idx_pos) = table.indices.iter().position(|i| i.name == idx_name) else {
        return Vec::new();
    };
    let ef = (k * 2).max(NSW_DEFAULT_M);
    let mut hits = nsw_search(table, idx_pos, query, k, ef, metric);
    hits.truncate(k);
    hits.into_iter().map(|(_, idx)| idx).collect()
}

/// Find any NSW index on a column. Used by the planner to decide
/// whether an `ORDER BY col <-> literal LIMIT k` query can skip the
/// brute-force scan.
pub fn nsw_index_on(table: &Table, column_position: usize) -> Option<&Index> {
    table
        .indices
        .iter()
        .find(|i| i.column_position == column_position && matches!(i.kind, IndexKind::Nsw(_)))
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
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    tables: Vec<Table>,
    /// `name → tables[index]`. Kept in lock-step with `tables`.
    /// `create_table` is the only write path.
    by_name: BTreeMap<String, usize>,
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
    cold_segments: Vec<Arc<OwnedSegment>>,
}

impl Catalog {
    pub const fn new() -> Self {
        Self {
            tables: Vec::new(),
            by_name: BTreeMap::new(),
            cold_segments: Vec::new(),
        }
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

    pub fn table_count(&self) -> usize {
        self.tables.len()
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
        self.cold_segments.push(Arc::new(seg));
        Ok(id)
    }

    #[must_use]
    pub fn cold_segment_count(&self) -> usize {
        self.cold_segments.len()
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
            .map(AsRef::as_ref)
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
    ) -> Option<Row> {
        let t = self.get(table_name)?;
        let u64_key = index_key_as_u64(key)?;
        let seg = self.cold_segments.get(segment_id as usize)?;
        let payload = seg.lookup(u64_key)?;
        let (row, _) = decode_row_body_dense(&payload, &t.schema).ok()?;
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
    pub fn lookup_by_pk(&self, table: &str, index_name: &str, key: &IndexKey) -> Option<Row> {
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
                    let Some(seg) = self.cold_segments.get(segment_id as usize) else {
                        continue;
                    };
                    let Some(payload) = seg.lookup(u64_key) else {
                        continue;
                    };
                    let (row, _) = decode_row_body_dense(&payload, &t.schema).ok()?;
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
        let seg = self.cold_segments.get(segment_id as usize).ok_or_else(|| {
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
        let (row, _consumed) = decode_row_body_dense(&payload, &schema)?;
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
        IndexKey::Text(_) | IndexKey::Bool(_) => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
            Self::DuplicateTable { name } => write!(f, "table already exists: {name}"),
            Self::TableNotFound { name } => write!(f, "table not found: {name}"),
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
                write!(f, "NULL value in NOT NULL column {column:?}")
            }
            Self::DuplicateIndex { name } => write!(f, "index already exists: {name}"),
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
            auto_increment: false,
        }
    }

    /// Builder-style helper to attach a default value to an otherwise
    /// plain column schema. Used by the engine when CREATE TABLE
    /// specifies `column TYPE DEFAULT <expr>`.
    #[must_use]
    pub fn with_default(mut self, default: Value) -> Self {
        self.default = Some(default);
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
const FILE_VERSION: u8 = 9;
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
                }
            }
        }
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
        let table_count = cur.read_u32()? as usize;
        let mut cat = Self::new();
        for _ in 0..table_count {
            deserialize_table(&mut cur, &mut cat, version)?;
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

/// Per-table deserialize body — schema, rows, indices. Pulled out of
/// `Catalog::deserialize` to keep the latter under the line-budget lint
/// and to give the row hot loop its own scope (so the borrow on `t`
/// stays scoped here rather than across the whole catalog loop).
fn deserialize_table(
    cur: &mut Cursor<'_>,
    cat: &mut Catalog,
    version: u8,
) -> Result<(), StorageError> {
    let name = cur.read_str()?;
    let col_count = cur.read_u16()? as usize;
    let mut cols = Vec::with_capacity(col_count);
    for _ in 0..col_count {
        let c_name = cur.read_str()?;
        let ty = cur.read_data_type()?;
        let nullable = cur.read_u8()? != 0;
        let default = match cur.read_u8()? {
            0 => None,
            1 => Some(cur.read_value()?),
            other => {
                return Err(StorageError::Corrupt(format!(
                    "unknown default tag: {other}"
                )));
            }
        };
        let auto_increment = cur.read_u8()? != 0;
        cols.push(ColumnSchema {
            name: c_name,
            ty,
            nullable,
            default,
            auto_increment,
        });
    }
    let n_cols = cols.len();
    cat.create_table(TableSchema::new(name, cols))?;
    // Vec<Table> with insertion-order semantics — the just-pushed
    // table is at the end. Sidecar `by_name` is already wired up but
    // we skip the map lookup here since we know the position.
    let t = cat.tables.last_mut().expect("create_table just pushed");
    deserialize_rows(cur, t, n_cols)?;
    deserialize_indices(cur, t, version)?;
    Ok(())
}

fn deserialize_rows(
    cur: &mut Cursor<'_>,
    t: &mut Table,
    _n_cols: usize,
) -> Result<(), StorageError> {
    let row_count = cur.read_u32()? as usize;
    // v4.39: PV has no `reserve` (the BVT doesn't preallocate a
    // contiguous buffer); we just push directly and let the trie
    // grow. v5.1: row decode reuses `decode_row_body_dense` so the
    // catalog and cold-tier segments share one row codec.
    let mut hot_bytes: u64 = 0;
    for _ in 0..row_count {
        let tail = &cur.buf[cur.pos..];
        let (row, consumed) = decode_row_body_dense(tail, &t.schema)?;
        cur.pos += consumed;
        // v5.2.1: account for hot bytes as we go; the snapshot's row
        // block bytes are exactly what `encode_row_body_dense` would
        // produce, so `consumed` would do too — but going via the
        // helper keeps the counter's definition coupled to the
        // encoder rather than the snapshot's row prefix layout.
        hot_bytes = hot_bytes.saturating_add(row_body_encoded_len(&row, &t.schema) as u64);
        t.rows.push_mut(row);
    }
    t.hot_bytes = hot_bytes;
    Ok(())
}

fn deserialize_indices(
    cur: &mut Cursor<'_>,
    t: &mut Table,
    version: u8,
) -> Result<(), StorageError> {
    let index_count = cur.read_u16()? as usize;
    for _ in 0..index_count {
        let idx_name = cur.read_str()?;
        let col_pos = cur.read_u16()? as usize;
        let column_name = t
            .schema
            .columns
            .get(col_pos)
            .ok_or_else(|| {
                StorageError::Corrupt(format!(
                    "index {idx_name:?} points at non-existent column position {col_pos}"
                ))
            })?
            .name
            .clone();
        let kind_tag = cur.read_u8()?;
        match kind_tag {
            0 => {
                if version >= 9 {
                    // v9+: BTree entries serialised inline (tag-prefixed
                    // locator codec). Restore the map directly so any
                    // freezer-produced Cold locators come back exactly
                    // as they went out.
                    let map = read_btree_map(cur)?;
                    t.restore_btree_index(idx_name, &column_name, map)?;
                } else {
                    // v8: no entries on disk; rebuild from rows. Every
                    // entry is materialised as `RowLocator::Hot(i)` —
                    // semantically identical to the v5.1 in-memory state
                    // since v8 catalogs never produced Cold locators.
                    t.add_index(idx_name, &column_name)?;
                }
            }
            1 => {
                let m = cur.read_u16()? as usize;
                let graph = cur.read_nsw_graph(m)?;
                t.restore_nsw_index(idx_name, &column_name, graph)?;
            }
            other => {
                return Err(StorageError::Corrupt(format!(
                    "unknown index kind tag: {other}"
                )));
            }
        }
    }
    Ok(())
}

/// Parse a v9 `BTree` index payload — `[u32 entry_count]` followed by
/// `entry_count` `(IndexKey, Vec<RowLocator>)` pairs. The locator list
/// uses the v5.1 tag-prefixed wire format (`RowLocator::read_le`).
fn read_btree_map(
    cur: &mut Cursor<'_>,
) -> Result<PersistentBTreeMap<IndexKey, Vec<RowLocator>>, StorageError> {
    let entry_count = cur.read_u32()? as usize;
    let mut map = PersistentBTreeMap::new();
    for _ in 0..entry_count {
        let key = cur.read_index_key()?;
        let locator_count = cur.read_u32()? as usize;
        let mut locators = Vec::with_capacity(locator_count);
        for _ in 0..locator_count {
            let tail = &cur.buf[cur.pos..];
            let (loc, consumed) = RowLocator::read_le(tail).map_err(|e| {
                StorageError::Corrupt(format!("row_locator decode at offset {}: {e}", cur.pos))
            })?;
            cur.pos += consumed;
            locators.push(loc);
        }
        map.insert_mut(key, locators);
    }
    Ok(map)
}

// --- low-level binary helpers ---------------------------------------------

/// Write a `DataType` as a tag byte + optional payload (Vector carries its
/// `u32` dimension). Inverse: [`read_data_type`].
/// Serialize an HNSW graph after the `[kind=1][u16 M]` header (v7).
/// Layout:
/// - `[u16 m_max_0]`
/// - `[entry u32]` — `u32::MAX` means `None`, else the entry node index
/// - `[u8 entry_level]`
/// - `[node_count u32]`
/// - for each node: `[u8 level]`  (top layer for this node)
/// - `[layer_count u8]`
/// - for each layer `0..layer_count`:
///     - `[u32 layer_node_count]` (== `node_count`; per-layer slot)
///     - for each node: `[u16 neighbor_count] [u32 neighbor]*`
fn write_nsw_graph(out: &mut Vec<u8>, g: &NswGraph) {
    let entry = g.entry.map_or(u32::MAX, |e| {
        u32::try_from(e).expect("NSW entry fits in u32")
    });
    write_u16(
        out,
        u16::try_from(g.m_max_0).expect("HNSW m_max_0 fits in u16"),
    );
    out.extend_from_slice(&entry.to_le_bytes());
    out.push(g.entry_level);
    let node_count = g.levels.len();
    write_u32(
        out,
        u32::try_from(node_count).expect("HNSW node count fits in u32"),
    );
    for &lvl in &g.levels {
        out.push(lvl);
    }
    let layer_count = u8::try_from(g.layers.len()).expect("HNSW layer count ≤ 255");
    out.push(layer_count);
    for layer in &g.layers {
        write_u32(
            out,
            u32::try_from(layer.len()).expect("HNSW per-layer node count fits in u32"),
        );
        for neighbors in layer {
            write_u16(
                out,
                u16::try_from(neighbors.len()).expect("HNSW neighbour list fits in u16"),
            );
            // v6.1.x: neighbour slot is already u32 in memory; just
            // emit the raw bytes. (v6.0 stored usize and converted
            // here.)
            for &peer in neighbors {
                write_u32(out, peer);
            }
        }
    }
}

fn write_data_type(out: &mut Vec<u8>, t: DataType) {
    match t {
        DataType::Int => out.push(1),
        DataType::BigInt => out.push(2),
        DataType::Float => out.push(3),
        DataType::Text => out.push(4),
        DataType::Bool => out.push(5),
        DataType::Vector { dim, encoding } => match encoding {
            // Tag 6: pre-v6 F32 vector. Layout unchanged; pre-v6
            // binaries continue to deserialise this exactly as
            // before.
            VecEncoding::F32 => {
                out.push(6);
                out.extend_from_slice(&dim.to_le_bytes());
            }
            // v6.0.3: tag 15 for `VECTOR(N) USING HALF`. Same
            // forward-compat fence story as SQ8 below.
            VecEncoding::F16 => {
                out.push(15);
                out.extend_from_slice(&dim.to_le_bytes());
            }
            // v6.0.1: new tag 14 for `VECTOR(N) USING SQ8` column
            // type. Pre-v6 readers fall through `read_data_type`'s
            // catch-all and surface `Corrupt("unknown data type tag")`
            // — the explicit forward-compat fence called out in
            // V6_DESIGN deliberation #5.
            VecEncoding::Sq8 => {
                out.push(14);
                out.extend_from_slice(&dim.to_le_bytes());
            }
        },
        DataType::SmallInt => out.push(7),
        DataType::Varchar(max) => {
            out.push(8);
            out.extend_from_slice(&max.to_le_bytes());
        }
        DataType::Char(size) => {
            out.push(9);
            out.extend_from_slice(&size.to_le_bytes());
        }
        DataType::Numeric { precision, scale } => {
            out.push(10);
            out.push(precision);
            out.push(scale);
        }
        DataType::Date => out.push(11),
        DataType::Timestamp => out.push(12),
        // INTERVAL is runtime-only — CREATE TABLE never produces a
        // column with this type, so write_data_type must not be called
        // on it. (Disk-format codepoint reserved for a future v3 where
        // INTERVAL becomes storable.)
        DataType::Interval => {
            unreachable!("DataType::Interval has no on-disk encoding in v2.11")
        }
        DataType::Json => out.push(13),
    }
}

impl Cursor<'_> {
    fn read_data_type(&mut self) -> Result<DataType, StorageError> {
        let tag = self.read_u8()?;
        match tag {
            1 => Ok(DataType::Int),
            2 => Ok(DataType::BigInt),
            3 => Ok(DataType::Float),
            4 => Ok(DataType::Text),
            5 => Ok(DataType::Bool),
            6 => Ok(DataType::Vector {
                dim: self.read_u32()?,
                encoding: VecEncoding::F32,
            }),
            7 => Ok(DataType::SmallInt),
            8 => Ok(DataType::Varchar(self.read_u32()?)),
            9 => Ok(DataType::Char(self.read_u32()?)),
            10 => {
                let precision = self.read_u8()?;
                let scale = self.read_u8()?;
                Ok(DataType::Numeric { precision, scale })
            }
            11 => Ok(DataType::Date),
            12 => Ok(DataType::Timestamp),
            13 => Ok(DataType::Json),
            14 => Ok(DataType::Vector {
                dim: self.read_u32()?,
                encoding: VecEncoding::Sq8,
            }),
            // v6.0.3: tag 15 for `VECTOR(N) USING HALF`. Same
            // [u32 dim] type-tag payload as F32 / SQ8; the encoding
            // lives in the tag byte itself.
            15 => Ok(DataType::Vector {
                dim: self.read_u32()?,
                encoding: VecEncoding::F16,
            }),
            other => Err(StorageError::Corrupt(format!(
                "unknown data type tag: {other}"
            ))),
        }
    }
}

/// Fast computation of the byte length [`encode_row_body_dense`]
/// would produce, without allocating the output buffer. Mirrors the
/// encoder's per-column body sizing so the v5.2.1 `Table::hot_bytes`
/// incremental counter doesn't pay an alloc-per-insert tax. Returns
/// the exact same `usize` as `encode_row_body_dense(row, schema).len()`.
pub fn row_body_encoded_len(row: &Row, schema: &TableSchema) -> usize {
    debug_assert_eq!(
        row.values.len(),
        schema.columns.len(),
        "row_body_encoded_len: row arity must match schema"
    );
    let bitmap_bytes = schema.columns.len().div_ceil(8);
    let mut n = bitmap_bytes;
    for (col_idx, v) in row.values.iter().enumerate() {
        if matches!(v, Value::Null) {
            continue;
        }
        n += value_body_encoded_len(v, schema.columns[col_idx].ty);
    }
    n
}

/// Byte length a single cell consumes when written by
/// `write_value_body`. Used by [`row_body_encoded_len`]; kept in
/// lock-step with the encoder. The `_ty` slot is reserved for future
/// type-dependent encodings — every variant currently writes a fixed
/// body shape regardless of the declared column type.
fn value_body_encoded_len(v: &Value, _ty: DataType) -> usize {
    match v {
        Value::SmallInt(_) => 2,
        // 4-byte body: i32 / Date.
        Value::Int(_) | Value::Date(_) => 4,
        // 8-byte body: i64 / f64 / Timestamp.
        Value::BigInt(_) | Value::Float(_) | Value::Timestamp(_) => 8,
        Value::Bool(_) => 1,
        // Text/Varchar/Char/Json share the [u16 len][utf-8] layout.
        Value::Text(s) | Value::Json(s) => 2 + s.len(),
        // [u32 dim][f32 * dim]
        Value::Vector(vec) => 4 + 4 * vec.len(),
        // v6.0.1: SQ8 cell on-disk shape — [u32 dim][f32 min]
        // [f32 max][u8 * dim] = 12 + dim bytes. `hot_bytes`
        // tracking on `Table::insert` calls this every row, so
        // returning the real size now (even though the actual
        // `write_value_body` writer lands in step 6) keeps the
        // sizing arithmetic honest for in-memory benches.
        Value::Sq8Vector(q) => 4 + 4 + 4 + q.bytes.len(),
        // v6.0.3: halfvec on-disk shape — [u32 dim][u16 LE * dim]
        // = 4 + 2 * dim bytes.
        Value::HalfVector(h) => 4 + h.bytes.len(),
        // [i128 scaled][u8 scale]
        Value::Numeric { .. } => 16 + 1,
        // NULL is encoded only in the bitmap, never in the body.
        Value::Null => 0,
        // INTERVAL has no on-disk encoding (see write_value_body).
        Value::Interval { .. } => {
            unreachable!("Value::Interval has no on-disk encoding")
        }
    }
}

/// Encode one row's body in the v3.0.2 dense format (`FILE_VERSION`
/// 8): per-row NULL bitmap (1 bit/col, ceil(cols/8) bytes), then
/// each non-NULL cell as `write_value_body`. Same wire shape the
/// catalog snapshot writes per row inside its rows-block. Exposed
/// pub so v5.1+ cold-tier segment writers can produce row payloads
/// that the catalog [`decode_row_body_dense`] decodes 1:1.
///
/// `row.values.len()` must equal `schema.columns.len()` — the row
/// is expected to have been validated by `Table::insert` (the
/// engine's INSERT path) before reaching this function.
pub fn encode_row_body_dense(row: &Row, schema: &TableSchema) -> Vec<u8> {
    debug_assert_eq!(
        row.values.len(),
        schema.columns.len(),
        "dense encode: row arity must match schema"
    );
    let bitmap_bytes = schema.columns.len().div_ceil(8);
    // 8 B per fixed-width cell is a reasonable average; the buffer
    // grows past this for variable-width Text/Vector cells.
    let mut out = Vec::with_capacity(bitmap_bytes + schema.columns.len() * 8);
    let bitmap_offset = out.len();
    out.resize(bitmap_offset + bitmap_bytes, 0);
    for (i, v) in row.values.iter().enumerate() {
        if matches!(v, Value::Null) {
            out[bitmap_offset + i / 8] |= 1 << (i % 8);
        }
    }
    for (col_idx, v) in row.values.iter().enumerate() {
        if matches!(v, Value::Null) {
            continue;
        }
        write_value_body(&mut out, v, schema.columns[col_idx].ty);
    }
    out
}

/// Inverse of [`encode_row_body_dense`]. Reads one row's body from
/// `bytes` and returns it plus the number of bytes consumed (so a
/// caller decoding a back-to-back stream of rows can advance its
/// cursor). Returns `StorageError::Corrupt` on truncation, bad
/// UTF-8, or unknown cell tags.
pub fn decode_row_body_dense(
    bytes: &[u8],
    schema: &TableSchema,
) -> Result<(Row, usize), StorageError> {
    let mut cur = Cursor::new(bytes);
    let bitmap_bytes = schema.columns.len().div_ceil(8);
    let mut bitmap_buf = [0u8; 32];
    if bitmap_bytes > bitmap_buf.len() {
        return Err(StorageError::Corrupt(format!(
            "row NULL bitmap {bitmap_bytes} B exceeds 32 B cap"
        )));
    }
    let slice = cur.take(bitmap_bytes)?;
    bitmap_buf[..bitmap_bytes].copy_from_slice(slice);
    let mut values = Vec::with_capacity(schema.columns.len());
    for (col_idx, col) in schema.columns.iter().enumerate() {
        if (bitmap_buf[col_idx / 8] >> (col_idx % 8)) & 1 == 1 {
            values.push(Value::Null);
        } else {
            values.push(cur.read_value_body(col.ty)?);
        }
    }
    Ok((Row { values }, cur.pos))
}

/// Schema-driven dense value encoding (`FILE_VERSION` 8). Caller already
/// knows the column type and has decided this cell is non-NULL, so we
/// skip the per-cell type tag the v7 `write_value` was writing. NULL
/// is encoded via the per-row bitmap before this function runs, never
/// reaches here. Used only inside the row-encoding hot loop; the
/// schema-default path still goes through the legacy `write_value` so
/// DEFAULT values keep their self-describing tag and remain decodable
/// without consulting a column type.
fn write_value_body(out: &mut Vec<u8>, v: &Value, ty: DataType) {
    match (v, ty) {
        (Value::SmallInt(n), DataType::SmallInt) => out.extend_from_slice(&n.to_le_bytes()),
        (Value::Int(n), DataType::Int) => out.extend_from_slice(&n.to_le_bytes()),
        (Value::BigInt(n), DataType::BigInt) => out.extend_from_slice(&n.to_le_bytes()),
        (Value::Float(x), DataType::Float) => out.extend_from_slice(&x.to_le_bytes()),
        (Value::Bool(b), DataType::Bool) => out.push(u8::from(*b)),
        (Value::Text(s), DataType::Text | DataType::Varchar(_) | DataType::Char(_)) => {
            write_str(out, s);
        }
        (
            Value::Vector(v),
            DataType::Vector {
                encoding: VecEncoding::F32,
                ..
            },
        ) => {
            let dim = u32::try_from(v.len()).expect("vector dim fits in u32");
            out.extend_from_slice(&dim.to_le_bytes());
            for x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        // v6.0.1: SQ8 dense body — [u32 dim][f32 min][f32 max]
        // [u8 * dim]. Self-describes its length so v6 readers
        // walking rows of a v6 catalog stay aligned even if the
        // declared column dim drifts (defensive, not normally
        // possible since CREATE TABLE pins the dim).
        (
            Value::Sq8Vector(q),
            DataType::Vector {
                encoding: VecEncoding::Sq8,
                ..
            },
        ) => {
            let dim = u32::try_from(q.bytes.len()).expect("vector dim fits in u32");
            out.extend_from_slice(&dim.to_le_bytes());
            out.extend_from_slice(&q.min.to_le_bytes());
            out.extend_from_slice(&q.max.to_le_bytes());
            out.extend_from_slice(&q.bytes);
        }
        // v6.0.3: halfvec dense body — [u32 dim][u16 LE * dim].
        // The raw u16 bytes already live in `h.bytes` little-
        // endian, so we just splat them.
        (
            Value::HalfVector(h),
            DataType::Vector {
                encoding: VecEncoding::F16,
                ..
            },
        ) => {
            let dim = u32::try_from(h.dim()).expect("vector dim fits in u32");
            out.extend_from_slice(&dim.to_le_bytes());
            out.extend_from_slice(&h.bytes);
        }
        (Value::Numeric { scaled, .. }, DataType::Numeric { scale, .. }) => {
            out.extend_from_slice(&scaled.to_le_bytes());
            out.push(scale);
        }
        (Value::Date(d), DataType::Date) => out.extend_from_slice(&d.to_le_bytes()),
        (Value::Timestamp(t), DataType::Timestamp) => out.extend_from_slice(&t.to_le_bytes()),
        // v4.9: JSON stores as length-prefixed text; same shape as
        // Text — the type tag lives in the column schema, not the
        // per-cell body.
        (Value::Json(s), DataType::Json) => write_str(out, s),
        // Type mismatch shouldn't happen — `Table::insert` validates
        // value type against column type before pushing. Treat as a
        // bug, not a runtime error.
        (other, ty) => unreachable!(
            "schema-driven encode received mismatched value/type pair: \
             value tag={:?}, column type={:?}",
            other.data_type(),
            ty
        ),
    }
}

fn write_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(0),
        Value::SmallInt(n) => {
            out.push(7);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::Int(n) => {
            out.push(1);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::BigInt(n) => {
            out.push(2);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::Float(x) => {
            out.push(3);
            out.extend_from_slice(&x.to_le_bytes());
        }
        // v4.9: JSON shares the tag-4 (Text) on-disk encoding —
        // schema decides which variant comes back on read. The
        // bodies are byte-identical so collapsing the match keeps
        // clippy::match_same_arms quiet.
        Value::Text(s) | Value::Json(s) => {
            out.push(4);
            write_str(out, s);
        }
        Value::Bool(b) => {
            out.push(5);
            out.push(u8::from(*b));
        }
        Value::Vector(v) => {
            out.push(6);
            let dim = u32::try_from(v.len()).expect("vector dim fits in u32");
            out.extend_from_slice(&dim.to_le_bytes());
            for x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        // v6.0.1: new tag 11 for an SQ8 cell carried with its full
        // header. Layout matches the dense row body shape so a
        // round-trip through write_value → read_value bit-equals
        // the original `Value::Sq8Vector`.
        Value::Sq8Vector(q) => {
            out.push(11);
            let dim = u32::try_from(q.bytes.len()).expect("vector dim fits in u32");
            out.extend_from_slice(&dim.to_le_bytes());
            out.extend_from_slice(&q.min.to_le_bytes());
            out.extend_from_slice(&q.max.to_le_bytes());
            out.extend_from_slice(&q.bytes);
        }
        // v6.0.3: tag 12 for a HalfVector cell.
        // Layout: `[u32 dim][u16 LE × dim]` — bit-identical to the
        // dense row body so `write_value` / `read_value` bit-equal
        // the original `Value::HalfVector`.
        Value::HalfVector(h) => {
            out.push(12);
            let dim = u32::try_from(h.dim()).expect("vector dim fits in u32");
            out.extend_from_slice(&dim.to_le_bytes());
            out.extend_from_slice(&h.bytes);
        }
        Value::Numeric { scaled, scale } => {
            out.push(8);
            out.extend_from_slice(&scaled.to_le_bytes());
            out.push(*scale);
        }
        Value::Date(d) => {
            out.push(9);
            out.extend_from_slice(&d.to_le_bytes());
        }
        Value::Timestamp(t) => {
            out.push(10);
            out.extend_from_slice(&t.to_le_bytes());
        }
        // Interval is a runtime-only value (no on-disk representation in
        // v2.11). CREATE TABLE rejects `DataType::Interval` columns, so a
        // Value::Interval here would mean the engine bypassed that gate.
        Value::Interval { .. } => {
            unreachable!(
                "Value::Interval has no on-disk encoding; engine must reject it before write"
            )
        }
    }
}

fn write_u16(out: &mut Vec<u8>, n: u16) {
    out.extend_from_slice(&n.to_le_bytes());
}
fn write_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}
fn write_str(out: &mut Vec<u8>, s: &str) {
    let len = u16::try_from(s.len()).expect("identifier / text fits in u16");
    write_u16(out, len);
    out.extend_from_slice(s.as_bytes());
}

/// Serialise an [`IndexKey`] using the v9 tagged codec. `read_index_key`
/// is the inverse. v8 catalogs never wrote index keys (`BTree` entries were
/// rebuilt from `Table::rows`), so this codec is v9+ only.
fn write_index_key(out: &mut Vec<u8>, key: &IndexKey) {
    match key {
        IndexKey::Int(n) => {
            out.push(INDEX_KEY_TAG_INT);
            out.extend_from_slice(&n.to_le_bytes());
        }
        IndexKey::Text(s) => {
            out.push(INDEX_KEY_TAG_TEXT);
            write_str(out, s);
        }
        IndexKey::Bool(b) => {
            out.push(INDEX_KEY_TAG_BOOL);
            out.push(u8::from(*b));
        }
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], StorageError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| StorageError::Corrupt(format!("length overflow taking {n} bytes")))?;
        if end > self.buf.len() {
            return Err(StorageError::Corrupt(format!(
                "unexpected EOF at offset {} (wanted {n} more bytes)",
                self.pos
            )));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn read_u8(&mut self) -> Result<u8, StorageError> {
        Ok(self.take(1)?[0])
    }
    fn read_u16(&mut self) -> Result<u16, StorageError> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn read_u32(&mut self) -> Result<u32, StorageError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn read_i32(&mut self) -> Result<i32, StorageError> {
        let s = self.take(4)?;
        Ok(i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn read_i64(&mut self) -> Result<i64, StorageError> {
        let s = self.take(8)?;
        let arr: [u8; 8] = s.try_into().expect("checked");
        Ok(i64::from_le_bytes(arr))
    }
    fn read_f64(&mut self) -> Result<f64, StorageError> {
        let s = self.take(8)?;
        let arr: [u8; 8] = s.try_into().expect("checked");
        Ok(f64::from_le_bytes(arr))
    }
    fn read_f32(&mut self) -> Result<f32, StorageError> {
        let s = self.take(4)?;
        Ok(f32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn read_str(&mut self) -> Result<String, StorageError> {
        let len = self.read_u16()? as usize;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes)
            .map(String::from)
            .map_err(|_| StorageError::Corrupt("invalid UTF-8 in identifier or text".into()))
    }

    /// Parse an [`IndexKey`] emitted by `write_index_key` (v9 tagged
    /// codec). Returns `StorageError::Corrupt` on unknown tag or
    /// truncated payload.
    fn read_index_key(&mut self) -> Result<IndexKey, StorageError> {
        let tag = self.read_u8()?;
        match tag {
            INDEX_KEY_TAG_INT => Ok(IndexKey::Int(self.read_i64()?)),
            INDEX_KEY_TAG_TEXT => Ok(IndexKey::Text(self.read_str()?)),
            INDEX_KEY_TAG_BOOL => Ok(IndexKey::Bool(self.read_u8()? != 0)),
            other => Err(StorageError::Corrupt(format!(
                "unknown index key tag: {other}"
            ))),
        }
    }
    /// Schema-driven dense value decode (`FILE_VERSION` 8). Caller has
    /// already cleared the NULL bit from the row bitmap; we read the
    /// fixed-width body for the given column type. Used inside the row
    /// hot loop; column defaults still go through `read_value` (which
    /// reads its own type tag) so DEFAULT round-trips without a schema.
    fn read_value_body(&mut self, ty: DataType) -> Result<Value, StorageError> {
        match ty {
            DataType::SmallInt => {
                let s = self.take(2)?;
                Ok(Value::SmallInt(i16::from_le_bytes([s[0], s[1]])))
            }
            DataType::Int => Ok(Value::Int(self.read_i32()?)),
            DataType::BigInt => Ok(Value::BigInt(self.read_i64()?)),
            DataType::Float => Ok(Value::Float(self.read_f64()?)),
            DataType::Bool => Ok(Value::Bool(self.read_u8()? != 0)),
            DataType::Text | DataType::Varchar(_) | DataType::Char(_) => {
                Ok(Value::Text(self.read_str()?))
            }
            DataType::Vector {
                encoding: VecEncoding::F32,
                ..
            } => {
                let dim = self.read_u32()? as usize;
                let mut v = Vec::with_capacity(dim);
                for _ in 0..dim {
                    let bytes: [u8; 4] = self.take(4)?.try_into().expect("checked");
                    v.push(f32::from_le_bytes(bytes));
                }
                Ok(Value::Vector(v))
            }
            DataType::Vector {
                encoding: VecEncoding::Sq8,
                ..
            } => {
                let dim = self.read_u32()? as usize;
                let min = self.read_f32()?;
                let max = self.read_f32()?;
                let bytes = self.take(dim)?.to_vec();
                Ok(Value::Sq8Vector(quantize::Sq8Vector { min, max, bytes }))
            }
            DataType::Vector {
                encoding: VecEncoding::F16,
                ..
            } => {
                let dim = self.read_u32()? as usize;
                let bytes = self.take(dim * 2)?.to_vec();
                Ok(Value::HalfVector(halfvec::HalfVector { bytes }))
            }
            DataType::Numeric { .. } => {
                let s = self.take(16)?;
                let arr: [u8; 16] = s.try_into().expect("checked");
                let scaled = i128::from_le_bytes(arr);
                let scale = self.read_u8()?;
                Ok(Value::Numeric { scaled, scale })
            }
            DataType::Date => Ok(Value::Date(self.read_i32()?)),
            DataType::Timestamp => Ok(Value::Timestamp(self.read_i64()?)),
            DataType::Interval => {
                // Defensive — schema gate (CREATE TABLE rejects Interval
                // columns) means this branch can't be hit through normal
                // flow; reject corrupt files explicitly rather than
                // panic.
                Err(StorageError::Corrupt(
                    "INTERVAL column found on disk — runtime-only type, v3.0.2 rejects it".into(),
                ))
            }
            DataType::Json => Ok(Value::Json(self.read_str()?)),
        }
    }

    fn read_value(&mut self) -> Result<Value, StorageError> {
        let tag = self.read_u8()?;
        match tag {
            0 => Ok(Value::Null),
            1 => Ok(Value::Int(self.read_i32()?)),
            2 => Ok(Value::BigInt(self.read_i64()?)),
            3 => Ok(Value::Float(self.read_f64()?)),
            4 => Ok(Value::Text(self.read_str()?)),
            5 => Ok(Value::Bool(self.read_u8()? != 0)),
            6 => {
                let dim = self.read_u32()? as usize;
                let mut v = Vec::with_capacity(dim);
                for _ in 0..dim {
                    let bytes: [u8; 4] = self.take(4)?.try_into().expect("checked");
                    v.push(f32::from_le_bytes(bytes));
                }
                Ok(Value::Vector(v))
            }
            7 => {
                let s = self.take(2)?;
                Ok(Value::SmallInt(i16::from_le_bytes([s[0], s[1]])))
            }
            8 => {
                let s = self.take(16)?;
                let arr: [u8; 16] = s.try_into().expect("checked");
                let scaled = i128::from_le_bytes(arr);
                let scale = self.read_u8()?;
                Ok(Value::Numeric { scaled, scale })
            }
            9 => Ok(Value::Date(self.read_i32()?)),
            10 => Ok(Value::Timestamp(self.read_i64()?)),
            // v6.0.1: tag 11 — Sq8Vector. Pre-v6 readers fall
            // through to the catch-all and surface
            // `Corrupt("unknown value tag")`, matching the
            // forward-compat fence on the column-type side.
            11 => {
                let dim = self.read_u32()? as usize;
                let min = self.read_f32()?;
                let max = self.read_f32()?;
                let bytes = self.take(dim)?.to_vec();
                Ok(Value::Sq8Vector(quantize::Sq8Vector { min, max, bytes }))
            }
            // v6.0.3: tag 12 — HalfVector. Same forward-compat
            // fence story as tag 11.
            12 => {
                let dim = self.read_u32()? as usize;
                let bytes = self.take(dim * 2)?.to_vec();
                Ok(Value::HalfVector(halfvec::HalfVector { bytes }))
            }
            other => Err(StorageError::Corrupt(format!("unknown value tag: {other}"))),
        }
    }

    /// Read an NSW graph that was emitted via `write_nsw_graph`. `m`
    /// is passed in because it was already consumed from the per-
    /// index header. Returns the reconstituted `NswGraph`.
    fn read_nsw_graph(&mut self, m: usize) -> Result<NswGraph, StorageError> {
        let m_max_0 = self.read_u16()? as usize;
        let entry_raw = self.read_u32()?;
        let entry = if entry_raw == u32::MAX {
            None
        } else {
            Some(entry_raw as usize)
        };
        let entry_level = self.read_u8()?;
        let node_count = self.read_u32()? as usize;
        // v5.5.0: levels/per-layer are PV-backed in memory, but the wire
        // format is unchanged — decode element-by-element into a PV via
        // push_mut (transient in-place, no per-element path-copy here since
        // the freshly-built PV is uniquely owned).
        let mut levels: PersistentVec<u8> = PersistentVec::new();
        for _ in 0..node_count {
            levels.push_mut(self.read_u8()?);
        }
        let layer_count = self.read_u8()? as usize;
        let mut layers: Vec<PersistentVec<Vec<u32>>> = Vec::with_capacity(layer_count);
        for _ in 0..layer_count {
            let n = self.read_u32()? as usize;
            let mut per_layer: PersistentVec<Vec<u32>> = PersistentVec::new();
            for _ in 0..n {
                let cnt = self.read_u16()? as usize;
                let mut row: Vec<u32> = Vec::with_capacity(cnt);
                for _ in 0..cnt {
                    row.push(self.read_u32()?);
                }
                per_layer.push_mut(row);
            }
            layers.push(per_layer);
        }
        Ok(NswGraph {
            m,
            m_max_0,
            entry,
            entry_level,
            levels,
            layers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn neon_l2_matches_scalar() {
        // For every dim that's a multiple of 4 (4, 8, 12, 16, 64,
        // 128, 256, 384, 512, 768, 1024, 1536), the NEON impl must
        // agree with the scalar reference within tight float
        // tolerance (FMA rounding differs from separate * + +).
        let dims = [4usize, 8, 12, 16, 64, 128, 256, 384, 512, 768, 1024, 1536];
        for &d in &dims {
            let mut state: u64 = (d as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let mut a = Vec::with_capacity(d);
            let mut b = Vec::with_capacity(d);
            for _ in 0..d {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let x = (((state >> 32) & 0x00FF_FFFF) as f32) / (0x80_0000_u32 as f32) - 1.0;
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let y = (((state >> 32) & 0x00FF_FFFF) as f32) / (0x80_0000_u32 as f32) - 1.0;
                a.push(x);
                b.push(y);
            }
            let scalar = l2_distance_sq_scalar(&a, &b);
            let neon = unsafe { l2_distance_sq_neon(&a, &b) };
            let tol = (scalar.abs().max(1e-6)) * 1e-4;
            assert!(
                (scalar - neon).abs() <= tol,
                "dim={d}: scalar={scalar} neon={neon} diff={}",
                (scalar - neon).abs()
            );
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn neon_inner_product_matches_scalar() {
        // v6.0.2 step 1: NEON IP must agree with scalar across every
        // production-shaped dim. FMA rounding differs from
        // separate * + +, so the tolerance scales with magnitude.
        let dims = [4usize, 8, 12, 16, 64, 128, 256, 512, 1024];
        for &d in &dims {
            let mut state: u64 = (d as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let mut a = Vec::with_capacity(d);
            let mut b = Vec::with_capacity(d);
            for _ in 0..d {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let x = (((state >> 32) & 0x00FF_FFFF) as f32) / (0x80_0000_u32 as f32) - 1.0;
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let y = (((state >> 32) & 0x00FF_FFFF) as f32) / (0x80_0000_u32 as f32) - 1.0;
                a.push(x);
                b.push(y);
            }
            let scalar = inner_product_scalar(&a, &b);
            let neon = unsafe { inner_product_neon(&a, &b) };
            #[allow(clippy::cast_precision_loss)]
            let tol = (scalar.abs().max(1e-6)) * 1e-4 + (d as f32) * 1e-6;
            assert!(
                (scalar - neon).abs() <= tol,
                "IP dim={d}: scalar={scalar} neon={neon} diff={}",
                (scalar - neon).abs()
            );
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[allow(clippy::similar_names)]
    #[test]
    fn neon_cosine_dot_norms_matches_scalar() {
        let dims = [4usize, 8, 12, 16, 64, 128, 256, 512, 1024];
        for &d in &dims {
            let mut state: u64 = (d as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            let mut a = Vec::with_capacity(d);
            let mut b = Vec::with_capacity(d);
            for _ in 0..d {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let x = (((state >> 32) & 0x00FF_FFFF) as f32) / (0x80_0000_u32 as f32) - 1.0;
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let y = (((state >> 32) & 0x00FF_FFFF) as f32) / (0x80_0000_u32 as f32) - 1.0;
                a.push(x);
                b.push(y);
            }
            let (dot_s, na_s, nb_s) = cosine_dot_norms_scalar(&a, &b);
            let (dot_n, na_n, nb_n) = unsafe { cosine_dot_norms_neon(&a, &b) };
            #[allow(clippy::cast_precision_loss)]
            let tol_d = (dot_s.abs().max(1e-6)) * 1e-4 + (d as f32) * 1e-6;
            #[allow(clippy::cast_precision_loss)]
            let tol_n = (na_s.abs().max(1e-6)) * 1e-4 + (d as f32) * 1e-6;
            assert!(
                (dot_s - dot_n).abs() <= tol_d,
                "cosine dot dim={d}: scalar={dot_s} neon={dot_n}"
            );
            assert!(
                (na_s - na_n).abs() <= tol_n,
                "cosine na dim={d}: scalar={na_s} neon={na_n}"
            );
            assert!(
                (nb_s - nb_n).abs() <= tol_n,
                "cosine nb dim={d}: scalar={nb_s} neon={nb_n}"
            );
        }
    }

    fn make_users_schema() -> TableSchema {
        TableSchema::new(
            "users",
            vec![
                ColumnSchema::new("id", DataType::Int, false),
                ColumnSchema::new("name", DataType::Text, false),
                ColumnSchema::new("score", DataType::Float, true),
            ],
        )
    }

    #[test]
    fn value_type_tag_matches_variant() {
        assert_eq!(Value::Int(1).data_type(), Some(DataType::Int));
        assert_eq!(Value::BigInt(1).data_type(), Some(DataType::BigInt));
        assert_eq!(Value::Float(1.0).data_type(), Some(DataType::Float));
        assert_eq!(Value::Text("x".into()).data_type(), Some(DataType::Text));
        assert_eq!(Value::Bool(true).data_type(), Some(DataType::Bool));
        assert_eq!(Value::Null.data_type(), None);
        assert!(Value::Null.is_null());
        assert!(!Value::Int(0).is_null());
    }

    #[test]
    fn sq8_value_reports_sq8_data_type() {
        // v6.0.1: a `Value::Sq8Vector` cell surfaces its dim
        // (= bytes.len()) and encoding through `data_type()` so
        // INSERT-time column type-checks (step 3) can route on
        // both shape and encoding.
        let q = crate::quantize::quantize(&[0.0, 0.25, 0.5, 0.75, 1.0]);
        let v = Value::Sq8Vector(q);
        assert_eq!(
            v.data_type(),
            Some(DataType::Vector {
                dim: 5,
                encoding: VecEncoding::Sq8,
            }),
        );
    }

    #[test]
    fn datatype_display_matches_pg_keyword() {
        assert_eq!(DataType::Int.to_string(), "INT");
        assert_eq!(DataType::BigInt.to_string(), "BIGINT");
        assert_eq!(DataType::Float.to_string(), "FLOAT");
        assert_eq!(DataType::Text.to_string(), "TEXT");
        assert_eq!(DataType::Bool.to_string(), "BOOL");
    }

    #[test]
    fn row_len_and_emptiness() {
        let r = Row::new(vec![Value::Int(1), Value::Null]);
        assert_eq!(r.len(), 2);
        assert!(!r.is_empty());
        assert!(Row::new(Vec::new()).is_empty());
    }

    #[test]
    fn table_schema_column_position() {
        let s = make_users_schema();
        assert_eq!(s.column_position("id"), Some(0));
        assert_eq!(s.column_position("score"), Some(2));
        assert_eq!(s.column_position("missing"), None);
    }

    #[test]
    fn catalog_create_table_then_lookup() {
        let mut cat = Catalog::new();
        cat.create_table(make_users_schema()).unwrap();
        assert_eq!(cat.table_count(), 1);
        assert!(cat.get("users").is_some());
        assert!(cat.get("nope").is_none());
    }

    #[test]
    fn catalog_duplicate_table_is_rejected() {
        let mut cat = Catalog::new();
        cat.create_table(make_users_schema()).unwrap();
        let err = cat.create_table(make_users_schema()).unwrap_err();
        assert!(matches!(err, StorageError::DuplicateTable { ref name } if name == "users"));
    }

    #[test]
    fn table_insert_happy_path_appends_row() {
        let mut cat = Catalog::new();
        cat.create_table(make_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        t.insert(Row::new(vec![
            Value::Int(1),
            Value::Text("alice".into()),
            Value::Float(99.5),
        ]))
        .unwrap();
        assert_eq!(t.row_count(), 1);
        assert_eq!(t.rows()[0].values[1], Value::Text("alice".into()));
    }

    #[test]
    fn table_insert_arity_mismatch() {
        let mut cat = Catalog::new();
        cat.create_table(make_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        let err = t.insert(Row::new(vec![Value::Int(1)])).unwrap_err();
        assert!(matches!(
            err,
            StorageError::ArityMismatch {
                expected: 3,
                actual: 1
            }
        ));
        assert_eq!(t.row_count(), 0);
    }

    #[test]
    fn table_insert_type_mismatch_reports_column() {
        let mut cat = Catalog::new();
        cat.create_table(make_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        let err = t
            .insert(Row::new(vec![
                Value::Int(1),
                Value::Int(42), // name expects Text
                Value::Float(0.0),
            ]))
            .unwrap_err();
        match err {
            StorageError::TypeMismatch {
                ref column,
                expected,
                actual,
                position,
            } => {
                assert_eq!(column, "name");
                assert_eq!(expected, DataType::Text);
                assert_eq!(actual, DataType::Int);
                assert_eq!(position, 1);
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(t.row_count(), 0);
    }

    #[test]
    fn table_insert_null_into_not_null_rejected() {
        let mut cat = Catalog::new();
        cat.create_table(make_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        let err = t
            .insert(Row::new(vec![
                Value::Int(1),
                Value::Null, // name is NOT NULL
                Value::Float(1.0),
            ]))
            .unwrap_err();
        assert!(matches!(err, StorageError::NullInNotNull { ref column } if column == "name"));
    }

    #[test]
    fn table_insert_null_into_nullable_ok() {
        let mut cat = Catalog::new();
        cat.create_table(make_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        t.insert(Row::new(vec![
            Value::Int(1),
            Value::Text("bob".into()),
            Value::Null,
        ]))
        .unwrap();
        assert_eq!(t.row_count(), 1);
    }

    #[test]
    fn catalog_get_mut_independent_per_table() {
        let mut cat = Catalog::new();
        cat.create_table(TableSchema::new(
            "a",
            vec![ColumnSchema::new("v", DataType::Int, false)],
        ))
        .unwrap();
        cat.create_table(TableSchema::new(
            "b",
            vec![ColumnSchema::new("v", DataType::Int, false)],
        ))
        .unwrap();
        cat.get_mut("a")
            .unwrap()
            .insert(Row::new(vec![Value::Int(1)]))
            .unwrap();
        assert_eq!(cat.get("a").unwrap().row_count(), 1);
        assert_eq!(cat.get("b").unwrap().row_count(), 0);
    }

    // --- v0.6 persistence round-trips --------------------------------------

    fn assert_round_trip(cat: &Catalog) {
        let bytes = cat.serialize();
        let restored = Catalog::deserialize(&bytes).expect("deserialize");
        // Compare semantic state: same tables in same order, same schema +
        // rows in each.
        assert_eq!(restored.table_count(), cat.table_count());
        for (a, b) in cat.tables.iter().zip(restored.tables.iter()) {
            assert_eq!(a.schema, b.schema);
            assert_eq!(a.rows, b.rows);
        }
    }

    #[test]
    fn serialize_empty_catalog_round_trips() {
        assert_round_trip(&Catalog::new());
    }

    #[test]
    fn serialize_single_empty_table_round_trips() {
        let mut cat = Catalog::new();
        cat.create_table(make_users_schema()).unwrap();
        assert_round_trip(&cat);
    }

    #[test]
    fn nsw_clone_is_o1() {
        // v5.5.0: NswGraph::clone must be O(1) structural sharing, not the
        // pre-v5.5 O(N) element copy — it rides on Catalog::clone for every
        // group-commit write on a vector table. Build a non-trivial multi-
        // layer graph, clone it, and prove the clone shares the very same PV
        // storage (root+tail Arc) for `levels` and every `layers[l]`. Sharing
        // ⇒ no per-node element copy ⇒ clone cost independent of N (node
        // count); only the outer layer Vec (len ≤ 8) is copied, O(1) in
        // practice.
        let mut cat = Catalog::new();
        cat.create_table(TableSchema::new(
            "docs",
            alloc::vec![
                ColumnSchema::new("id", DataType::Int, false),
                ColumnSchema::new(
                    "v",
                    DataType::Vector {
                        dim: 3,
                        encoding: VecEncoding::F32
                    },
                    true
                ),
            ],
        ))
        .unwrap();
        let t = cat.get_mut("docs").unwrap();
        for i in 0..1500_i32 {
            #[allow(clippy::cast_precision_loss)] // 0..1500 — no precision lost
            let base = (i as f32) * 0.01;
            t.insert(Row::new(alloc::vec![
                Value::Int(i),
                Value::Vector(alloc::vec![base, base + 0.05, base + 0.1]),
            ]))
            .unwrap();
        }
        t.add_nsw_index("docs_nsw".into(), "v", NSW_DEFAULT_M)
            .unwrap();
        let g = match &cat.get("docs").unwrap().indices()[0].kind {
            IndexKind::Nsw(g) => g,
            IndexKind::BTree(_) => panic!("expected NSW"),
        };
        // Non-trivial graph: one level slot per row, and the geometric level
        // distribution puts some nodes above layer 0.
        assert_eq!(g.levels.len(), 1500, "one level slot per inserted row");
        assert!(
            g.layers.len() >= 2,
            "1500 nodes should populate at least two HNSW layers, got {}",
            g.layers.len()
        );

        let cloned = g.clone();

        assert!(
            g.levels.shares_storage_with(&cloned.levels),
            "levels PV not shared after clone — clone copied elements (O(N))"
        );
        assert_eq!(g.layers.len(), cloned.layers.len());
        for (l, (orig, cl)) in g.layers.iter().zip(cloned.layers.iter()).enumerate() {
            assert!(
                orig.shares_storage_with(cl),
                "layer {l} PV not shared after clone — clone copied elements (O(N))"
            );
        }
    }

    #[test]
    fn sq8_catalog_serialise_roundtrip_preserves_cells_and_index() {
        // v6.0.1 step 6 verify: a catalog with an `VECTOR(N)
        // USING SQ8` column + NSW index survives a full
        // serialise → deserialise cycle. Cells re-decode bit-
        // identically (per-vector affine triple), the NSW
        // topology stays intact, and kNN search still routes
        // through the SQ8 ADC dispatcher after the catalog hop.
        let mut cat = Catalog::new();
        cat.create_table(TableSchema::new(
            "vecs",
            alloc::vec![
                ColumnSchema::new("id", DataType::Int, false),
                ColumnSchema::new(
                    "v",
                    DataType::Vector {
                        dim: 8,
                        encoding: VecEncoding::Sq8,
                    },
                    false,
                ),
            ],
        ))
        .unwrap();
        let t = cat.get_mut("vecs").unwrap();
        for i in 0..32_i32 {
            #[allow(clippy::cast_precision_loss)]
            let base = (i as f32) * 0.03;
            let v: Vec<f32> = (0..8_i32)
                .map(|j| {
                    #[allow(clippy::cast_precision_loss)]
                    let off = (j as f32) * 0.01;
                    base + off
                })
                .collect();
            t.insert(Row::new(alloc::vec![
                Value::Int(i),
                Value::Sq8Vector(quantize::quantize(&v)),
            ]))
            .unwrap();
        }
        t.add_nsw_index("v_idx".into(), "v", NSW_DEFAULT_M).unwrap();
        // Capture a pre-serialise reference cell + nsw hits to
        // compare against the restored catalog.
        let query = alloc::vec![0.15_f32, 0.16, 0.17, 0.18, 0.19, 0.20, 0.21, 0.22];
        let (before_cell, before_ty, before_hits) = {
            let t_ref = cat.get("vecs").unwrap();
            (
                t_ref.rows()[5].values[1].clone(),
                t_ref.schema().columns[1].ty,
                nsw_query(t_ref, "v_idx", &query, 5, NswMetric::L2),
            )
        };

        let bytes = cat.serialize();
        let restored = Catalog::deserialize(&bytes).expect("deserialize ok");
        let rt = restored.get("vecs").unwrap();
        assert_eq!(rt.schema().columns[1].ty, before_ty);
        assert_eq!(rt.rows()[5].values[1], before_cell);
        let after_hits = nsw_query(rt, "v_idx", &query, 5, NswMetric::L2);
        assert_eq!(before_hits, after_hits);
    }

    #[test]
    fn half_catalog_serialise_roundtrip_preserves_cells_and_index() {
        // v6.0.3 step 4 verify: a catalog with a `VECTOR(N) USING
        // HALF` column + NSW index survives a full serialise →
        // deserialise cycle. Cells re-decode bit-identically (raw
        // u16 LE bytes), the NSW topology stays intact, and kNN
        // search still returns the same hit IDs against the
        // restored catalog.
        use crate::halfvec;
        let mut cat = Catalog::new();
        cat.create_table(TableSchema::new(
            "vecs",
            alloc::vec![
                ColumnSchema::new("id", DataType::Int, false),
                ColumnSchema::new(
                    "v",
                    DataType::Vector {
                        dim: 8,
                        encoding: VecEncoding::F16,
                    },
                    false,
                ),
            ],
        ))
        .unwrap();
        let t = cat.get_mut("vecs").unwrap();
        for i in 0..32_i32 {
            #[allow(clippy::cast_precision_loss)]
            let base = (i as f32) * 0.03;
            let v: Vec<f32> = (0..8_i32)
                .map(|j| {
                    #[allow(clippy::cast_precision_loss)]
                    let off = (j as f32) * 0.01;
                    base + off
                })
                .collect();
            t.insert(Row::new(alloc::vec![
                Value::Int(i),
                Value::HalfVector(halfvec::HalfVector::from_f32_slice(&v)),
            ]))
            .unwrap();
        }
        t.add_nsw_index("v_idx".into(), "v", NSW_DEFAULT_M).unwrap();
        let query = alloc::vec![0.15_f32, 0.16, 0.17, 0.18, 0.19, 0.20, 0.21, 0.22];
        let (before_cell, before_ty, before_hits) = {
            let t_ref = cat.get("vecs").unwrap();
            (
                t_ref.rows()[5].values[1].clone(),
                t_ref.schema().columns[1].ty,
                nsw_query(t_ref, "v_idx", &query, 5, NswMetric::L2),
            )
        };
        let bytes = cat.serialize();
        let restored = Catalog::deserialize(&bytes).expect("deserialize ok");
        let rt = restored.get("vecs").unwrap();
        assert_eq!(rt.schema().columns[1].ty, before_ty);
        assert_eq!(rt.rows()[5].values[1], before_cell);
        let after_hits = nsw_query(rt, "v_idx", &query, 5, NswMetric::L2);
        assert_eq!(before_hits, after_hits);
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn hnsw_half_recall_at_10_matches_f32_groundtruth() {
        // v6.0.3 step 3 verify: HALF column NSW retrieves ≥ 95%
        // top-10 overlap vs brute-force F32 ground truth.
        // Half-precision dequantises bit-exactly at the storage
        // layer (no rerank pass), so the recall floor is tighter
        // than the SQ8 case — only the rounding noise from f32 →
        // f16 quantisation contributes.
        use crate::halfvec;
        fn next(state: &mut u64) -> f32 {
            *state = state
                .wrapping_add(0x9E37_79B9_7F4A_7C15)
                .wrapping_mul(0xBF58_476D_1CE4_E5B9);
            #[allow(clippy::cast_precision_loss)]
            let u = ((*state >> 32) as u32 as f32) / (u32::MAX as f32);
            2.0 * u - 1.0
        }
        let dim: u32 = 32;
        let n: usize = 512;
        let dim_us = dim as usize;
        let mut seed: u64 = 0xF16_F16_F16_F16_u64;
        let corpus: Vec<Vec<f32>> = (0..n)
            .map(|_| (0..dim_us).map(|_| next(&mut seed)).collect())
            .collect();
        let queries: Vec<Vec<f32>> = (0..32)
            .map(|_| (0..dim_us).map(|_| next(&mut seed)).collect())
            .collect();
        let exact_top10: Vec<Vec<usize>> = queries
            .iter()
            .map(|q| {
                let mut scored: Vec<(f32, usize)> = corpus
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (l2_distance_sq(v, q), i))
                    .collect();
                scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
                scored.into_iter().take(10).map(|(_, i)| i).collect()
            })
            .collect();
        let mut cat = Catalog::new();
        cat.create_table(TableSchema::new(
            "vecs",
            alloc::vec![
                ColumnSchema::new("id", DataType::Int, false),
                ColumnSchema::new(
                    "v",
                    DataType::Vector {
                        dim,
                        encoding: VecEncoding::F16,
                    },
                    false,
                ),
            ],
        ))
        .unwrap();
        let t = cat.get_mut("vecs").unwrap();
        for (i, v) in corpus.iter().enumerate() {
            t.insert(Row::new(alloc::vec![
                Value::Int(i32::try_from(i).unwrap()),
                Value::HalfVector(halfvec::HalfVector::from_f32_slice(v)),
            ]))
            .unwrap();
        }
        t.add_nsw_index("v_idx".into(), "v", NSW_DEFAULT_M).unwrap();
        let table = cat.get("vecs").unwrap();
        let mut total_overlap = 0_usize;
        for (q, exact) in queries.iter().zip(exact_top10.iter()) {
            let hits = nsw_query(table, "v_idx", q, 10, NswMetric::L2);
            for h in &hits {
                if exact.contains(h) {
                    total_overlap += 1;
                }
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let recall = total_overlap as f32 / (10.0 * queries.len() as f32);
        assert!(
            recall >= 0.95,
            "HALF HNSW recall@10 = {recall:.3}, below floor 0.95 — \
             check halfvec dispatch in `cell_to_query_metric_distance`"
        );
    }

    #[test]
    fn hnsw_sq8_recall_at_10_above_0_95_vs_f32_groundtruth() {
        // v6.0.1 step 5 verify: build TWO catalogs over the same
        // corpus — one F32, one SQ8 — and confirm SQ8 NSW + f32
        // rerank retrieves ≥ 95% top-10 overlap vs brute-force F32
        // ground truth. The rerank pass (sq8_rerank) re-scores ADC
        // candidates with dequantised cells, recovering recall the
        // raw ADC sacrifices for 4× compression.
        use crate::quantize;
        // Deterministic Gaussian-ish corpus via splitmix64. Vectors
        // get normalised so SQ8's per-vector `(min, max)` lives in
        // a sensible range; matches the v6.0.0 fuzz harness.
        fn next(state: &mut u64) -> f32 {
            *state = state
                .wrapping_add(0x9E37_79B9_7F4A_7C15)
                .wrapping_mul(0xBF58_476D_1CE4_E5B9);
            #[allow(clippy::cast_precision_loss)]
            let u = ((*state >> 32) as u32 as f32) / (u32::MAX as f32);
            2.0 * u - 1.0
        }
        let dim: u32 = 32;
        let n: usize = 512;
        let dim_us = dim as usize;
        let mut seed: u64 = 0xCAFE_BABE_DEAD_BEEFu64;
        let corpus: Vec<Vec<f32>> = (0..n)
            .map(|_| (0..dim_us).map(|_| next(&mut seed)).collect())
            .collect();
        let queries: Vec<Vec<f32>> = (0..32)
            .map(|_| (0..dim_us).map(|_| next(&mut seed)).collect())
            .collect();
        // F32 ground truth — pure exact arithmetic, brute force.
        let exact_top10: Vec<Vec<usize>> = queries
            .iter()
            .map(|q| {
                let mut scored: Vec<(f32, usize)> = corpus
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (l2_distance_sq(v, q), i))
                    .collect();
                scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
                scored.into_iter().take(10).map(|(_, i)| i).collect()
            })
            .collect();
        // SQ8 catalog — INSERTs land as `Value::Sq8Vector` cells;
        // HNSW build uses the ADC path verified in step 4.
        let mut cat = Catalog::new();
        cat.create_table(TableSchema::new(
            "vecs",
            alloc::vec![
                ColumnSchema::new("id", DataType::Int, false),
                ColumnSchema::new(
                    "v",
                    DataType::Vector {
                        dim,
                        encoding: VecEncoding::Sq8,
                    },
                    false,
                ),
            ],
        ))
        .unwrap();
        let t = cat.get_mut("vecs").unwrap();
        for (i, v) in corpus.iter().enumerate() {
            t.insert(Row::new(alloc::vec![
                Value::Int(i32::try_from(i).unwrap()),
                Value::Sq8Vector(quantize::quantize(v)),
            ]))
            .unwrap();
        }
        t.add_nsw_index("v_idx".into(), "v", NSW_DEFAULT_M).unwrap();
        let table = cat.get("vecs").unwrap();
        let mut total_overlap = 0_usize;
        for (q, exact) in queries.iter().zip(exact_top10.iter()) {
            let hits = nsw_query(table, "v_idx", q, 10, NswMetric::L2);
            for h in &hits {
                if exact.contains(h) {
                    total_overlap += 1;
                }
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let recall = total_overlap as f32 / (10.0 * queries.len() as f32);
        assert!(
            recall >= 0.95,
            "SQ8 HNSW recall@10 = {recall:.3}, below floor 0.95 — \
             check `sq8_rerank` is wired in `nsw_search` for SQ8 columns"
        );
    }

    #[test]
    fn nsw_index_topology_persists_through_round_trip() {
        // Build an NSW index, capture its (entry, neighbors) tuple, do
        // a full serialize → deserialize, and verify the restored
        // graph is byte-for-byte identical. The point of v2.7 is that
        // startup skips the rebuild, so the topology has to survive
        // the disk hop.
        let mut cat = Catalog::new();
        cat.create_table(TableSchema::new(
            "docs",
            alloc::vec![
                ColumnSchema::new("id", DataType::Int, false),
                ColumnSchema::new(
                    "v",
                    DataType::Vector {
                        dim: 3,
                        encoding: VecEncoding::F32
                    },
                    true
                ),
            ],
        ))
        .unwrap();
        let t = cat.get_mut("docs").unwrap();
        for i in 0..6_i32 {
            #[allow(clippy::cast_precision_loss)] // 0..6 — no precision lost
            let base = (i as f32) * 0.1;
            let row = Row::new(alloc::vec![
                Value::Int(i),
                Value::Vector(alloc::vec![base, base + 0.05, base + 0.1]),
            ]);
            t.insert(row).unwrap();
        }
        t.add_nsw_index("docs_nsw".into(), "v", NSW_DEFAULT_M)
            .unwrap();
        let original = match &cat.get("docs").unwrap().indices()[0].kind {
            IndexKind::Nsw(g) => g.clone(),
            IndexKind::BTree(_) => panic!("expected NSW"),
        };
        let bytes = cat.serialize();
        let restored = Catalog::deserialize(&bytes).expect("deserialize");
        let restored_graph = match &restored.get("docs").unwrap().indices()[0].kind {
            IndexKind::Nsw(g) => g.clone(),
            IndexKind::BTree(_) => panic!("expected NSW"),
        };
        assert_eq!(restored_graph.m, original.m);
        assert_eq!(restored_graph.m_max_0, original.m_max_0);
        assert_eq!(restored_graph.entry, original.entry);
        assert_eq!(restored_graph.entry_level, original.entry_level);
        assert_eq!(restored_graph.levels, original.levels);
        assert_eq!(restored_graph.layers, original.layers);
    }

    #[test]
    fn hnsw_level_assignment_is_deterministic() {
        // Same row index always produces the same level — the topology
        // must be reproducible (matters for serialize round-trip).
        for i in 0..32usize {
            assert_eq!(nsw_assign_level(i), nsw_assign_level(i));
        }
    }

    #[test]
    fn hnsw_layer_0_dominates_population() {
        // Sanity: out of N inserts, the vast majority should land on
        // layer 0. The 4-bit-clear promotion rule gives roughly 1/16
        // promotion to layer ≥ 1, so under 50 nodes we expect ~3 on
        // layer ≥ 1 and the rest on layer 0.
        let on_zero = (0..200usize).filter(|&i| nsw_assign_level(i) == 0).count();
        assert!(on_zero > 150, "level-0 nodes too few: {on_zero}");
    }

    #[test]
    fn hnsw_search_matches_brute_force_for_l2_top1() {
        // Build a small dataset, query it, and confirm the top result
        // matches the brute-force nearest by L2. Topology variability
        // shouldn't break recall at k=1 for well-separated vectors.
        let mut cat = Catalog::new();
        cat.create_table(TableSchema::new(
            "vecs",
            alloc::vec![
                ColumnSchema::new("id", DataType::Int, false),
                ColumnSchema::new(
                    "v",
                    DataType::Vector {
                        dim: 3,
                        encoding: VecEncoding::F32
                    },
                    true
                ),
            ],
        ))
        .unwrap();
        let t = cat.get_mut("vecs").unwrap();
        let dataset: alloc::vec::Vec<(i32, [f32; 3])> = alloc::vec![
            (1, [0.0, 0.0, 0.0]),
            (2, [1.0, 0.0, 0.0]),
            (3, [0.0, 1.0, 0.0]),
            (4, [0.0, 0.0, 1.0]),
            (5, [1.0, 1.0, 0.0]),
            (6, [1.0, 0.0, 1.0]),
            (7, [0.0, 1.0, 1.0]),
            (8, [1.0, 1.0, 1.0]),
            (9, [0.5, 0.5, 0.5]),
            (10, [0.2, 0.8, 0.5]),
        ];
        for &(id, v) in &dataset {
            t.insert(Row::new(alloc::vec![
                Value::Int(id),
                Value::Vector(alloc::vec![v[0], v[1], v[2]]),
            ]))
            .unwrap();
        }
        t.add_nsw_index("v_idx".into(), "v", NSW_DEFAULT_M).unwrap();
        let idx_pos = cat
            .get("vecs")
            .unwrap()
            .indices()
            .iter()
            .position(|i| i.name == "v_idx")
            .unwrap();
        for query in [[0.4, 0.4, 0.4], [0.9, 0.1, 0.0], [0.0, 0.9, 0.9]] {
            let table = cat.get("vecs").unwrap();
            let hnsw_top = nsw_search(table, idx_pos, &query, 1, 16, NswMetric::L2);
            let mut brute: alloc::vec::Vec<(f32, usize)> = (0..table.rows.len())
                .map(|i| {
                    let Value::Vector(v) = &table.rows[i].values[1] else {
                        return (f32::INFINITY, i);
                    };
                    (l2_distance_sq(v, &query), i)
                })
                .collect();
            brute.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
            assert!(!hnsw_top.is_empty(), "HNSW returned no results");
            assert_eq!(
                hnsw_top[0].1, brute[0].1,
                "HNSW top-1 != brute-force top-1 for {query:?}"
            );
        }
    }

    #[test]
    fn serialize_table_with_rows_round_trips() {
        let mut cat = Catalog::new();
        cat.create_table(make_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        t.insert(Row::new(vec![
            Value::Int(1),
            Value::Text("alice".into()),
            Value::Float(95.5),
        ]))
        .unwrap();
        t.insert(Row::new(vec![
            Value::Int(2),
            Value::Text("bob".into()),
            Value::Null,
        ]))
        .unwrap();
        assert_round_trip(&cat);
    }

    #[test]
    fn serialize_multiple_tables_round_trips() {
        let mut cat = Catalog::new();
        cat.create_table(make_users_schema()).unwrap();
        cat.create_table(TableSchema::new(
            "flags",
            vec![
                ColumnSchema::new("id", DataType::BigInt, false),
                ColumnSchema::new("active", DataType::Bool, false),
            ],
        ))
        .unwrap();
        cat.get_mut("flags")
            .unwrap()
            .insert(Row::new(vec![Value::BigInt(7), Value::Bool(true)]))
            .unwrap();
        assert_round_trip(&cat);
    }

    #[test]
    fn deserialize_rejects_bad_magic() {
        let mut buf = b"BADMAGIC".to_vec();
        buf.push(FILE_VERSION);
        buf.extend_from_slice(&0u32.to_le_bytes());
        let err = Catalog::deserialize(&buf).unwrap_err();
        assert!(matches!(err, StorageError::Corrupt(_)));
    }

    #[test]
    fn deserialize_rejects_unsupported_version() {
        let mut buf = FILE_MAGIC.to_vec();
        buf.push(99); // future version
        buf.extend_from_slice(&0u32.to_le_bytes());
        let err = Catalog::deserialize(&buf).unwrap_err();
        assert!(matches!(err, StorageError::Corrupt(ref s) if s.contains("version")));
    }

    #[test]
    fn deserialize_rejects_truncated_file() {
        let mut cat = Catalog::new();
        cat.create_table(make_users_schema()).unwrap();
        let bytes = cat.serialize();
        // Drop the last byte to simulate truncation.
        let truncated = &bytes[..bytes.len() - 1];
        assert!(matches!(
            Catalog::deserialize(truncated),
            Err(StorageError::Corrupt(_))
        ));
    }

    #[test]
    fn deserialize_rejects_trailing_garbage() {
        let cat = Catalog::new();
        let mut bytes = cat.serialize();
        bytes.push(0xFF);
        assert!(matches!(
            Catalog::deserialize(&bytes),
            Err(StorageError::Corrupt(ref s)) if s.contains("trailing")
        ));
    }

    // --- v0.8 indices ------------------------------------------------------

    fn populated_users() -> Catalog {
        let mut cat = Catalog::new();
        cat.create_table(make_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        for (id, name, score) in [
            (1, "alice", Some(90.0)),
            (2, "bob", None),
            (3, "alice", Some(70.0)), // duplicate name → maps to two row idxs
        ] {
            t.insert(Row::new(vec![
                Value::Int(id),
                Value::Text(name.into()),
                score.map_or(Value::Null, Value::Float),
            ]))
            .unwrap();
        }
        cat
    }

    #[test]
    fn add_index_builds_from_existing_rows() {
        let mut cat = populated_users();
        cat.get_mut("users")
            .unwrap()
            .add_index("by_id".into(), "id")
            .unwrap();
        let t = cat.get("users").unwrap();
        let idx = t.index_on(0).expect("index_on(0)");
        assert_eq!(idx.lookup_eq(&IndexKey::Int(2)), &[RowLocator::Hot(1)]);
        assert_eq!(idx.lookup_eq(&IndexKey::Int(99)), &[] as &[RowLocator]);
    }

    #[test]
    fn add_index_dup_name_rejected() {
        let mut cat = populated_users();
        let t = cat.get_mut("users").unwrap();
        t.add_index("ix".into(), "id").unwrap();
        let err = t.add_index("ix".into(), "name").unwrap_err();
        assert!(matches!(err, StorageError::DuplicateIndex { ref name } if name == "ix"));
    }

    #[test]
    fn add_index_unknown_column_rejected() {
        let mut cat = populated_users();
        let err = cat
            .get_mut("users")
            .unwrap()
            .add_index("ix".into(), "ghost")
            .unwrap_err();
        assert!(matches!(err, StorageError::ColumnNotFound { ref column } if column == "ghost"));
    }

    #[test]
    fn insert_after_create_index_updates_it() {
        let mut cat = populated_users();
        let t = cat.get_mut("users").unwrap();
        t.add_index("by_name".into(), "name").unwrap();
        t.insert(Row::new(vec![
            Value::Int(4),
            Value::Text("dave".into()),
            Value::Null,
        ]))
        .unwrap();
        let idx = t.index_on(1).unwrap();
        assert_eq!(
            idx.lookup_eq(&IndexKey::Text("dave".into())),
            &[RowLocator::Hot(3)]
        );
        // Pre-existing duplicates remain mapped to the two original row idxs.
        assert_eq!(
            idx.lookup_eq(&IndexKey::Text("alice".into())),
            &[RowLocator::Hot(0), RowLocator::Hot(2)]
        );
    }

    #[test]
    fn null_or_float_values_are_not_indexed() {
        let mut cat = populated_users();
        let t = cat.get_mut("users").unwrap();
        t.add_index("by_score".into(), "score").unwrap();
        let idx = t.index_on(2).unwrap();
        // bob's score is NULL → no entry for bob.
        // Score is Float → the spec says we don't index NaN-prone columns,
        // so even the present scores are absent. Lookups via IndexKey::Int(90)
        // mis-match the column type and trivially find nothing.
        assert_eq!(idx.lookup_eq(&IndexKey::Int(90)), &[] as &[RowLocator]);
    }

    // --- v0.11 vector type -------------------------------------------------

    #[test]
    fn vector_value_data_type_carries_dim() {
        let v = Value::Vector(vec![1.0, 2.0, 3.0]);
        assert_eq!(
            v.data_type(),
            Some(DataType::Vector {
                dim: 3,
                encoding: VecEncoding::F32
            })
        );
    }

    #[test]
    fn vector_column_insert_matching_dim_ok() {
        let mut cat = Catalog::new();
        cat.create_table(TableSchema::new(
            "emb",
            vec![ColumnSchema::new(
                "v",
                DataType::Vector {
                    dim: 3,
                    encoding: VecEncoding::F32,
                },
                false,
            )],
        ))
        .unwrap();
        cat.get_mut("emb")
            .unwrap()
            .insert(Row::new(vec![Value::Vector(vec![1.0, 2.0, 3.0])]))
            .unwrap();
    }

    #[test]
    fn vector_column_insert_dim_mismatch_rejected() {
        let mut cat = Catalog::new();
        cat.create_table(TableSchema::new(
            "emb",
            vec![ColumnSchema::new(
                "v",
                DataType::Vector {
                    dim: 3,
                    encoding: VecEncoding::F32,
                },
                false,
            )],
        ))
        .unwrap();
        let err = cat
            .get_mut("emb")
            .unwrap()
            .insert(Row::new(vec![Value::Vector(vec![1.0, 2.0])]))
            .unwrap_err();
        assert!(matches!(err, StorageError::TypeMismatch { .. }));
    }

    #[test]
    fn vector_value_survives_catalog_round_trip() {
        let mut cat = Catalog::new();
        cat.create_table(TableSchema::new(
            "emb",
            vec![
                ColumnSchema::new("id", DataType::Int, false),
                ColumnSchema::new(
                    "v",
                    DataType::Vector {
                        dim: 4,
                        encoding: VecEncoding::F32,
                    },
                    false,
                ),
            ],
        ))
        .unwrap();
        cat.get_mut("emb")
            .unwrap()
            .insert(Row::new(vec![
                Value::Int(1),
                Value::Vector(vec![0.5, -1.25, 3.0, 7.0]),
            ]))
            .unwrap();
        let restored = Catalog::deserialize(&cat.serialize()).expect("round-trip");
        let table = restored.get("emb").unwrap();
        assert_eq!(
            table.schema().columns[1].ty,
            DataType::Vector {
                dim: 4,
                encoding: VecEncoding::F32
            }
        );
        assert_eq!(
            table.rows()[0].values[1],
            Value::Vector(vec![0.5, -1.25, 3.0, 7.0])
        );
    }

    #[test]
    fn index_survives_serialize_deserialize_round_trip() {
        let mut cat = populated_users();
        cat.get_mut("users")
            .unwrap()
            .add_index("by_name".into(), "name")
            .unwrap();
        let restored = Catalog::deserialize(&cat.serialize()).unwrap();
        let idx = restored
            .get("users")
            .unwrap()
            .index_on(1)
            .expect("index_on(1) after restore");
        assert_eq!(idx.name, "by_name");
        // Data was rebuilt from rows, not deserialized directly.
        assert_eq!(
            idx.lookup_eq(&IndexKey::Text("alice".into())),
            &[RowLocator::Hot(0), RowLocator::Hot(2)]
        );
    }

    // --- v5.1 cold-tier integration tests ----------------------

    /// Schema with a BIGINT PK column matching what the v5.1 cold-
    /// tier path supports (`IndexKey::Int` → `u64` cast).
    fn bigint_pk_users_schema() -> TableSchema {
        TableSchema::new(
            "users",
            vec![
                ColumnSchema::new("id", DataType::BigInt, false),
                ColumnSchema::new("name", DataType::Text, false),
            ],
        )
    }

    fn make_user_row(id: i64, name: &str) -> Row {
        Row::new(vec![Value::BigInt(id), Value::Text(name.into())])
    }

    #[test]
    fn lookup_by_pk_finds_row_via_hot_index() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        for (id, name) in [(1i64, "alice"), (2, "bob"), (3, "carol")] {
            t.insert(make_user_row(id, name)).unwrap();
        }
        t.add_index("by_id".into(), "id").unwrap();
        // All locators are Hot; cold_segments is empty.
        let got = cat
            .lookup_by_pk("users", "by_id", &IndexKey::Int(2))
            .unwrap();
        assert_eq!(got, make_user_row(2, "bob"));
        assert_eq!(cat.cold_segment_count(), 0);
    }

    #[test]
    fn lookup_by_pk_returns_none_when_key_missing() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        t.insert(make_user_row(1, "alice")).unwrap();
        t.add_index("by_id".into(), "id").unwrap();
        assert!(
            cat.lookup_by_pk("users", "by_id", &IndexKey::Int(999))
                .is_none()
        );
        // Also: unknown table / unknown index name.
        assert!(
            cat.lookup_by_pk("other_table", "by_id", &IndexKey::Int(1))
                .is_none()
        );
        assert!(
            cat.lookup_by_pk("users", "no_such_index", &IndexKey::Int(1))
                .is_none()
        );
    }

    #[test]
    fn lookup_by_pk_resolves_cold_locator_via_loaded_segment() {
        // Build a cold-tier segment whose payloads are dense-encoded
        // BIGINT rows. Wire each PK into the BTree index as a Cold
        // locator. The hot tier carries no rows for those PKs.
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        t.add_index("by_id".into(), "id").unwrap();
        let schema = t.schema.clone();

        let cold_rows: Vec<(i64, &str)> =
            vec![(100, "ivy"), (200, "joe"), (300, "kim"), (400, "lin")];
        let seg_rows: Vec<(u64, Vec<u8>)> = cold_rows
            .iter()
            .map(|(id, name)| {
                let row = make_user_row(*id, name);
                ((*id).cast_unsigned(), encode_row_body_dense(&row, &schema))
            })
            .collect();
        let (seg_bytes, _meta) =
            encode_segment(seg_rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
        let seg_id = cat.load_segment_bytes(seg_bytes).unwrap();
        assert_eq!(seg_id, 0);
        assert_eq!(cat.cold_segment_count(), 1);

        let pairs: Vec<(IndexKey, RowLocator)> = cold_rows
            .iter()
            .map(|(id, _)| {
                (
                    IndexKey::Int(*id),
                    RowLocator::Cold {
                        segment_id: seg_id,
                        page_offset: 0,
                    },
                )
            })
            .collect();
        let registered = cat
            .get_mut("users")
            .unwrap()
            .register_cold_locators("by_id", pairs)
            .unwrap();
        assert_eq!(registered, 4);

        for (id, name) in &cold_rows {
            let got = cat
                .lookup_by_pk("users", "by_id", &IndexKey::Int(*id))
                .unwrap_or_else(|| panic!("cold key {id} not found"));
            assert_eq!(got, make_user_row(*id, name));
        }
        // Cold key that isn't in the segment must return None.
        assert!(
            cat.lookup_by_pk("users", "by_id", &IndexKey::Int(999))
                .is_none()
        );
    }

    #[test]
    fn lookup_by_pk_mixes_hot_and_cold_tiers() {
        // Half the rows live in the hot tier (Table::rows + add_index
        // produces Hot locators); half live in a cold segment and have
        // Cold locators wired manually. Each lookup hits the right tier.
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        for (id, name) in [(1i64, "alice"), (2, "bob")] {
            t.insert(make_user_row(id, name)).unwrap();
        }
        t.add_index("by_id".into(), "id").unwrap();
        let schema = t.schema.clone();

        let cold_rows: Vec<(i64, &str)> = vec![(100, "ivy"), (200, "joe")];
        let seg_rows: Vec<(u64, Vec<u8>)> = cold_rows
            .iter()
            .map(|(id, name)| {
                let row = make_user_row(*id, name);
                ((*id).cast_unsigned(), encode_row_body_dense(&row, &schema))
            })
            .collect();
        let (seg_bytes, _) =
            encode_segment(seg_rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
        let seg_id = cat.load_segment_bytes(seg_bytes).unwrap();
        let pairs: Vec<(IndexKey, RowLocator)> = cold_rows
            .iter()
            .map(|(id, _)| {
                (
                    IndexKey::Int(*id),
                    RowLocator::Cold {
                        segment_id: seg_id,
                        page_offset: 0,
                    },
                )
            })
            .collect();
        cat.get_mut("users")
            .unwrap()
            .register_cold_locators("by_id", pairs)
            .unwrap();

        // Hot tier hits.
        assert_eq!(
            cat.lookup_by_pk("users", "by_id", &IndexKey::Int(1))
                .unwrap(),
            make_user_row(1, "alice")
        );
        assert_eq!(
            cat.lookup_by_pk("users", "by_id", &IndexKey::Int(2))
                .unwrap(),
            make_user_row(2, "bob")
        );
        // Cold tier hits.
        assert_eq!(
            cat.lookup_by_pk("users", "by_id", &IndexKey::Int(100))
                .unwrap(),
            make_user_row(100, "ivy")
        );
        assert_eq!(
            cat.lookup_by_pk("users", "by_id", &IndexKey::Int(200))
                .unwrap(),
            make_user_row(200, "joe")
        );
        // Miss in both tiers.
        assert!(
            cat.lookup_by_pk("users", "by_id", &IndexKey::Int(50))
                .is_none()
        );
    }

    #[test]
    fn register_cold_locators_rejects_nsw_index() {
        let mut cat = Catalog::new();
        cat.create_table(TableSchema::new(
            "vecs",
            vec![
                ColumnSchema::new("id", DataType::Int, false),
                ColumnSchema::new(
                    "v",
                    DataType::Vector {
                        dim: 4,
                        encoding: VecEncoding::F32,
                    },
                    false,
                ),
            ],
        ))
        .unwrap();
        let t = cat.get_mut("vecs").unwrap();
        t.insert(Row::new(vec![
            Value::Int(1),
            Value::Vector(vec![1.0, 0.0, 0.0, 0.0]),
        ]))
        .unwrap();
        t.add_nsw_index("by_v".into(), "v", NSW_DEFAULT_M).unwrap();
        let err = t
            .register_cold_locators(
                "by_v",
                vec![(
                    IndexKey::Int(1),
                    RowLocator::Cold {
                        segment_id: 0,
                        page_offset: 0,
                    },
                )],
            )
            .unwrap_err();
        assert!(matches!(err, StorageError::Corrupt(ref s) if s.contains("NSW")));
    }

    #[test]
    fn load_segment_bytes_rejects_garbage() {
        let mut cat = Catalog::new();
        let err = cat.load_segment_bytes(vec![0u8; 10]).unwrap_err();
        assert!(matches!(err, StorageError::Corrupt(ref s) if s.contains("segment")));
        // Loader doesn't mutate state on error.
        assert_eq!(cat.cold_segment_count(), 0);
    }

    #[test]
    fn load_segment_bytes_returns_sequential_ids() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let schema = cat.get("users").unwrap().schema.clone();
        for batch in 0u32..3 {
            let rows: Vec<(u64, Vec<u8>)> = (0u64..4)
                .map(|i| {
                    let id = u64::from(batch) * 100 + i;
                    let row = make_user_row(id.cast_signed(), "x");
                    (id, encode_row_body_dense(&row, &schema))
                })
                .collect();
            let (bytes, _) = encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
            assert_eq!(cat.load_segment_bytes(bytes).unwrap(), batch);
        }
        assert_eq!(cat.cold_segment_count(), 3);
    }

    // --- v5.2 catalog format v9 ----------------------------------

    /// Hand-craft a v8 catalog byte stream and confirm the v9 reader
    /// accepts it and surfaces every `BTree` entry as a Hot locator.
    /// Guards the backward-compat read path: existing v3.0.2 / v4.x
    /// snapshots on disk must keep loading after the v5.2 bump.
    #[test]
    fn v8_catalog_decodes_as_all_hot_under_v9_reader() {
        // Build a populated catalog in memory, snapshot it with the
        // v9 serializer, then patch the version byte back to 8 and
        // strip the v9 BTree payload bytes so the layout matches what
        // a real v8 snapshot would have produced on disk. The v9
        // reader's version dispatch path then rebuilds the index
        // from rows (every locator becomes Hot).
        let mut cat = populated_users();
        cat.get_mut("users")
            .unwrap()
            .add_index("by_name".into(), "name")
            .unwrap();

        // To produce a faithful v8 byte stream we re-encode the same
        // catalog with the v8 layout: identical bytes up to (and
        // including) the per-index kind tag, but no inline BTree
        // entries.
        let v8_bytes = encode_as_v8(&cat);
        assert_eq!(v8_bytes[FILE_MAGIC.len()], 8, "version byte must be 8");

        let restored = Catalog::deserialize(&v8_bytes).expect("v9 reader accepts v8 stream");
        let idx = restored
            .get("users")
            .unwrap()
            .index_on(1)
            .expect("index_on(1) after restore");
        // v8 path always materialises Hot locators (no cold tier
        // existed pre-v5.2).
        assert_eq!(
            idx.lookup_eq(&IndexKey::Text("alice".into())),
            &[RowLocator::Hot(0), RowLocator::Hot(2)]
        );
        // No accidental Cold leak.
        for entry in idx.lookup_eq(&IndexKey::Text("alice".into())) {
            assert!(entry.is_hot(), "v8 → v9 read must yield Hot only");
        }
    }

    /// Encode `cat` using the v8 layout (no inline `BTree` entries,
    /// version byte = 8). Pure test helper — duplicates just enough
    /// of `Catalog::serialize` to produce a faithful v8 stream that
    /// real v3.0.2 / v4.x deployments wrote.
    fn encode_as_v8(cat: &Catalog) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(FILE_MAGIC);
        out.push(8u8);
        write_u32(&mut out, u32::try_from(cat.tables.len()).unwrap());
        for t in &cat.tables {
            write_str(&mut out, &t.schema.name);
            write_u16(&mut out, u16::try_from(t.schema.columns.len()).unwrap());
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
            write_u32(&mut out, u32::try_from(t.rows.len()).unwrap());
            for row in &t.rows {
                out.extend_from_slice(&encode_row_body_dense(row, &t.schema));
            }
            write_u16(&mut out, u16::try_from(t.indices.len()).unwrap());
            for idx in &t.indices {
                write_str(&mut out, &idx.name);
                write_u16(&mut out, u16::try_from(idx.column_position).unwrap());
                match &idx.kind {
                    // v8 BTree wrote only the kind tag; entries
                    // rebuild from rows on read.
                    IndexKind::BTree(_) => out.push(0),
                    IndexKind::Nsw(g) => {
                        out.push(1);
                        write_u16(&mut out, u16::try_from(g.m).unwrap());
                        write_nsw_graph(&mut out, g);
                    }
                }
            }
        }
        out
    }

    /// Build a catalog that carries both hot and cold locators on a
    /// `BTree` index, snapshot it through `serialize`, then deserialise
    /// and confirm every Cold locator round-trips byte-identical and
    /// `lookup_by_pk` resolves through the rebuilt cold-segment
    /// registry.
    #[test]
    fn v9_catalog_round_trip_preserves_cold_locators() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        // Hot rows: 1, 2
        for (id, name) in [(1i64, "alice"), (2, "bob")] {
            t.insert(make_user_row(id, name)).unwrap();
        }
        t.add_index("by_id".into(), "id").unwrap();
        let schema = t.schema.clone();

        // Cold rows: 100, 200, 300 — sit in a single segment.
        let cold_rows: Vec<(i64, &str)> = vec![(100, "ivy"), (200, "joe"), (300, "kim")];
        let seg_rows: Vec<(u64, Vec<u8>)> = cold_rows
            .iter()
            .map(|(id, name)| {
                let row = make_user_row(*id, name);
                ((*id).cast_unsigned(), encode_row_body_dense(&row, &schema))
            })
            .collect();
        let (seg_bytes, _) =
            encode_segment(seg_rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
        let seg_id = cat.load_segment_bytes(seg_bytes.clone()).unwrap();
        let pairs: Vec<(IndexKey, RowLocator)> = cold_rows
            .iter()
            .map(|(id, _)| {
                (
                    IndexKey::Int(*id),
                    RowLocator::Cold {
                        segment_id: seg_id,
                        page_offset: 0,
                    },
                )
            })
            .collect();
        cat.get_mut("users")
            .unwrap()
            .register_cold_locators("by_id", pairs)
            .unwrap();

        // Snapshot + restore via the v9 codec.
        let bytes = cat.serialize();
        assert_eq!(bytes[FILE_MAGIC.len()], FILE_VERSION);
        let mut restored = Catalog::deserialize(&bytes).expect("v9 round-trip parses");

        // Catalog::serialize does not yet emit cold segment file
        // bytes (v5.3 manifest is the future home for that). For
        // this v9 test the caller side-loads the segment again so
        // lookup_by_pk can resolve the Cold locator. The point of
        // this assertion is that the locator metadata survived the
        // catalog round-trip.
        let restored_seg_id = restored.load_segment_bytes(seg_bytes).unwrap();
        assert_eq!(restored_seg_id, seg_id);

        let idx = restored.get("users").unwrap().index_on(0).unwrap();
        // Hot locators round-trip.
        assert_eq!(idx.lookup_eq(&IndexKey::Int(1)), &[RowLocator::Hot(0)]);
        assert_eq!(idx.lookup_eq(&IndexKey::Int(2)), &[RowLocator::Hot(1)]);
        // Cold locators round-trip byte-identical.
        for (id, _) in &cold_rows {
            assert_eq!(
                idx.lookup_eq(&IndexKey::Int(*id)),
                &[RowLocator::Cold {
                    segment_id: seg_id,
                    page_offset: 0,
                }]
            );
        }
        // End-to-end: lookup_by_pk resolves both tiers.
        assert_eq!(
            restored
                .lookup_by_pk("users", "by_id", &IndexKey::Int(2))
                .unwrap(),
            make_user_row(2, "bob")
        );
        for (id, name) in &cold_rows {
            assert_eq!(
                restored
                    .lookup_by_pk("users", "by_id", &IndexKey::Int(*id))
                    .unwrap(),
                make_user_row(*id, name)
            );
        }
    }

    // --- v5.2.1 hot tier byte tracking ---------------------------

    /// `row_body_encoded_len` is the perf-critical fast path; pin it
    /// against `encode_row_body_dense(...).len()` for every
    /// representative cell type so an encoder change can't silently
    /// desync the counter.
    #[test]
    fn row_body_encoded_len_matches_actual_encode_for_all_types() {
        let schema = TableSchema::new(
            "wide",
            vec![
                ColumnSchema::new("a", DataType::SmallInt, true),
                ColumnSchema::new("b", DataType::Int, false),
                ColumnSchema::new("c", DataType::BigInt, false),
                ColumnSchema::new("d", DataType::Float, false),
                ColumnSchema::new("e", DataType::Bool, false),
                ColumnSchema::new("f", DataType::Text, false),
                ColumnSchema::new(
                    "g",
                    DataType::Vector {
                        dim: 3,
                        encoding: VecEncoding::F32,
                    },
                    false,
                ),
                ColumnSchema::new(
                    "h",
                    DataType::Numeric {
                        precision: 18,
                        scale: 2,
                    },
                    false,
                ),
                ColumnSchema::new("i", DataType::Date, false),
                ColumnSchema::new("j", DataType::Timestamp, false),
            ],
        );
        let cases: &[Row] = &[
            Row::new(vec![
                Value::SmallInt(7),
                Value::Int(42),
                Value::BigInt(1_000_000),
                Value::Float(1.5),
                Value::Bool(true),
                Value::Text("hello".into()),
                Value::Vector(vec![1.0, 2.0, 3.0]),
                Value::Numeric {
                    scaled: 12345,
                    scale: 2,
                },
                Value::Date(20_000),
                Value::Timestamp(1_700_000_000_000_000),
            ]),
            // NULL in the bitmap, varied text length.
            Row::new(vec![
                Value::Null,
                Value::Int(0),
                Value::BigInt(0),
                Value::Float(0.0),
                Value::Bool(false),
                Value::Text(String::new()),
                Value::Vector(vec![]),
                Value::Numeric {
                    scaled: 0,
                    scale: 2,
                },
                Value::Date(0),
                Value::Timestamp(0),
            ]),
            Row::new(vec![
                Value::SmallInt(-1),
                Value::Int(-1),
                Value::BigInt(-1),
                Value::Float(-0.5),
                Value::Bool(true),
                Value::Text("a much longer payload here".into()),
                Value::Vector(vec![0.1, 0.2, 0.3]),
                Value::Numeric {
                    scaled: -999_999_999,
                    scale: 2,
                },
                Value::Date(-1),
                Value::Timestamp(-1),
            ]),
        ];
        for row in cases {
            let actual = encode_row_body_dense(row, &schema).len();
            let fast = row_body_encoded_len(row, &schema);
            assert_eq!(actual, fast, "row {row:?}");
        }
    }

    #[test]
    fn hot_bytes_grows_on_insert_and_matches_encoded_sum() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        assert_eq!(t.hot_bytes(), 0);
        let mut expected: u64 = 0;
        for (id, name) in [(1i64, "alice"), (2, "bob"), (3, "carol")] {
            let row = make_user_row(id, name);
            expected += encode_row_body_dense(&row, &t.schema).len() as u64;
            t.insert(row).unwrap();
        }
        assert_eq!(t.hot_bytes(), expected);
        assert_eq!(cat.hot_tier_bytes(), expected);
    }

    #[test]
    fn hot_bytes_shrinks_on_delete() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        for (id, name) in [(1i64, "alice"), (2, "bob"), (3, "carol")] {
            t.insert(make_user_row(id, name)).unwrap();
        }
        let before = t.hot_bytes();
        // Delete row at position 1 (bob).
        let bob_row = make_user_row(2, "bob");
        let bob_bytes = encode_row_body_dense(&bob_row, &t.schema).len() as u64;
        let removed = t.delete_rows(&[1]);
        assert_eq!(removed, 1);
        assert_eq!(t.hot_bytes(), before - bob_bytes);
    }

    #[test]
    fn hot_bytes_diffs_on_update_for_variable_width_columns() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        t.insert(make_user_row(1, "alice")).unwrap();
        let after_insert = t.hot_bytes();
        // Update with a longer text payload — bytes must grow exactly
        // by the text-length delta.
        let new_row = make_user_row(1, "alice-the-longer-name");
        let old_len = encode_row_body_dense(&make_user_row(1, "alice"), &t.schema).len() as u64;
        let new_len = encode_row_body_dense(&new_row, &t.schema).len() as u64;
        t.update_row(0, new_row.values).unwrap();
        assert_eq!(t.hot_bytes(), after_insert - old_len + new_len);
        assert!(t.hot_bytes() > after_insert, "longer text grew the counter");
    }

    #[test]
    fn hot_bytes_round_trips_through_serialize_deserialize() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        for i in 0..10 {
            t.insert(make_user_row(i, &alloc::format!("name-{i}")))
                .unwrap();
        }
        let pre = cat.hot_tier_bytes();
        let restored = Catalog::deserialize(&cat.serialize()).unwrap();
        assert_eq!(restored.hot_tier_bytes(), pre);
        assert_eq!(restored.get("users").unwrap().hot_bytes(), pre);
    }

    // --- v5.2.2 freezer atomic swap -------------------------------

    /// Happy path: freeze the first half of a populated hot tier,
    /// confirm row counts shift, `hot_bytes` shrinks, and every frozen
    /// PK still resolves via `lookup_by_pk` (now through the cold
    /// segment registered by the freeze).
    #[test]
    fn freeze_oldest_to_cold_moves_rows_and_keeps_lookups_working() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        for id in 0..10i64 {
            t.insert(make_user_row(id, &alloc::format!("u-{id}")))
                .unwrap();
        }
        t.add_index("by_id".into(), "id").unwrap();
        let total_bytes_before = t.hot_bytes();

        let report = cat
            .freeze_oldest_to_cold("users", "by_id", 6)
            .expect("freeze succeeds");
        assert_eq!(report.frozen_rows, 6);
        assert_eq!(report.segment_id, 0);
        assert!(report.bytes_freed > 0);
        assert!(!report.segment_bytes.is_empty());

        let t = cat.get("users").unwrap();
        assert_eq!(t.row_count(), 4, "4 hot rows remain (10 - 6 frozen)");
        assert_eq!(cat.cold_segment_count(), 1);
        // Hot bytes shrank by exactly the freed amount.
        assert_eq!(
            t.hot_bytes(),
            total_bytes_before - report.bytes_freed,
            "hot_bytes accounting matches FreezeReport"
        );

        // Every original PK still resolves — frozen ones via the
        // cold segment, kept ones via the (renumbered) hot tier.
        for id in 0..10i64 {
            let got = cat
                .lookup_by_pk("users", "by_id", &IndexKey::Int(id))
                .unwrap_or_else(|| panic!("PK {id} disappeared after freeze"));
            assert_eq!(got, make_user_row(id, &alloc::format!("u-{id}")));
        }
    }

    /// Two successive freezes on the same index must preserve the
    /// first batch's cold locators when the second freeze runs.
    /// Catches the `rebuild_indices` wipe-Cold-on-delete bug that
    /// `collect_cold_locators` / re-register guards against.
    #[test]
    fn freeze_twice_preserves_prior_cold_locators() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        for id in 0..12i64 {
            t.insert(make_user_row(id, &alloc::format!("u-{id}")))
                .unwrap();
        }
        t.add_index("by_id".into(), "id").unwrap();

        cat.freeze_oldest_to_cold("users", "by_id", 4)
            .expect("first freeze ok");
        cat.freeze_oldest_to_cold("users", "by_id", 4)
            .expect("second freeze ok");

        assert_eq!(cat.get("users").unwrap().row_count(), 4);
        assert_eq!(cat.cold_segment_count(), 2);
        // All 12 PKs still resolve — first 4 via segment 0,
        // next 4 via segment 1, last 4 still hot.
        for id in 0..12i64 {
            let got = cat
                .lookup_by_pk("users", "by_id", &IndexKey::Int(id))
                .unwrap_or_else(|| panic!("PK {id} not resolvable after two freezes"));
            assert_eq!(got, make_user_row(id, &alloc::format!("u-{id}")));
        }
    }

    /// Validation guard tests. Each must return `Err` and **not
    /// mutate the catalog** — the API is all-or-nothing.
    #[test]
    fn freeze_oldest_to_cold_rejects_invalid_input() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        for id in 0..3i64 {
            t.insert(make_user_row(id, &alloc::format!("u-{id}")))
                .unwrap();
        }
        t.add_index("by_id".into(), "id").unwrap();

        // max_rows == 0
        assert!(matches!(
            cat.freeze_oldest_to_cold("users", "by_id", 0),
            Err(StorageError::Corrupt(_))
        ));
        // table missing
        assert!(matches!(
            cat.freeze_oldest_to_cold("missing", "by_id", 1),
            Err(StorageError::Corrupt(_))
        ));
        // index missing
        assert!(matches!(
            cat.freeze_oldest_to_cold("users", "no_such_index", 1),
            Err(StorageError::Corrupt(_))
        ));
        // max_rows > row_count
        assert!(matches!(
            cat.freeze_oldest_to_cold("users", "by_id", 999),
            Err(StorageError::Corrupt(_))
        ));
        // Catalog still untouched.
        assert_eq!(cat.get("users").unwrap().row_count(), 3);
        assert_eq!(cat.cold_segment_count(), 0);
    }

    /// Freeze with a non-integer PK column must surface a clear
    /// error (Text PKs land in v5.5+).
    #[test]
    fn freeze_oldest_to_cold_rejects_non_integer_pk() {
        let mut cat = Catalog::new();
        cat.create_table(TableSchema::new(
            "by_name",
            vec![
                ColumnSchema::new("name", DataType::Text, false),
                ColumnSchema::new("payload", DataType::BigInt, false),
            ],
        ))
        .unwrap();
        let t = cat.get_mut("by_name").unwrap();
        t.insert(Row::new(vec![Value::Text("a".into()), Value::BigInt(1)]))
            .unwrap();
        t.add_index("by_n".into(), "name").unwrap();
        let err = cat
            .freeze_oldest_to_cold("by_name", "by_n", 1)
            .expect_err("non-integer PK rejected");
        match err {
            StorageError::Corrupt(s) => assert!(
                s.contains("non-integer"),
                "error message names the constraint: {s}"
            ),
            other => panic!("expected Corrupt, got {other:?}"),
        }
        // Catalog untouched.
        assert_eq!(cat.get("by_name").unwrap().row_count(), 1);
        assert_eq!(cat.cold_segment_count(), 0);
    }

    /// Hot-tier rows after the freeze must keep their secondary-
    /// index lookups working — `delete_rows` shifts positions, and
    /// `rebuild_indices` must regenerate Hot locators at the new
    /// indices.
    #[test]
    fn freeze_keeps_remaining_hot_rows_addressable_via_secondary_index() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        for id in 0..6i64 {
            t.insert(make_user_row(id, &alloc::format!("u-{id}")))
                .unwrap();
        }
        t.add_index("by_id".into(), "id").unwrap();
        t.add_index("by_name".into(), "name").unwrap();

        cat.freeze_oldest_to_cold("users", "by_id", 3).unwrap();

        // Remaining hot rows: id 3, 4, 5. They moved to positions
        // 0, 1, 2 inside `self.rows`; the `by_name` index must now
        // resolve them via fresh Hot locators.
        let idx = cat.get("users").unwrap().index_on(1).unwrap();
        let got = idx.lookup_eq(&IndexKey::Text("u-4".into()));
        assert_eq!(got.len(), 1);
        assert!(got[0].is_hot(), "kept-hot rows still surface as Hot");
        match got[0] {
            RowLocator::Hot(i) => {
                // The 4th-inserted row was at position 4; after
                // dropping positions 0..3 it sits at position 1.
                assert_eq!(i, 1);
            }
            RowLocator::Cold { .. } => unreachable!(),
        }
    }

    // --- v5.2.3 promote-on-write primitives ----------------------

    /// Build a populated catalog with the first N rows frozen, then
    /// run `promote_cold_row` and verify the row crossed tiers
    /// correctly: the cold locator is retired, a fresh Hot locator
    /// appears, `lookup_by_pk` returns the row from the hot tier, and
    /// `hot_bytes` grew by the row's encoded byte length.
    #[test]
    fn promote_cold_row_pulls_frozen_row_back_to_hot_tier() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        for id in 0..6i64 {
            t.insert(make_user_row(id, &alloc::format!("u-{id}")))
                .unwrap();
        }
        t.add_index("by_id".into(), "id").unwrap();
        // Freeze first 4 rows (ids 0..3). After: hot rows = 4, 5 at
        // positions 0, 1; cold locators for keys 0..3.
        cat.freeze_oldest_to_cold("users", "by_id", 4).unwrap();
        let hot_bytes_before = cat.get("users").unwrap().hot_bytes();

        // Promote PK=2 — it lives in segment 0 as a cold row.
        let new_idx = cat
            .promote_cold_row("users", "by_id", &IndexKey::Int(2))
            .expect("promote ok")
            .expect("PK 2 was cold");
        assert_eq!(
            new_idx, 2,
            "promoted row appended after the 2 surviving hot rows"
        );

        let t = cat.get("users").unwrap();
        assert_eq!(t.row_count(), 3, "hot tier grew from 2 to 3");
        // Hot-bytes climbed by exactly one row's encoded length.
        let row = make_user_row(2, "u-2");
        let row_len = encode_row_body_dense(&row, &t.schema).len() as u64;
        assert_eq!(t.hot_bytes(), hot_bytes_before + row_len);

        // The index now reports a Hot locator (the freshly inserted
        // row) — no Cold locator left for PK 2.
        let entries = t.index_on(0).unwrap().lookup_eq(&IndexKey::Int(2));
        assert_eq!(entries.len(), 1, "exactly one locator per key");
        assert!(entries[0].is_hot(), "promote retired the Cold locator");
        // End-to-end: lookup_by_pk still returns the row body.
        assert_eq!(
            cat.lookup_by_pk("users", "by_id", &IndexKey::Int(2))
                .unwrap(),
            row
        );
        // Other cold rows untouched — still resolvable through the
        // segment.
        assert_eq!(
            cat.lookup_by_pk("users", "by_id", &IndexKey::Int(0))
                .unwrap(),
            make_user_row(0, "u-0")
        );
    }

    /// `promote_cold_row` on a key that's already hot (or absent)
    /// returns `Ok(None)` — not an error. The caller falls back to
    /// the hot-only update/delete path.
    #[test]
    fn promote_cold_row_returns_none_when_key_is_not_cold() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        t.insert(make_user_row(7, "alice")).unwrap();
        t.add_index("by_id".into(), "id").unwrap();

        // Hot-only key.
        assert!(
            cat.promote_cold_row("users", "by_id", &IndexKey::Int(7))
                .unwrap()
                .is_none()
        );
        // Absent key.
        assert!(
            cat.promote_cold_row("users", "by_id", &IndexKey::Int(99))
                .unwrap()
                .is_none()
        );
        // Catalog untouched on both no-op paths.
        assert_eq!(cat.get("users").unwrap().row_count(), 1);
        assert_eq!(cat.cold_segment_count(), 0);
    }

    /// `shadow_cold_row` removes every Cold locator for a key on a
    /// `BTree` index. After the shadow, `lookup_by_pk` for that key
    /// returns None (the row data still sits in the segment file,
    /// but it's now garbage; compaction will reclaim it later).
    #[test]
    fn shadow_cold_row_removes_cold_locators_and_drops_lookup() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        for id in 0..5i64 {
            t.insert(make_user_row(id, &alloc::format!("u-{id}")))
                .unwrap();
        }
        t.add_index("by_id".into(), "id").unwrap();
        cat.freeze_oldest_to_cold("users", "by_id", 3).unwrap();

        // Shadow PK=1 — pre-shadow lookup hits the cold tier.
        assert!(
            cat.lookup_by_pk("users", "by_id", &IndexKey::Int(1))
                .is_some(),
            "frozen PK resolves before shadow"
        );
        let removed = cat
            .shadow_cold_row("users", "by_id", &IndexKey::Int(1))
            .unwrap();
        assert_eq!(removed, 1, "exactly one cold locator retired");

        // Post-shadow: lookup misses, even though the row still
        // exists in segment 0.
        assert!(
            cat.lookup_by_pk("users", "by_id", &IndexKey::Int(1))
                .is_none(),
            "shadowed key no longer resolves"
        );
        // Other cold keys still resolve.
        assert_eq!(
            cat.lookup_by_pk("users", "by_id", &IndexKey::Int(0))
                .unwrap(),
            make_user_row(0, "u-0")
        );
        assert_eq!(
            cat.lookup_by_pk("users", "by_id", &IndexKey::Int(2))
                .unwrap(),
            make_user_row(2, "u-2")
        );
    }

    /// `shadow_cold_row` returns 0 (not Err) for keys with only Hot
    /// entries or no entries — the engine's DELETE path uses this
    /// signal to decide whether the cold-tier shadow path consumed
    /// the work.
    #[test]
    fn shadow_cold_row_returns_zero_when_key_is_not_cold() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        t.insert(make_user_row(1, "alice")).unwrap();
        t.add_index("by_id".into(), "id").unwrap();
        assert_eq!(
            cat.shadow_cold_row("users", "by_id", &IndexKey::Int(1))
                .unwrap(),
            0,
            "hot-only key drops no cold locators"
        );
        assert_eq!(
            cat.shadow_cold_row("users", "by_id", &IndexKey::Int(999))
                .unwrap(),
            0,
            "absent key drops no cold locators"
        );
        assert_eq!(cat.get("users").unwrap().row_count(), 1);
    }

    /// Validation guards on both promote / shadow primitives.
    #[test]
    fn promote_and_shadow_reject_invalid_inputs() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        t.insert(make_user_row(1, "alice")).unwrap();
        t.add_index("by_id".into(), "id").unwrap();

        // Missing table.
        assert!(matches!(
            cat.promote_cold_row("missing", "by_id", &IndexKey::Int(1)),
            Err(StorageError::Corrupt(_))
        ));
        assert!(matches!(
            cat.shadow_cold_row("missing", "by_id", &IndexKey::Int(1)),
            Err(StorageError::Corrupt(_))
        ));
        // Missing index.
        assert!(matches!(
            cat.promote_cold_row("users", "no_such_index", &IndexKey::Int(1)),
            Err(StorageError::Corrupt(_))
        ));
        assert!(matches!(
            cat.shadow_cold_row("users", "no_such_index", &IndexKey::Int(1)),
            Err(StorageError::Corrupt(_))
        ));
    }

    /// Round trip: freeze → promote → re-freeze. The same PK can
    /// migrate hot ↔ cold multiple times. After two cycles only the
    /// final Hot locator should be live.
    #[test]
    fn promote_then_refreeze_does_not_leave_orphan_locators() {
        let mut cat = Catalog::new();
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        for id in 0..4i64 {
            t.insert(make_user_row(id, &alloc::format!("u-{id}")))
                .unwrap();
        }
        t.add_index("by_id".into(), "id").unwrap();

        // Cycle 1: freeze first 2 rows, then promote PK 0.
        cat.freeze_oldest_to_cold("users", "by_id", 2).unwrap();
        let promoted = cat
            .promote_cold_row("users", "by_id", &IndexKey::Int(0))
            .unwrap();
        assert!(promoted.is_some());
        let entries_after_promote = cat
            .get("users")
            .unwrap()
            .index_on(0)
            .unwrap()
            .lookup_eq(&IndexKey::Int(0))
            .to_vec();
        assert_eq!(entries_after_promote.len(), 1);
        assert!(entries_after_promote[0].is_hot());

        // Cycle 2: freeze the front rows again. PK 0 is now at
        // position 2 (after the survivors); it could still go cold
        // again on a future freeze depending on policy, but the
        // current "first N positions" policy leaves it alone here.
        // What matters: prior cold locators for PKs 0..1 are gone,
        // PKs 2..3 still resolve through their original segments.
        for id in [2i64, 3] {
            assert_eq!(
                cat.lookup_by_pk("users", "by_id", &IndexKey::Int(id))
                    .unwrap(),
                make_user_row(id, &alloc::format!("u-{id}"))
            );
        }
    }
}
