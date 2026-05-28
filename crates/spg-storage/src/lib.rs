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
pub mod persistent;
pub mod persistent_btree;

pub use self::bloom::{BloomError, BloomFilter};

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use self::persistent::PersistentVec;
use self::persistent_btree::PersistentBTreeMap;

/// Runtime type tags. `Vector(dim)` / `Varchar(max)` / `Char(size)` are
/// parameterised; the parameter travels with both the column schema and
/// the on-wire serialised representation.
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
    /// pgvector-style fixed-dimension float32 vector.
    Vector(u32),
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
            Self::Vector(n) => write!(f, "VECTOR({n})"),
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
            Self::Vector(v) => Some(DataType::Vector(
                u32::try_from(v.len()).expect("vector dim ≤ u32"),
            )),
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

#[derive(Debug, Clone)]
pub enum IndexKind {
    /// v4.40: structural-sharing B-tree over `IndexKey`. Replaces the v0.8
    /// `BTreeMap<IndexKey, Vec<usize>>` — `Index::clone` is now an `Arc`
    /// bump regardless of index size, so `Catalog::clone` inside the
    /// v4.34 auto-commit wrap stays O(1) even for tables with secondary
    /// indices (the case that bottlenecked v4.39 at 1M rows in the
    /// sweep).
    BTree(PersistentBTreeMap<IndexKey, Vec<usize>>),
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
    pub levels: Vec<u8>,
    /// `layers[l][i]` = neighbours of node `i` at layer `l`. Inner vec
    /// is empty when node `i` doesn't reach layer `l`.
    pub layers: Vec<Vec<Vec<usize>>>,
}

