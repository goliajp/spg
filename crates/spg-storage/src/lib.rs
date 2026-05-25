//! In-memory storage primitives.
//!
//! v0.3 is intentionally simple: a flat catalog of tables, each holding rows
//! as `Vec<Value>` (positional, matching the table's `TableSchema`). No MVCC,
//! no on-disk format — those land in later milestones.
#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

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
            Value::Null | Value::Float(_) | Value::Vector(_) | Value::Numeric { .. } => None,
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
    /// B-tree over `IndexKey` (the legacy equality-lookup index).
    BTree(BTreeMap<IndexKey, Vec<usize>>),
    /// Navigable-small-world graph for vector kNN search.
    Nsw(NswGraph),
}

/// Single-layer NSW graph (v2.0). Each node tracks up to `m` undirected
/// neighbors; search walks greedily from `entry`. v2.x will layer this.
#[derive(Debug, Clone)]
pub struct NswGraph {
    pub m: usize,
    pub entry: Option<usize>,
    /// `neighbors[i]` are row indices connected to row `i`. Rows whose
    /// value at the index's column is NULL / non-Vector are absent from
    /// the graph (their `Vec` stays empty).
    pub neighbors: Vec<Vec<usize>>,
}

impl NswGraph {
    fn new(m: usize) -> Self {
        Self {
            m,
            entry: None,
            neighbors: Vec::new(),
        }
    }
}

