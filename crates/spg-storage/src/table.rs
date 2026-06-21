//! The `Table` storage object: row insert/update/delete, index
//! construction + rebuild (BTree / BRIN / GIN / GIN-trgm /
//! GIN-fulltext / NSW), cold-locator registration, and schema
//! mutation (add/drop/rename column). Split out of lib.rs (monster
//! tier-3 cut 4). The `Table` struct itself stays in lib.rs as
//! storage vocabulary; this module is the inherent `impl` over it.
//! `Table`'s private fields are reachable here because `table` is a
//! descendant module of the crate root where the struct is declared.

use super::*;

impl Table {
    pub fn new(schema: TableSchema) -> Self {
        Self {
            schema,
            rows: PersistentVec::new(),
            indices: Vec::new(),
            hot_bytes: 0,
            cold_row_count: 0,
            cold_row_count_stale: false,
            redo_log: None,
        }
    }

    /// v7.34 (crash-recovery P0 #2) — start capturing row-level redo into
    /// this table (engine call before a mutating statement when
    /// persistence is on). Idempotent; existing captured changes are kept.
    pub fn enable_redo(&mut self) {
        if self.redo_log.is_none() {
            self.redo_log = Some(Vec::new());
        }
    }

    /// v7.34 — drain the captured redo changes and stop capturing.
    /// Returns the physical [`RowChange`]s applied since `enable_redo`,
    /// in apply order (empty when capture was off or nothing changed).
    pub fn take_redo(&mut self) -> Vec<RowChange> {
        self.redo_log.take().unwrap_or_default()
    }

    /// Record one captured change when redo capture is on. The table name
    /// rides on the change (taken from the schema) so a drained log is
    /// self-describing against the whole catalog.
    fn record_redo(&mut self, make: impl FnOnce(String) -> RowChange) {
        if self.redo_log.is_some() {
            let change = make(self.schema.name.clone());
            if let Some(log) = self.redo_log.as_mut() {
                log.push(change);
            }
        }
    }

    /// Total encoded byte size of every row currently in the hot tier
    /// (`self.rows`). See struct docs for the maintenance contract.
    /// Returns 0 for an empty table.
    #[must_use]
    pub const fn hot_bytes(&self) -> u64 {
        self.hot_bytes
    }

    /// v6.7.0 — cached count of cold-tier rows. See struct field
    /// docs for the staleness contract.
    #[must_use]
    pub const fn cold_row_count(&self) -> u64 {
        self.cold_row_count
    }

    /// v6.7.0 — overwrite the cached count. Called by the engine's
    /// `analyze_one_table` after walking the indices.
    pub fn set_cold_row_count(&mut self, n: u64) {
        self.cold_row_count = n;
        self.cold_row_count_stale = false;
    }

    /// v6.7.0 — mark the cached count as potentially out of date.
    /// Called by freezer / promote / DELETE paths so a subsequent
    /// `spg_statistic` read knows the number may not reflect the
    /// current state.
    pub fn mark_cold_row_count_stale(&mut self) {
        self.cold_row_count_stale = true;
    }

    /// v6.7.0 — report whether the cached count is known to be out
    /// of date. Exposed for completeness; the virtual table surface
    /// returns the cached value regardless.
    #[must_use]
    pub const fn cold_row_count_stale(&self) -> bool {
        self.cold_row_count_stale
    }

    /// v7.36 — O(1) "could this table possibly have cold rows?"
    /// predicate, intended for perf-critical executor hot paths
    /// that just need to skip the cold-tier branch when there's
    /// definitely nothing there. Reads the cached `cold_row_count`:
    ///   - cache fresh + cache == 0 → return false (fast path)
    ///   - cache stale → return true (conservative; the executor
    ///     pays the cold-aware path's `iter_cold_rows_*` cost but
    ///     stays correct)
    ///   - cache fresh + cache > 0 → return true
    /// `count_cold_locators` remains the right call for the EXACT
    /// count (ANALYZE etc.) — its O(N) walk is unsuitable per join
    /// stage.
    #[must_use]
    pub const fn has_cold_rows_fast(&self) -> bool {
        self.cold_row_count_stale || self.cold_row_count > 0
    }

    /// v6.7.0 — walk every BTree index and count `RowLocator::Cold`
    /// entries; return the MAX across indices. The freeze path
    /// (`freeze_oldest_to_cold`) writes cold locators to ONE
    /// designated index — that index ends up with the full per-row
    /// count. MAX-across-indices yields the precise count when a
    /// PK-style index exists; for multi-index tables without a
    /// covering index it's a lower bound (rare in practice).
    /// Caller responsibility: only invoke under `engine.write()`
    /// or after taking ownership; the walk is O(N) over every
    /// (key, locator) pair.
    #[must_use]
    pub fn count_cold_locators(&self) -> u64 {
        let mut best: u64 = 0;
        for idx in &self.indices {
            if let IndexKind::BTree(map) = &idx.kind {
                let n: u64 = map
                    .iter()
                    .map(|(_, locs)| locs.iter().filter(|l| l.is_cold()).count() as u64)
                    .sum();
                if n > best {
                    best = n;
                }
            }
        }
        best
    }

