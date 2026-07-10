//! v7.37.22 (22.4) — PG amcheck extension equivalents.
//!
//! PG's `amcheck` extension exposes `bt_index_check(regclass)` and
//! `verify_heapam(regclass)` for structural-integrity validation
//! that complements pg_dump's logical round-trip. SPG's storage
//! model differs (PersistentVec rows + parallel RowHeader vec; no
//! shared-buffer pages; CoW-2 snapshot under v7.37.15 MVCC), so
//! the actual checks differ — but the PG-compatible function names
//! plus return-NULL-on-success contract let monitoring queries
//! moves over without changes.
//!
//! Checks performed:
//! - heap (`check_heap_invariants`):
//!     - headers.len() == rows.len() lock-step (v7.37.15 Phase A
//!       invariant; first thing to break under a half-applied
//!       INSERT / restore bug)
//!     - schema column count agrees with row width on a sample
//!       of rows
//! - btree (`check_btree_indices`):
//!     - column_position < schema.columns.len() for every index
//!     - included_columns positions all in-range
//!     - is_unique implies a non-overlapping invariant — we only
//!       check that the index name is not empty (deeper traversal
//!       lands when v7.37.17 wires per-AM probes)
//!
//! Each entry point returns `Ok(())` on a clean check or
//! `Err(message)` with a human-readable description of the first
//! issue found. The caller surfaces NULL or the message as a TEXT
//! scalar.

use alloc::format;
use alloc::string::{String, ToString};

use spg_storage::Catalog;

/// Check heap-level invariants for `table_name`. Returns `Ok(())`
/// on success, `Err(msg)` for the first issue.
///
/// Errors out with a clear message when the table doesn't exist
/// (matching PG's behaviour: `verify_heapam('does_not_exist')`
/// raises).
pub fn check_heap_invariants(catalog: &Catalog, table_name: &str) -> Result<(), String> {
    let table = catalog
        .get(table_name)
        .ok_or_else(|| format!("table {table_name:?} does not exist"))?;
    let row_len = table.rows().len();
    let header_len = table.headers().len();
    if row_len != header_len {
        return Err(format!(
            "header/row lock-step violated for {table_name:?}: \
             rows.len()={row_len} headers.len()={header_len}"
        ));
    }
    let expected_cols = table.schema().columns.len();
    // Sample up to the first 16 rows: their value vector must
    // match the schema column count. A truncated row is the
    // single most common corruption mode under a half-applied
    // ALTER TABLE ADD COLUMN.
    for (i, row) in table.rows().iter().take(16).enumerate() {
        if row.values.len() != expected_cols {
            return Err(format!(
                "row {i} of {table_name:?} has {} values, expected {} \
                 (schema column count)",
                row.values.len(),
                expected_cols
            ));
        }
    }
    Ok(())
}

/// Check BTree index structural invariants for every index on
/// `table_name`. Returns `Ok(())` on success.
pub fn check_btree_indices(catalog: &Catalog, table_name: &str) -> Result<(), String> {
    let table = catalog
        .get(table_name)
        .ok_or_else(|| format!("table {table_name:?} does not exist"))?;
    let n_cols = table.schema().columns.len();
    for idx in table.indices() {
        if idx.name.is_empty() {
            return Err(format!("index on {table_name:?} has empty name"));
        }
        if idx.column_position >= n_cols {
            return Err(format!(
                "index {:?}: column_position {} ≥ table column count {n_cols}",
                idx.name, idx.column_position
            ));
        }
        for &extra in &idx.extra_column_positions {
            if extra >= n_cols {
                return Err(format!(
                    "index {:?}: extra_column_position {extra} ≥ {n_cols}",
                    idx.name
                ));
            }
        }
        for &incl in &idx.included_columns {
            if incl >= n_cols {
                return Err(format!(
                    "index {:?}: included_column position {incl} ≥ {n_cols}",
                    idx.name
                ));
            }
        }
    }
    let _ = table.indices().len().to_string(); // silence unused-arg lint on no-index tables
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use spg_storage::{ColumnSchema, DataType, TableSchema};

    fn fresh_catalog_with_table(name: &str, cols: Vec<(&str, DataType)>) -> Catalog {
        let mut cat = Catalog::new();
        let col_schemas: alloc::vec::Vec<ColumnSchema> = cols
            .into_iter()
            .map(|(n, ty)| ColumnSchema::new(n, ty, false))
            .collect();
        cat.create_table(TableSchema::new(name, col_schemas))
            .unwrap();
        cat
    }

    #[test]
    fn heap_check_passes_on_fresh_table() {
        let cat = fresh_catalog_with_table("t", alloc::vec![("id", DataType::Int)]);
        assert!(check_heap_invariants(&cat, "t").is_ok());
    }

    #[test]
    fn heap_check_fails_on_missing_table() {
        let cat = Catalog::new();
        let err = check_heap_invariants(&cat, "nope").unwrap_err();
        assert!(err.contains("does not exist"), "msg: {err}");
    }

    #[test]
    fn btree_check_passes_when_no_indices() {
        let cat = fresh_catalog_with_table("t", alloc::vec![("id", DataType::Int)]);
        assert!(check_btree_indices(&cat, "t").is_ok());
    }

    #[test]
    fn btree_check_fails_on_missing_table() {
        let cat = Catalog::new();
        let err = check_btree_indices(&cat, "nope").unwrap_err();
        assert!(err.contains("does not exist"), "msg: {err}");
    }
}