impl Index {
    fn new_btree(name: String, column_position: usize) -> Self {
        Self {
            name,
            column_position,
            kind: IndexKind::BTree(BTreeMap::new()),
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

/// In-memory table: schema + a flat row vector + secondary indices.
#[derive(Debug, Clone)]
pub struct Table {
    schema: TableSchema,
    rows: Vec<Row>,
    indices: Vec<Index>,
}

impl Table {
    pub const fn new(schema: TableSchema) -> Self {
        Self {
            schema,
            rows: Vec::new(),
            indices: Vec::new(),
        }
    }

    pub const fn schema(&self) -> &TableSchema {
        &self.schema
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn row_count(&self) -> usize {
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
                    (DataType::Text, DataType::Varchar(_) | DataType::Char(_))
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
                map.entry(key).or_default().push(new_row_idx);
            }
        }
        self.rows.push(row);
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
                    map.entry(key).or_default().push(i);
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

/// Insert one row into the NSW graph held by index slot `idx_pos`.
/// No-op when the row's value at the indexed column isn't a Vector.
fn nsw_insert_at(table: &mut Table, idx_pos: usize, new_row_idx: usize) {
    let col_pos = table.indices[idx_pos].column_position;
    let dim = match &table.rows[new_row_idx].values[col_pos] {
        Value::Vector(v) => v.len(),
        _ => return,
    };
    if dim == 0 {
        return;
    }
    let m = match &table.indices[idx_pos].kind {
        IndexKind::Nsw(g) => g.m,
        IndexKind::BTree(_) => unreachable!("nsw_insert_at on a BTree index"),
    };
    let entry = match &table.indices[idx_pos].kind {
        IndexKind::Nsw(g) => g.entry,
        IndexKind::BTree(_) => unreachable!(),
    };
    // First node ever — declare it the entry and call it done.
    if entry.is_none() {
        if let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind {
            // Make sure neighbours vector is big enough.
            while g.neighbors.len() <= new_row_idx {
                g.neighbors.push(Vec::new());
            }
            g.entry = Some(new_row_idx);
        }
        return;
    }
    // Find the M nearest existing nodes via greedy walk.
    let query = match &table.rows[new_row_idx].values[col_pos] {
        Value::Vector(v) => v.clone(),
        _ => return,
    };
    // The graph topology is always built with L2 — querying under a
    // different metric still reuses the same edges (graph topology is
    // approximate by design).
    let nearest = nsw_search(table, idx_pos, &query, m, m * 2, NswMetric::L2);
    // Connect bidirectionally. Trim each endpoint to M to keep degree bounded.
    let new_neighbors: Vec<usize> = nearest
        .iter()
        .filter(|(_, idx)| *idx != new_row_idx)
        .map(|(_, idx)| *idx)
        .collect();
    let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind else {
        unreachable!()
    };
    while g.neighbors.len() <= new_row_idx {
        g.neighbors.push(Vec::new());
    }
    g.neighbors[new_row_idx].clone_from(&new_neighbors);
    for n in new_neighbors {
        // Ensure target row's adjacency vector is reachable.
        while g.neighbors.len() <= n {
            g.neighbors.push(Vec::new());
        }
        if !g.neighbors[n].contains(&new_row_idx) {
            g.neighbors[n].push(new_row_idx);
            if g.neighbors[n].len() > g.m {
                // Drop one (the most distant). We trim by recomputing
                // distances on demand — degree stays bounded.
                let host = n;
                let Value::Vector(host_vec) = table.rows[host].values[col_pos].clone() else {
                    continue;
                };
                let mut tagged: Vec<(f32, usize)> = g.neighbors[host]
                    .iter()
                    .map(|&peer| {
                        let Value::Vector(pv) = &table.rows[peer].values[col_pos] else {
                            return (f32::INFINITY, peer);
                        };
                        (l2_distance_sq(&host_vec, pv), peer)
                    })
                    .collect();
                tagged.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
                tagged.truncate(g.m);
                g.neighbors[host] = tagged.into_iter().map(|(_, peer)| peer).collect();
            }
        }
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

/// Greedy NSW kNN search: walk from the graph entry node, maintaining
/// an `ef`-sized candidate pool, return the top `k` results under the
/// caller-chosen metric.
fn nsw_search(
    table: &Table,
    idx_pos: usize,
    query: &[f32],
    k: usize,
    ef: usize,
    metric: NswMetric,
) -> Vec<(f32, usize)> {
    let g = match &table.indices[idx_pos].kind {
        IndexKind::Nsw(g) => g,
        IndexKind::BTree(_) => return Vec::new(),
    };
    let Some(entry) = g.entry else {
        return Vec::new();
    };
    let col_pos = table.indices[idx_pos].column_position;
    let ef = ef.max(k);
    let mut visited: alloc::collections::BTreeSet<usize> = alloc::collections::BTreeSet::new();
    visited.insert(entry);
    let d0 = match &table.rows[entry].values[col_pos] {
        Value::Vector(v) => metric_distance(metric, v, query),
        _ => return Vec::new(),
    };
    // `candidates` is the open frontier (min-distance first).
    // `results` is the working top-`ef` (max-distance last).
    let mut candidates: Vec<(f32, usize)> = alloc::vec![(d0, entry)];
    let mut results: Vec<(f32, usize)> = alloc::vec![(d0, entry)];
    while let Some(&(d_cur, idx)) = candidates.first() {
        candidates.remove(0);
        let worst = results.last().map_or(f32::INFINITY, |&(d, _)| d);
        if d_cur > worst && results.len() >= ef {
            break;
        }
        let neighbors: Vec<usize> = g.neighbors.get(idx).cloned().unwrap_or_default();
        for n in neighbors {
            if !visited.insert(n) {
                continue;
            }
            let Value::Vector(nv) = &table.rows[n].values[col_pos] else {
                continue;
            };
            if nv.len() != query.len() {
                continue;
            }
            let dn = metric_distance(metric, nv, query);
            let worst = results.last().map_or(f32::INFINITY, |&(d, _)| d);
            if results.len() < ef || dn < worst {
                let pos = results.partition_point(|&(d, _)| d <= dn);
                results.insert(pos, (dn, n));
                if results.len() > ef {
                    results.truncate(ef);
                }
                let pos = candidates.partition_point(|&(d, _)| d <= dn);
                candidates.insert(pos, (dn, n));
            }
        }
    }
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
fn l2_distance_sq(a: &[f32], b: &[f32]) -> f32 {
    let mut sum: f32 = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        let d = *x - *y;
        sum += d * d;
    }
    sum
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

/// Flat catalog. `Vec` is intentional — std `HashMap` is unavailable in
/// `no_std` + alloc, and v0.3 is single-database with a small table count.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    tables: Vec<Table>,
}

impl Catalog {
    pub const fn new() -> Self {
        Self { tables: Vec::new() }
    }

    pub fn create_table(&mut self, schema: TableSchema) -> Result<(), StorageError> {
        if self.tables.iter().any(|t| t.schema.name == schema.name) {
            return Err(StorageError::DuplicateTable {
                name: schema.name.clone(),
            });
        }
        self.tables.push(Table::new(schema));
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.schema.name == name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Table> {
        self.tables.iter_mut().find(|t| t.schema.name == name)
    }

    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    /// Borrow-free copy of every table's name in catalog order. Used
    /// by `SHOW TABLES` so the engine can build a result set without
    /// holding a reference into the catalog past the row build.
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
// NSW graph topology started travelling on disk (v2.7).
// =========================================================================

const FILE_MAGIC: &[u8; 8] = b"SPGDB001";
const FILE_VERSION: u8 = 6;

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
            for row in &t.rows {
                for v in &row.values {
                    write_value(&mut out, v);
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
            cat.create_table(TableSchema::new(name.clone(), cols))?;
            let n_cols = cat.get(&name).expect("just inserted").schema.columns.len();
            let row_count = cur.read_u32()? as usize;
            for _ in 0..row_count {
                let mut values = Vec::with_capacity(n_cols);
                for _ in 0..n_cols {
                    values.push(cur.read_value()?);
                }
                let t = cat.get_mut(&name).expect("just inserted");
                t.rows.push(Row { values });
            }
            // v0.8 index definitions — rebuild data from the rows we just
            // restored. (Older files without this section will fail at
            // `read_u16` with `Truncated`; not a concern for this branch
            // because all on-disk dumps are written by the current code.)
            let index_count = cur.read_u16()? as usize;
            for _ in 0..index_count {
                let idx_name = cur.read_str()?;
                let col_pos = cur.read_u16()? as usize;
                let column_name = cat
                    .get(&name)
                    .expect("just inserted")
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
                let t = cat.get_mut(&name).expect("just inserted");
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

// --- low-level binary helpers ---------------------------------------------

/// Write a `DataType` as a tag byte + optional payload (Vector carries its
/// `u32` dimension). Inverse: [`read_data_type`].
/// Serialize an NSW graph after the `[kind=1][u16 M]` header.
/// Layout:
/// - `[entry u32]` — `u32::MAX` means `None`, else the entry node index
/// - `[node_count u32]`
/// - for each node: `[neighbor_count u16] [neighbor u32]*`
fn write_nsw_graph(out: &mut Vec<u8>, g: &NswGraph) {
    let entry = g.entry.map_or(u32::MAX, |e| {
        u32::try_from(e).expect("NSW entry fits in u32")
    });
    out.extend_from_slice(&entry.to_le_bytes());
    write_u32(
        out,
        u32::try_from(g.neighbors.len()).expect("NSW node count fits in u32"),
    );
    for neighbors in &g.neighbors {
        write_u16(
            out,
            u16::try_from(neighbors.len()).expect("NSW neighbour list fits in u16"),
        );
        for &peer in neighbors {
            write_u32(
                out,
                u32::try_from(peer).expect("NSW neighbour index fits in u32"),
            );
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
            other => Err(StorageError::Corrupt(format!(
                "unknown data type tag: {other}"
            ))),
        }
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
        Value::Text(s) => {
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
        let entry_raw = self.read_u32()?;
        let entry = if entry_raw == u32::MAX {
            None
        } else {
            Some(entry_raw as usize)
        };
        let node_count = self.read_u32()? as usize;
        let mut neighbors: Vec<Vec<usize>> = Vec::with_capacity(node_count);
        for _ in 0..node_count {
            let cnt = self.read_u16()? as usize;
            let mut row = Vec::with_capacity(cnt);
            for _ in 0..cnt {
                row.push(self.read_u32()? as usize);
            }
            neighbors.push(row);
        }
        Ok(NswGraph {
            m,
            entry,
            neighbors,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

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
        for (a, b) in cat.tables.iter().zip(&restored.tables) {
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
        for i in 0..6 {
            let base = i as f32 * 0.1;
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
        assert_eq!(restored_graph.entry, original.entry);
        assert_eq!(restored_graph.neighbors, original.neighbors);
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