    pub const fn schema(&self) -> &TableSchema {
        &self.schema
    }

    /// v6.7.2 — mutable schema accessor for ALTER TABLE paths.
    /// Used by `Engine::exec_alter_table` to flip per-table
    /// settings like `hot_tier_bytes`.
    pub const fn schema_mut(&mut self) -> &mut TableSchema {
        &mut self.schema
    }

    /// v4.39: returns the persistent row vector by reference. Callers that
    /// used to take `&[Row]` should switch to `.iter()` (via
    /// `IntoIterator for &PersistentVec`) or `.get(i)` for indexing.
    pub const fn rows(&self) -> &PersistentVec<Row<'static>> {
        &self.rows
    }

    pub const fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// v6.8.0 — exposed for the engine layer to patch
    /// `Index::included_columns` post-creation. Could fold into
    /// `add_index` once the engine's IF-NOT-EXISTS guard moves up,
    /// but the patch shape is the minimal change for v6.8.0.
    pub fn indices_mut(&mut self) -> &mut [Index] {
        &mut self.indices
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
        // v6.7.1 — prefer BTree (has the key→locator map needed
        // for `lookup_eq`) over BRIN (metadata-only). When only a
        // BRIN exists on the column, return None so the executor
        // falls back to the hot-tier row scan instead of trying
        // to use BRIN for an equality lookup (which would always
        // return an empty slice and look like "no rows matched").
        self.indices
            .iter()
            .find(|i| i.column_position == column_position && matches!(i.kind, IndexKind::BTree(_)))
            .or_else(|| {
                self.indices.iter().find(|i| {
                    i.column_position == column_position && matches!(i.kind, IndexKind::Nsw(_))
                })
            })
    }

    /// Insert one row after validating it matches the schema (length + type).
    /// Returns `StorageError` on mismatch — the table is left unchanged.
    /// Updates every defined index with the new row's key.
    pub fn insert(&mut self, row: Row<'static>) -> Result<(), StorageError> {
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
                        DataType::Varchar(_) | DataType::Char(_) | DataType::Json | DataType::Jsonb
                    ) | (DataType::Json | DataType::Jsonb, DataType::Text)
                        | (DataType::Json, DataType::Jsonb)
                        | (DataType::Jsonb, DataType::Json)
                        | (DataType::Timestamp, DataType::Timestamptz)
                        | (DataType::Timestamptz, DataType::Timestamp)
                        // v7.37.5 ship triage — BIT / VARBIT share the
                        // BitString storage shape; INET / CIDR likewise.
                        | (DataType::Bit, DataType::BitVarying)
                        | (DataType::BitVarying, DataType::Bit)
                        | (DataType::Inet, DataType::Cidr)
                        | (DataType::Cidr, DataType::Inet)
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
            match &mut idx.kind {
                IndexKind::BTree(map) => {
                    if let Some(key) = IndexKey::from_value(&row.values[idx.column_position]) {
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
                IndexKind::Gin(map) => {
                    // v7.12.3 — extend posting list per lexeme word.
                    // NULL or non-TsVector cell → no-op (cell carries
                    // no lexemes to index).
                    if let Value::TsVector(lexemes) = &row.values[idx.column_position] {
                        for lex in lexemes {
                            let mut entries = map.get(&lex.word).cloned().unwrap_or_default();
                            entries.push(RowLocator::Hot(new_row_idx));
                            map.insert_mut(lex.word.clone(), entries);
                        }
                    }
                }
                IndexKind::GinTrgm(map) => {
                    // v7.15.0 — trigram GIN. Shingle the TEXT cell
                    // into PG-compatible 3-byte trigrams and extend
                    // each trigram's posting list.
                    if let Value::Text(s) = &row.values[idx.column_position] {
                        for tri in trgm::extract_trigrams(s) {
                            let mut entries = map.get(&tri).cloned().unwrap_or_default();
                            entries.push(RowLocator::Hot(new_row_idx));
                            map.insert_mut(tri, entries);
                        }
                    }
                }
                IndexKind::GinFulltext(map) => {
                    // v7.17.0 Phase 2.2 — MySQL FULLTEXT-shape
                    // GIN over a TEXT / VARCHAR cell. Tokenise
                    // via the storage-local `simple_lex` (same
                    // rule as `to_tsvector('simple', text)`) and
                    // extend each lexeme's posting list.
                    let text_cell = match &row.values[idx.column_position] {
                        Value::Text(s) => Some(s.as_ref()),
                        // mysqldump-style mediumtext / longtext
                        // land as Value::Text on insert; varchar
                        // cells likewise. Anything else (NULL,
                        // integer, …) contributes no lexemes.
                        _ => None,
                    };
                    if let Some(s) = text_cell {
                        for lex in fts_simple::simple_lex(s) {
                            let mut entries = map.get(&lex).cloned().unwrap_or_default();
                            entries.push(RowLocator::Hot(new_row_idx));
                            map.insert_mut(lex, entries);
                        }
                    }
                }
                IndexKind::GinJsonb(map) => {
                    // v7.37.8(sentori Epic 5 P2)— real JSONB-GIN.
                    // Extract canonical `(path, leaf)` tokens from
                    // the cell text and extend each token's posting
                    // list. NULL or non-Json cell contributes no
                    // tokens(`labels @> '...'` against a NULL row
                    // is always false so absence here is correct).
                    let json_cell = match &row.values[idx.column_position] {
                        Value::Json(s) => Some(s.as_ref()),
                        _ => None,
                    };
                    if let Some(s) = json_cell {
                        for tok in jsonb_gin::extract_tokens(s) {
                            let mut entries = map.get(&tok).cloned().unwrap_or_default();
                            entries.push(RowLocator::Hot(new_row_idx));
                            map.insert_mut(tok, entries);
                        }
                    }
                }
                // NSW handled below after the row push (so the new row
                // is visible to the kNN-graph connect step). BRIN
                // carries no per-row state.
                IndexKind::Nsw(_) | IndexKind::Brin { .. } => {}
            }
        }
        // v5.2.1: maintain incremental hot-tier byte counter. Computed
        // before the move so we don't need to borrow `row` after push.
        self.hot_bytes = self
            .hot_bytes
            .saturating_add(row_body_encoded_len(&row, &self.schema) as u64);
        // v7.34 — capture the row-level redo before the row is moved in.
        self.record_redo(|table| RowChange::Insert {
            table,
            row: row.clone(),
        });
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
            IndexKind::BTree(_)
            | IndexKind::Brin { .. }
            | IndexKind::Gin(_)
            | IndexKind::GinTrgm(_)
            | IndexKind::GinFulltext(_)
            | IndexKind::GinJsonb(_) => {
                return Err(StorageError::Unsupported(format!(
                    "ALTER INDEX REBUILD on non-NSW index {name:?} — only NSW indexes can rebuild"
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
            included_columns: Vec::new(),
            partial_predicate: None,
            expression: None,
            is_unique: false,
            extra_column_positions: Vec::new(),
        });
        Ok(())
    }

    /// v6.7.1 — public restore counterpart for BRIN indices. Used
    /// by `Catalog::deserialize` when a v10 snapshot carries a
    /// BRIN index entry. BRIN carries no in-memory data — only the
    /// `column_type` snapshot is restored.
    pub fn restore_brin_index(
        &mut self,
        name: String,
        column_name: &str,
        column_type: DataType,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        self.indices
            .push(Index::new_brin(name, column_position, column_type));
        Ok(())
    }

    /// v6.7.1 — public CREATE INDEX counterpart for BRIN. Creates
    /// the index entry with a snapshot of the indexed column's
    /// current `DataType`.
    pub fn add_brin_index(&mut self, name: String, column_name: &str) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        let column_type = self.schema.columns[column_position].ty;
        self.indices
            .push(Index::new_brin(name, column_position, column_type));
        Ok(())
    }

    /// v7.12.3 — Build a new GIN inverted index over a `tsvector`
    /// column. Populates posting lists from existing rows. Errors
    /// if the column doesn't exist, isn't `TsVector`, or the index
    /// name is taken.
    pub fn add_gin_index(&mut self, name: String, column_name: &str) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        if self.schema.columns[column_position].ty != DataType::TsVector {
            return Err(StorageError::Corrupt(format!(
                "GIN index {name:?} requires a tsvector column; \
                 {column_name:?} is {:?}",
                self.schema.columns[column_position].ty
            )));
        }
        let mut idx = Index::new_gin(name, column_position);
        if let IndexKind::Gin(map) = &mut idx.kind {
            for (i, row) in self.rows.iter().enumerate() {
                if let Value::TsVector(lexemes) = &row.values[column_position] {
                    for lex in lexemes {
                        let mut entries = map.get(&lex.word).cloned().unwrap_or_default();
                        entries.push(RowLocator::Hot(i));
                        map.insert_mut(lex.word.clone(), entries);
                    }
                }
            }
        }
        self.indices.push(idx);
        Ok(())
    }

    /// v7.12.3 — Restore a GIN index from a deserialised snapshot.
    /// Mirrors [`Self::restore_btree_index`] but takes the GIN's
    /// `word → Vec<RowLocator>` posting-list map (already populated
    /// from the catalog stream) instead of an `IndexKey` map.
    pub fn restore_gin_index(
        &mut self,
        name: String,
        column_name: &str,
        map: PersistentBTreeMap<String, Vec<RowLocator>>,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        let mut idx = Index::new_gin(name, column_position);
        idx.kind = IndexKind::Gin(map);
        self.indices.push(idx);
        Ok(())
    }

    /// v7.15.0 — `gin_trgm_ops` GIN over a TEXT column. Walks
    /// every row, shingles the cell into PG-compatible trigrams,
    /// and builds the posting-list map. NULL / non-TEXT cells
    /// contribute nothing (no trigrams).
    pub fn add_gin_trgm_index(
        &mut self,
        name: String,
        column_name: &str,
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
            DataType::Text | DataType::Varchar(_)
        ) {
            return Err(StorageError::Corrupt(format!(
                "trigram-GIN index {name:?} requires a TEXT/VARCHAR column; \
                 {column_name:?} is {:?}",
                self.schema.columns[column_position].ty
            )));
        }
        let mut idx = Index::new_gin_trgm(name, column_position);
        if let IndexKind::GinTrgm(map) = &mut idx.kind {
            for (i, row) in self.rows.iter().enumerate() {
                if let Value::Text(s) = &row.values[column_position] {
                    for tri in trgm::extract_trigrams(s) {
                        let mut entries = map.get(&tri).cloned().unwrap_or_default();
                        entries.push(RowLocator::Hot(i));
                        map.insert_mut(tri, entries);
                    }
                }
            }
        }
        self.indices.push(idx);
        Ok(())
    }

    /// v7.15.0 — restore a trigram-GIN from its catalog snapshot
    /// payload. Mirrors [`Self::restore_gin_index`].
    pub fn restore_gin_trgm_index(
        &mut self,
        name: String,
        column_name: &str,
        map: PersistentBTreeMap<String, Vec<RowLocator>>,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        let mut idx = Index::new_gin_trgm(name, column_position);
        idx.kind = IndexKind::GinTrgm(map);
        self.indices.push(idx);
        Ok(())
    }

    /// v7.17.0 Phase 2.2 — MySQL `FULLTEXT KEY` GIN over a TEXT
    /// column. Walks every row, tokenises the cell into lower-
    /// cased word lexemes (`fts_simple::simple_lex` — same rule
    /// as `to_tsvector('simple', text)`), and builds the
    /// posting-list map. NULL / non-TEXT cells contribute
    /// nothing (no lexemes).
    pub fn add_gin_fulltext_index(
        &mut self,
        name: String,
        column_name: &str,
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
            DataType::Text | DataType::Varchar(_)
        ) {
            return Err(StorageError::Corrupt(format!(
                "fulltext-GIN index {name:?} requires a TEXT/VARCHAR column; \
                 {column_name:?} is {:?}",
                self.schema.columns[column_position].ty
            )));
        }
        let mut idx = Index::new_gin_fulltext(name, column_position);
        if let IndexKind::GinFulltext(map) = &mut idx.kind {
            for (i, row) in self.rows.iter().enumerate() {
                if let Value::Text(s) = &row.values[column_position] {
                    for lex in fts_simple::simple_lex(s) {
                        let mut entries = map.get(&lex).cloned().unwrap_or_default();
                        entries.push(RowLocator::Hot(i));
                        map.insert_mut(lex, entries);
                    }
                }
            }
        }
        self.indices.push(idx);
        Ok(())
    }

    /// v7.17.0 Phase 2.2 — restore a fulltext-GIN from its
    /// catalog snapshot payload. Mirrors
    /// [`Self::restore_gin_trgm_index`].
    pub fn restore_gin_fulltext_index(
        &mut self,
        name: String,
        column_name: &str,
        map: PersistentBTreeMap<String, Vec<RowLocator>>,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        let mut idx = Index::new_gin_fulltext(name, column_position);
        idx.kind = IndexKind::GinFulltext(map);
        self.indices.push(idx);
        Ok(())
    }

    /// v7.37.8(sentori Epic 5 P2)— JSONB-GIN over a `Json` /
    /// `Jsonb` column. Walks every row, extracts canonical
    /// `(path, leaf)` tokens via
    /// [`crate::jsonb_gin::extract_tokens`], and builds the
    /// posting-list map. NULL or non-Json cells contribute no
    /// tokens(`<col> @> <jsonb>` against a NULL row is always
    /// false so absence here is correct).
    pub fn add_gin_jsonb_index(
        &mut self,
        name: String,
        column_name: &str,
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
            DataType::Json | DataType::Jsonb
        ) {
            return Err(StorageError::Corrupt(format!(
                "JSONB-GIN index {name:?} requires a JSON/JSONB column; \
                 {column_name:?} is {:?}",
                self.schema.columns[column_position].ty
            )));
        }
        let mut idx = Index::new_gin_jsonb(name, column_position);
        if let IndexKind::GinJsonb(map) = &mut idx.kind {
            for (i, row) in self.rows.iter().enumerate() {
                if let Value::Json(s) = &row.values[column_position] {
                    for tok in jsonb_gin::extract_tokens(s) {
                        let mut entries = map.get(&tok).cloned().unwrap_or_default();
                        entries.push(RowLocator::Hot(i));
                        map.insert_mut(tok, entries);
                    }
                }
            }
        }
        self.indices.push(idx);
        Ok(())
    }

    /// v7.37.8 — restore a JSONB-GIN from its catalog snapshot
    /// payload. Mirrors [`Self::restore_gin_fulltext_index`].
    pub fn restore_gin_jsonb_index(
        &mut self,
        name: String,
        column_name: &str,
        map: PersistentBTreeMap<String, Vec<RowLocator>>,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        let mut idx = Index::new_gin_jsonb(name, column_position);
        idx.kind = IndexKind::GinJsonb(map);
        self.indices.push(idx);
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
            IndexKind::Nsw(_)
            | IndexKind::Brin { .. }
            | IndexKind::Gin(_)
            | IndexKind::GinTrgm(_)
            | IndexKind::GinFulltext(_)
            | IndexKind::GinJsonb(_) => {
                return Err(StorageError::Corrupt(format!(
                    "index {index_name:?} is not BTree; cold locators apply only to BTree indices"
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

    /// v7.12.3 — GIN-side parallel to [`Self::register_cold_locators`].
    /// Re-attaches `word → cold RowLocator` posting-list entries after
    /// the from-rows rebuild loop. Errors when the index doesn't
    /// exist or isn't a GIN. Both tsvector-GIN and trigram-GIN
    /// variants share posting-list shape (`String → Vec<RowLocator>`),
    /// so this helper accepts either.
    pub fn register_gin_cold_locators<I>(
        &mut self,
        index_name: &str,
        locators: I,
    ) -> Result<usize, StorageError>
    where
        I: IntoIterator<Item = (String, RowLocator)>,
    {
        let idx = self
            .indices
            .iter_mut()
            .find(|i| i.name == index_name)
            .ok_or_else(|| StorageError::Corrupt(format!("index {index_name:?} not found")))?;
        let map = match &mut idx.kind {
            // v7.17.0 Phase 2.2 — fulltext-GIN posting lists are
            // shape-compatible with tsvector / trigram GINs, so
            // cold-locator re-attach handles all three.
            // v7.37.8 — JSONB-GIN shares the same posting-list shape,
            // so it joins the same re-attach path.
            IndexKind::Gin(map)
            | IndexKind::GinTrgm(map)
            | IndexKind::GinFulltext(map)
            | IndexKind::GinJsonb(map) => map,
            IndexKind::BTree(_) | IndexKind::Nsw(_) | IndexKind::Brin { .. } => {
                return Err(StorageError::Corrupt(format!(
                    "register_gin_cold_locators: index {index_name:?} is not GIN"
                )));
            }
        };
        let mut count = 0usize;
        for (word, locator) in locators {
            let mut entries = map.get(&word).cloned().unwrap_or_default();
            entries.push(locator);
            map.insert_mut(word, entries);
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
            IndexKind::Nsw(_)
            | IndexKind::Brin { .. }
            | IndexKind::Gin(_)
            | IndexKind::GinTrgm(_)
            | IndexKind::GinFulltext(_)
            | IndexKind::GinJsonb(_) => {
                return Err(StorageError::Corrupt(format!(
                    "remove_cold_locators_for_key: index {index_name:?} is not BTree; \
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

    /// v7.13.0 — append a new column to the schema and back-fill
    /// every existing row with `fill_value`. Used by the engine's
    /// `ALTER TABLE t ADD COLUMN …` handler (mailrs round-5 G1).
    /// Indices on existing columns keep working — column positions
    /// don't shift since the new column lands at the end — so no
    /// index rebuild is needed.
    pub fn add_column(&mut self, col: ColumnSchema, fill_value: Value<'static>) {
        self.schema.columns.push(col);
        let mut new_rows: PersistentVec<Row<'static>> = PersistentVec::new();
        for row in self.rows.iter() {
            let mut values = row.values.clone();
            values.push(fill_value.clone());
            new_rows.push_mut(Row::new(values));
        }
        self.rows = new_rows;
    }

    /// v7.15.0 — replace the partial-index predicate source on
    /// the index at slot `idx`. Used by `ALTER TABLE … RENAME
    /// COLUMN` after the engine rewrites column-identifier
    /// references in the predicate source text. Pure metadata
    /// edit; index rows are unaffected (they're keyed by
    /// column position, not predicate text).
    pub fn set_partial_predicate(&mut self, idx: usize, pred: Option<String>) {
        debug_assert!(idx < self.indices.len());
        self.indices[idx].partial_predicate = pred;
    }

    /// v7.15.0 — rename the column at `col_pos` to `new_name`.
    /// The on-disk row encoding is positional, so no row rewrite
    /// is needed; only the schema's column name changes. Indices,
    /// UCs, FKs all key off column positions and are unaffected.
    /// Source-text references that hold the column name (CHECK
    /// predicates, partial-index predicates, runtime DEFAULT
    /// expressions, trigger `UPDATE OF` lists) are rewritten by
    /// the engine before this helper is called — the storage
    /// layer doesn't depend on `spg-sql` and so can't re-parse the
    /// predicate sources itself.
    pub fn rename_column(&mut self, col_pos: usize, new_name: &str) {
        debug_assert!(col_pos < self.schema.columns.len());
        self.schema.columns[col_pos].name = new_name.to_string();
    }

    /// v7.13.3 — drop the column at `col_pos`. Removes the entry
    /// from the schema, the value from every row, any index that
    /// references the column (pure drop, not shift), and shifts
    /// every remaining index/UC/FK column position that pointed
    /// past `col_pos` down by one. Used by `ALTER TABLE t DROP
    /// COLUMN <c>` (mailrs round-7 S8). FK dependents on this
    /// column must already have been removed by the caller (CASCADE
    /// path); the helper assumes only same-column index removal is
    /// needed.
    pub fn drop_column(&mut self, col_pos: usize) {
        debug_assert!(col_pos < self.schema.columns.len());
        // Strip the column from the schema.
        self.schema.columns.remove(col_pos);
        // Rewrite every row to omit the cell at col_pos.
        let mut new_rows: PersistentVec<Row> = PersistentVec::new();
        for row in self.rows.iter() {
            let mut values = row.values.clone();
            if col_pos < values.len() {
                values.remove(col_pos);
            }
            new_rows.push_mut(Row::new(values));
        }
        self.rows = new_rows;
        // Drop indices on the column outright; shift the rest.
        self.indices.retain(|idx| idx.column_position != col_pos);
        for idx in &mut self.indices {
            if idx.column_position > col_pos {
                idx.column_position -= 1;
            }
            // Same shift for any included-columns reference.
            for inc in &mut idx.included_columns {
                if *inc > col_pos {
                    *inc -= 1;
                }
            }
        }
        // Shift uniqueness-constraint column positions (and drop
        // entries that lose all columns, though that shouldn't
        // happen in practice — caller has already CASCADE-removed
        // FKs and there's no general CASCADE for UCs).
        let mut surviving_ucs: Vec<UniquenessConstraint> = Vec::new();
        for mut uc in core::mem::take(&mut self.schema.uniqueness_constraints) {
            uc.columns.retain(|&c| c != col_pos);
            if uc.columns.is_empty() {
                continue;
            }
            for c in &mut uc.columns {
                if *c > col_pos {
                    *c -= 1;
                }
            }
            surviving_ucs.push(uc);
        }
        self.schema.uniqueness_constraints = surviving_ucs;
        // Shift FK local_columns (parent-pointing column positions
        // are off-table and untouched).
        for fk in &mut self.schema.foreign_keys {
            for c in &mut fk.local_columns {
                if *c > col_pos {
                    *c -= 1;
                }
            }
        }
        // Rebuild remaining indices' payload — the column-position
        // shift means existing IndexKey entries are still keyed by
        // the same column data but the position numbers changed;
        // existing key→locator maps stay valid because they're
        // keyed by Value not position. The rebuild is conservative
        // — same pattern delete_rows uses post-mutation.
        self.rebuild_indices();
    }

    /// v4.4: delete the rows at the given positions in one pass.
    /// `positions` must be unique; ordering doesn't matter. Indices
    /// are rebuilt from scratch (cheaper than tracking incremental
    /// shifts across both B-tree and NSW). Returns the number of
    /// rows removed.
    /// v7.17.0 Phase 1.3 — wipe every row. Used by REFRESH
    /// MATERIALIZED VIEW; same effect as `delete_rows((0..N).into())`
    /// but skips the per-position bookkeeping for the all-removed
    /// fast path. Indices are rebuilt (empty).
    pub fn truncate(&mut self) {
        self.rows = PersistentVec::new();
        self.hot_bytes = 0;
        self.rebuild_indices();
    }

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
        // v7.34 — capture row-level redo. Record the input positions
        // (replay's `delete_rows` dedups + bounds-filters identically);
        // skip a no-op delete so the log stays minimal.
        if removed > 0 {
            self.record_redo(|table| RowChange::Delete {
                table,
                positions: positions.to_vec(),
            });
        }
        removed
    }

    /// v4.4: replace the row at `position` with `new_values` (must
    /// match the schema arity + types). v7.20: index maintenance is
    /// incremental — only indices whose key value changed are
    /// touched (B-tree entry move in place; NSW / BRIN / GIN fall
    /// back to a full rebuild when their column changed).
    pub fn update_row(
        &mut self,
        position: usize,
        new_values: Vec<Value<'static>>,
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
                        DataType::Varchar(_) | DataType::Char(_) | DataType::Json | DataType::Jsonb
                    ) | (DataType::Json | DataType::Jsonb, DataType::Text)
                        | (DataType::Json, DataType::Jsonb)
                        | (DataType::Jsonb, DataType::Json)
                        | (DataType::Timestamp, DataType::Timestamptz)
                        | (DataType::Timestamptz, DataType::Timestamp)
                        // v7.37.5 ship triage — BIT / VARBIT share the
                        // BitString storage shape; INET / CIDR likewise.
                        | (DataType::Bit, DataType::BitVarying)
                        | (DataType::BitVarying, DataType::Bit)
                        | (DataType::Inet, DataType::Cidr)
                        | (DataType::Cidr, DataType::Inet)
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
        // v7.20 P4 — incremental index maintenance. `rows.set`
        // replaces the row in place, so every OTHER row's Hot
        // locator stays valid; only indices whose key value
        // actually changed at `position` need touching. The
        // common OLTP shape (`UPDATE … SET non_indexed_col = …
        // WHERE pk = $1`) touches no index at all — pre-v7.20
        // this path paid a full rebuild_indices() (O(rows ×
        // indices)) per UPDATE, which dominated the profiled
        // write cost on a 5k-row table (~1 ms/stmt).
        //
        // BTree gets an in-place entry move (drop Hot(position)
        // from the old key's locator list, append to the new
        // key's). NSW graphs / BRIN summaries / GIN posting
        // lists have no cheap single-key move — a changed column
        // under one of those falls back to the full rebuild.
        enum IdxFix {
            BTreeMove {
                idx_pos: usize,
                old_key: Option<IndexKey>,
                new_key: Option<IndexKey>,
            },
            FullRebuild,
        }
        let mut fixes: Vec<IdxFix> = Vec::new();
        for (idx_pos, idx) in self.indices.iter().enumerate() {
            let col = idx.column_position;
            let old_v = &old_row.values[col];
            let new_v = &new_row.values[col];
            if old_v == new_v {
                continue;
            }
            match &idx.kind {
                IndexKind::BTree(_) => fixes.push(IdxFix::BTreeMove {
                    idx_pos,
                    old_key: IndexKey::from_value(old_v),
                    new_key: IndexKey::from_value(new_v),
                }),
                IndexKind::Nsw(_)
                | IndexKind::Brin { .. }
                | IndexKind::Gin(_)
                | IndexKind::GinTrgm(_)
                | IndexKind::GinFulltext(_)
                | IndexKind::GinJsonb(_) => {
                    fixes.clear();
                    fixes.push(IdxFix::FullRebuild);
                    break;
                }
            }
        }
        self.rows = self
            .rows
            .set(position, new_row)
            .expect("position bounds-checked above");
        self.hot_bytes = self
            .hot_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        // v7.34 — capture row-level redo (after the row is in place; the
        // immutable read of the new values is dropped before record_redo's
        // mutable borrow, and gated so capture-off pays nothing).
        if self.redo_log.is_some() {
            let new_row = self
                .rows
                .get(position)
                .map(|r| r.values.clone())
                .unwrap_or_default();
            self.record_redo(|table| RowChange::Update {
                table,
                pos: position,
                new_row,
            });
        }
        for fix in fixes {
            match fix {
                IdxFix::FullRebuild => {
                    self.rebuild_indices();
                    break;
                }
                IdxFix::BTreeMove {
                    idx_pos,
                    old_key,
                    new_key,
                } => {
                    let IndexKind::BTree(map) = &mut self.indices[idx_pos].kind else {
                        unreachable!("IdxFix::BTreeMove built from a BTree index");
                    };
                    // NULL keys never enter the B-tree (from_value
                    // returns None), so a None on either side means
                    // "no entry on that side".
                    if let Some(k) = old_key
                        && let Some(locs) = map.get(&k)
                    {
                        let mut locs = locs.clone();
                        locs.retain(|l| *l != RowLocator::Hot(position));
                        // No remove_mut on the persistent map: an
                        // empty locator list is the tombstone —
                        // lookup_eq returns an empty slice, and the
                        // next rebuild_indices() drops the key.
                        map.insert_mut(k, locs);
                    }
                    if let Some(k) = new_key {
                        let mut entries = map.get(&k).cloned().unwrap_or_default();
                        entries.push(RowLocator::Hot(position));
                        map.insert_mut(k, entries);
                    }
                }
            }
        }
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
                // BRIN / NSW carry no key→locator map. GIN handles
                // its own cold preservation below in `preserved_gin_cold`.
                IndexKind::Nsw(_)
                | IndexKind::Brin { .. }
                | IndexKind::Gin(_)
                | IndexKind::GinTrgm(_)
                | IndexKind::GinFulltext(_)
                | IndexKind::GinJsonb(_) => None,
            })
            .collect();

        // v7.12.3 — same cold-preservation pattern for GIN's
        // `word → Vec<RowLocator>` posting lists. Parallel to the
        // BTree pass above (different key type so a separate vec is
        // cleaner than a generic merge). v7.15.0: trigram-GIN
        // (`gin_trgm_ops`) shares the same posting-list shape, so
        // one pass handles both — the `RebuildKind` carries the
        // kind tag to drive resurrection.
        let preserved_gin_cold: Vec<(String, Vec<(String, RowLocator)>)> = self
            .indices
            .iter()
            .filter_map(|idx| match &idx.kind {
                // v7.17.0 Phase 2.2 — fulltext-GIN posting lists
                // share the `String → Vec<RowLocator>` shape, so
                // cold preservation handles all three GIN flavours
                // in one pass.
                IndexKind::Gin(map)
                | IndexKind::GinTrgm(map)
                | IndexKind::GinFulltext(map)
                | IndexKind::GinJsonb(map) => {
                    let cold: Vec<(String, RowLocator)> = map
                        .iter()
                        .flat_map(|(w, locs)| {
                            locs.iter()
                                .filter(|l| l.is_cold())
                                .copied()
                                .map(move |l| (w.clone(), l))
                        })
                        .collect();
                    if cold.is_empty() {
                        None
                    } else {
                        Some((idx.name.clone(), cold))
                    }
                }
                IndexKind::BTree(_) | IndexKind::Nsw(_) | IndexKind::Brin { .. } => None,
            })
            .collect();

        // v6.7.1 — descriptor needs to capture index kind so the
        // rebuild loop can resurrect BTree / NSW / BRIN / GIN exactly
        // as they were. (NSW carries m; BRIN carries the column type
        // snapshot; BTree / GIN need no extra payload.)
        #[derive(Clone)]
        enum RebuildKind {
            BTree,
            Nsw(usize),
            Brin(DataType),
            Gin,
            GinTrgm,
            GinFulltext,
            GinJsonb,
        }
        let descriptors: Vec<(String, usize, RebuildKind)> = self
            .indices
            .iter()
            .map(|idx| {
                let kind = match &idx.kind {
                    IndexKind::Nsw(g) => RebuildKind::Nsw(g.m),
                    IndexKind::Brin { column_type } => RebuildKind::Brin(*column_type),
                    IndexKind::BTree(_) => RebuildKind::BTree,
                    IndexKind::Gin(_) => RebuildKind::Gin,
                    IndexKind::GinTrgm(_) => RebuildKind::GinTrgm,
                    IndexKind::GinFulltext(_) => RebuildKind::GinFulltext,
                    IndexKind::GinJsonb(_) => RebuildKind::GinJsonb,
                };
                (idx.name.clone(), idx.column_position, kind)
            })
            .collect();
        self.indices.clear();
        for (name, column_position, rebuild_kind) in descriptors {
            match rebuild_kind {
                RebuildKind::Nsw(m) => {
                    let idx = Index::new_nsw(name, column_position, m);
                    self.indices.push(idx);
                    let idx_pos = self.indices.len() - 1;
                    let row_indices: Vec<usize> = (0..self.rows.len()).collect();
                    for row_idx in row_indices {
                        nsw_insert_at(self, idx_pos, row_idx);
                    }
                }
                RebuildKind::Brin(column_type) => {
                    // BRIN has no in-memory rebuild — the summaries
                    // live in cold segments which freeze emits.
                    self.indices
                        .push(Index::new_brin(name, column_position, column_type));
                }
                RebuildKind::BTree => {
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
                RebuildKind::Gin => {
                    let mut idx = Index::new_gin(name, column_position);
                    if let IndexKind::Gin(map) = &mut idx.kind {
                        for (i, row) in self.rows.iter().enumerate() {
                            if let Value::TsVector(lexemes) = &row.values[column_position] {
                                for lex in lexemes {
                                    let mut entries =
                                        map.get(&lex.word).cloned().unwrap_or_default();
                                    entries.push(RowLocator::Hot(i));
                                    map.insert_mut(lex.word.clone(), entries);
                                }
                            }
                        }
                    }
                    self.indices.push(idx);
                }
                RebuildKind::GinTrgm => {
                    let mut idx = Index::new_gin_trgm(name, column_position);
                    if let IndexKind::GinTrgm(map) = &mut idx.kind {
                        for (i, row) in self.rows.iter().enumerate() {
                            if let Value::Text(s) = &row.values[column_position] {
                                for tri in trgm::extract_trigrams(s) {
                                    let mut entries = map.get(&tri).cloned().unwrap_or_default();
                                    entries.push(RowLocator::Hot(i));
                                    map.insert_mut(tri, entries);
                                }
                            }
                        }
                    }
                    self.indices.push(idx);
                }
                RebuildKind::GinFulltext => {
                    // v7.17.0 Phase 2.2 — re-derive the lexeme
                    // posting list from each TEXT/VARCHAR cell.
                    // Mirrors the GinTrgm rebuild shape but
                    // tokenises via `fts_simple::simple_lex`
                    // (same rule as `to_tsvector('simple')`).
                    let mut idx = Index::new_gin_fulltext(name, column_position);
                    if let IndexKind::GinFulltext(map) = &mut idx.kind {
                        for (i, row) in self.rows.iter().enumerate() {
                            if let Value::Text(s) = &row.values[column_position] {
                                for lex in fts_simple::simple_lex(s) {
                                    let mut entries = map.get(&lex).cloned().unwrap_or_default();
                                    entries.push(RowLocator::Hot(i));
                                    map.insert_mut(lex, entries);
                                }
                            }
                        }
                    }
                    self.indices.push(idx);
                }
                RebuildKind::GinJsonb => {
                    // v7.37.8 — re-derive the JSONB posting list
                    // from each `Value::Json` cell.
                    let mut idx = Index::new_gin_jsonb(name, column_position);
                    if let IndexKind::GinJsonb(map) = &mut idx.kind {
                        for (i, row) in self.rows.iter().enumerate() {
                            if let Value::Json(s) = &row.values[column_position] {
                                for tok in jsonb_gin::extract_tokens(s) {
                                    let mut entries = map.get(&tok).cloned().unwrap_or_default();
                                    entries.push(RowLocator::Hot(i));
                                    map.insert_mut(tok, entries);
                                }
                            }
                        }
                    }
                    self.indices.push(idx);
                }
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
        // v7.12.3 — same for GIN posting-list cold locators.
        for (idx_name, locators) in preserved_gin_cold {
            let _ = self.register_gin_cold_locators(&idx_name, locators);
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
                included_columns: Vec::new(),
                partial_predicate: None,
                expression: None,
                is_unique: false,
                extra_column_positions: Vec::new(),
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
fn recode_vector_cell(
    cell: Value<'static>,
    target: VecEncoding,
) -> Result<Value<'static>, StorageError> {
    if matches!(cell, Value::Null) {
        return Ok(cell);
    }
    // Step 1 — extract the f32 representation of the source cell.
    let as_f32: Vec<f32> = match &cell {
        Value::Vector(v) => v.to_vec(),
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
        VecEncoding::F32 => Value::Vector(Cow::Owned(as_f32)),
        VecEncoding::Sq8 => Value::Sq8Vector(quantize::quantize(&as_f32)),
        VecEncoding::F16 => Value::HalfVector(halfvec::HalfVector::from_f32_slice(&as_f32)),
    })
}