impl NswGraph {
    fn new(m: usize) -> Self {
        Self {
            m,
            m_max_0: m.saturating_mul(2),
            entry: None,
            entry_level: 0,
            levels: Vec::new(),
            layers: alloc::vec![Vec::new()],
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

    /// Look up the row indices stored under `key` (B-tree only). Returns
    /// an empty slice when the key is absent or the index is an NSW
    /// graph — callers can treat both cases uniformly.
    pub fn lookup_eq(&self, key: &IndexKey) -> &[usize] {
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
#[derive(Debug, Clone)]
pub struct Table {
    schema: TableSchema,
    rows: PersistentVec<Row>,
    indices: Vec<Index>,
}

impl Table {
    pub fn new(schema: TableSchema) -> Self {
        Self {
            schema,
            rows: PersistentVec::new(),
            indices: Vec::new(),
        }
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
                entries.push(new_row_idx);
                map.insert_mut(key, entries);
            }
        }
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
                    entries.push(i);
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
        for (i, row) in self.rows.iter().enumerate() {
            if !to_remove[i] {
                new_rows.push_mut(row.clone());
            }
        }
        self.rows = new_rows;
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
        self.rows = self
            .rows
            .set(position, Row::new(new_values))
            .expect("position bounds-checked above");
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
                            entries.push(i);
                            map.insert_mut(key, entries);
                        }
                    }
                }
                self.indices.push(idx);
            }
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
        if !matches!(self.schema.columns[column_position].ty, DataType::Vector(_)) {
            return Err(StorageError::TypeMismatch {
                column: column_name.into(),
                expected: DataType::Vector(0),
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

/// Insert one row into the HNSW graph held by index slot `idx_pos`.
/// No-op when the row's value at the indexed column isn't a Vector.
fn nsw_insert_at(table: &mut Table, idx_pos: usize, new_row_idx: usize) {
    let col_pos = table.indices[idx_pos].column_position;
    let Value::Vector(v) = &table.rows[new_row_idx].values[col_pos] else {
        // Even non-vector rows occupy a level slot so per-node Vec
        // lengths stay aligned with `table.rows.len()`.
        ensure_node_slot(table, idx_pos, new_row_idx, 0);
        return;
    };
    if v.is_empty() {
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
            g.levels[new_row_idx] = level;
        }
        return;
    }
    // Set the node's recorded level.
    if let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind {
        g.levels[new_row_idx] = level;
    }
    let query = match &table.rows[new_row_idx].values[col_pos] {
        Value::Vector(v) => v.clone(),
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
        g.layers.push(Vec::new());
    }
    while g.levels.len() <= new_row_idx {
        g.levels.push(0);
    }
    for layer_vec in &mut g.layers {
        while layer_vec.len() <= new_row_idx {
            layer_vec.push(Vec::new());
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
        let neighbours: &[usize] = g
            .layers
            .get(layer as usize)
            .and_then(|layer_v| layer_v.get(current))
            .map_or(&[][..], Vec::as_slice);
        let mut best = current;
        let mut best_d = current_d;
        for &n in neighbours {
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
        match &table.rows[entry_node].values[col_pos] {
            Value::Vector(v) => metric_distance(metric, v, query),
            _ => return Vec::new(),
        }
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
        let neighbours: &[usize] = g
            .layers
            .get(layer as usize)
            .and_then(|layer_v| layer_v.get(cur.node))
            .map_or(&[][..], Vec::as_slice);
        for &n in neighbours {
            if n >= row_count || visited[n] {
                continue;
            }
            visited[n] = true;
            let Value::Vector(nv) = &table.rows[n].values[col_pos] else {
                continue;
            };
            if nv.len() != query.len() {
                continue;
            }
            let dn = metric_distance(metric, nv, query);
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
        let Some(Value::Vector(e_vec)) = table.rows.get(e).and_then(|r| r.values.get(col_pos))
        else {
            continue;
        };
        let mut covered = false;
        for &r in &chosen {
            let Some(Value::Vector(r_vec)) =
                table.rows.get(r).and_then(|row| row.values.get(col_pos))
            else {
                continue;
            };
            if e_vec.len() != r_vec.len() {
                continue;
            }
            // dist(e, r) measured in the same metric the topology was
            // built with (L2). If a chosen `r` is closer to `e` than
            // the query is, `r` already "covers" `e` for navigation.
            if l2_distance_sq(e_vec, r_vec) < d_q {
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
    if let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind {
        let layer_v = &mut g.layers[layer as usize];
        layer_v[new_row_idx] = peers.to_vec();
    }
    for &peer in peers {
        let host_vec = match &table.rows[peer].values[col_pos] {
            Value::Vector(v) => v.clone(),
            _ => continue,
        };
        // 1. add the new node to peer's adjacency
        if let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind {
            let layer_v = &mut g.layers[layer as usize];
            if !layer_v[peer].contains(&new_row_idx) {
                layer_v[peer].push(new_row_idx);
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
                IndexKind::Nsw(g) => g.layers[layer as usize][peer].clone(),
                IndexKind::BTree(_) => continue,
            };
            // Sort by distance to `host_vec` ascending so the heuristic
            // receives candidates closest-first.
            let mut tagged: Vec<(f32, usize)> = current_peers
                .iter()
                .map(|&p| {
                    let Value::Vector(pv) = &table.rows[p].values[col_pos] else {
                        return (f32::INFINITY, p);
                    };
                    (l2_distance_sq(&host_vec, pv), p)
                })
                .collect();
            tagged.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
            let kept = select_neighbours_heuristic(&tagged, cap, table, col_pos);
            if let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind {
                g.layers[layer as usize][peer] = kept;
            }
        }
    }
}

/// Squared L2 distance from `query` to the vector at `(row, col_pos)`.
/// Returns `f32::INFINITY` when the cell isn't a Vector (so the caller
/// can compare uniformly without an Option ladder).
fn vec_l2_sq(table: &Table, col_pos: usize, row: usize, query: &[f32]) -> f32 {
    match table.rows.get(row).and_then(|r| r.values.get(col_pos)) {
        Some(Value::Vector(v)) if v.len() == query.len() => l2_distance_sq(v, query),
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
    let ef = ef.max(k);
    // Descend by L2 (the topology metric) so layers prune consistently.
    let entry_d = vec_l2_sq(table, col_pos, entry, query);
    let mut current = entry;
    let mut current_d = entry_d;
    for layer in (1..=entry_level).rev() {
        (current, current_d) = greedy_layer_walk(table, idx_pos, layer, current, current_d, query);
    }
    // Final beam search on layer 0 under the caller's metric.
    let mut results = layer_beam_search(table, idx_pos, 0, current, current_d, query, ef, metric);
    results.truncate(k);
    results
}

fn metric_distance(metric: NswMetric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        NswMetric::L2 => l2_distance_sq(a, b),
        NswMetric::InnerProduct => {
            let mut dot: f32 = 0.0;
            for (x, y) in a.iter().zip(b.iter()) {
                dot += x * y;
            }
            -dot
        }
        NswMetric::Cosine => {
            let mut dot: f32 = 0.0;
            let mut na: f32 = 0.0;
            let mut nb: f32 = 0.0;
            for (x, y) in a.iter().zip(b.iter()) {
                dot += x * y;
                na += x * x;
                nb += y * y;
            }
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
}

impl Catalog {
    pub const fn new() -> Self {
        Self {
            tables: Vec::new(),
            by_name: BTreeMap::new(),
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
const FILE_VERSION: u8 = 8;

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
            // bitmap inlined into `out` (no per-row alloc), then a
            // tightly packed body for each non-NULL cell, decoded by
            // column type. Saves one tag byte per cell vs the v7
            // self-describing value format.
            let bitmap_bytes = t.schema.columns.len().div_ceil(8);
            for row in &t.rows {
                // Reserve the bitmap slot first (zeroed), remember the
                // offset, OR-in each NULL bit, then write bodies.
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
                    write_value_body(&mut out, v, t.schema.columns[col_idx].ty);
                }
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
                    IndexKind::BTree(_) => out.push(0),
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
        if version != FILE_VERSION {
            return Err(StorageError::Corrupt(format!(
                "unsupported file version: {version}"
            )));
        }
        let table_count = cur.read_u32()? as usize;
        let mut cat = Self::new();
        for _ in 0..table_count {
            deserialize_table(&mut cur, &mut cat)?;
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
fn deserialize_table(cur: &mut Cursor<'_>, cat: &mut Catalog) -> Result<(), StorageError> {
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
    deserialize_indices(cur, t)?;
    Ok(())
}

fn deserialize_rows(
    cur: &mut Cursor<'_>,
    t: &mut Table,
    n_cols: usize,
) -> Result<(), StorageError> {
    let row_count = cur.read_u32()? as usize;
    // v4.39: PV has no `reserve` (the BVT doesn't preallocate a contiguous
    // buffer); we just push directly and let the trie grow.
    let bitmap_bytes = n_cols.div_ceil(8);
    let col_types: Vec<DataType> = t.schema.columns.iter().map(|c| c.ty).collect();
    let mut bitmap_buf = [0u8; 32];
    for _ in 0..row_count {
        let slice = cur.take(bitmap_bytes)?;
        if bitmap_bytes > bitmap_buf.len() {
            return Err(StorageError::Corrupt(format!(
                "row NULL bitmap {bitmap_bytes} B exceeds 32 B cap"
            )));
        }
        bitmap_buf[..bitmap_bytes].copy_from_slice(slice);
        let mut values = Vec::with_capacity(n_cols);
        for col_idx in 0..n_cols {
            if (bitmap_buf[col_idx / 8] >> (col_idx % 8)) & 1 == 1 {
                values.push(Value::Null);
            } else {
                values.push(cur.read_value_body(col_types[col_idx])?);
            }
        }
        t.rows.push_mut(Row { values });
    }
    Ok(())
}

fn deserialize_indices(cur: &mut Cursor<'_>, t: &mut Table) -> Result<(), StorageError> {
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
                t.add_index(idx_name, &column_name)?;
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
            for &peer in neighbors {
                write_u32(
                    out,
                    u32::try_from(peer).expect("HNSW neighbour index fits in u32"),
                );
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
        DataType::Vector(dim) => {
            out.push(6);
            out.extend_from_slice(&dim.to_le_bytes());
        }
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
            6 => Ok(DataType::Vector(self.read_u32()?)),
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
            other => Err(StorageError::Corrupt(format!(
                "unknown data type tag: {other}"
            ))),
        }
    }
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
        (Value::Vector(v), DataType::Vector(_)) => {
            let dim = u32::try_from(v.len()).expect("vector dim fits in u32");
            out.extend_from_slice(&dim.to_le_bytes());
            for x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
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
    fn read_str(&mut self) -> Result<String, StorageError> {
        let len = self.read_u16()? as usize;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes)
            .map(String::from)
            .map_err(|_| StorageError::Corrupt("invalid UTF-8 in identifier or text".into()))
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
            DataType::Vector(_) => {
                let dim = self.read_u32()? as usize;
                let mut v = Vec::with_capacity(dim);
                for _ in 0..dim {
                    let bytes: [u8; 4] = self.take(4)?.try_into().expect("checked");
                    v.push(f32::from_le_bytes(bytes));
                }
                Ok(Value::Vector(v))
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
        let mut levels: Vec<u8> = Vec::with_capacity(node_count);
        for _ in 0..node_count {
            levels.push(self.read_u8()?);
        }
        let layer_count = self.read_u8()? as usize;
        let mut layers: Vec<Vec<Vec<usize>>> = Vec::with_capacity(layer_count);
        for _ in 0..layer_count {
            let n = self.read_u32()? as usize;
            let mut per_layer: Vec<Vec<usize>> = Vec::with_capacity(n);
            for _ in 0..n {
                let cnt = self.read_u16()? as usize;
                let mut row = Vec::with_capacity(cnt);
                for _ in 0..cnt {
                    row.push(self.read_u32()? as usize);
                }
                per_layer.push(row);
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
                ColumnSchema::new("v", DataType::Vector(3), true),
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
                ColumnSchema::new("v", DataType::Vector(3), true),
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
        assert_eq!(idx.lookup_eq(&IndexKey::Int(2)), &[1]);
        assert_eq!(idx.lookup_eq(&IndexKey::Int(99)), &[] as &[usize]);
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
        assert_eq!(idx.lookup_eq(&IndexKey::Text("dave".into())), &[3]);
        // Pre-existing duplicates remain mapped to the two original row idxs.
        assert_eq!(idx.lookup_eq(&IndexKey::Text("alice".into())), &[0, 2]);
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
        assert_eq!(idx.lookup_eq(&IndexKey::Int(90)), &[] as &[usize]);
    }

    // --- v0.11 vector type -------------------------------------------------

    #[test]
    fn vector_value_data_type_carries_dim() {
        let v = Value::Vector(vec![1.0, 2.0, 3.0]);
        assert_eq!(v.data_type(), Some(DataType::Vector(3)));
    }

    #[test]
    fn vector_column_insert_matching_dim_ok() {
        let mut cat = Catalog::new();
        cat.create_table(TableSchema::new(
            "emb",
            vec![ColumnSchema::new("v", DataType::Vector(3), false)],
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
            vec![ColumnSchema::new("v", DataType::Vector(3), false)],
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
                ColumnSchema::new("v", DataType::Vector(4), false),
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
        assert_eq!(table.schema().columns[1].ty, DataType::Vector(4));
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
        assert_eq!(idx.lookup_eq(&IndexKey::Text("alice".into())), &[0, 2]);
    }
}
