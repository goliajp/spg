//! Write-time constraint enforcement split out of `lib.rs`: foreign-key
//! resolution / enforcement (resolve_foreign_key, enforce_fk_inserts,
//! plan_fk_parent_deletions / plan_fk_parent_updates, apply_fk_child_step,
//! the cascade helpers), UNIQUE / PK enforcement
//! (enforce_unique_index_inserts, enforce_uniqueness_inserts,
//! check_existing_unique_violation), CHECK constraints
//! (enforce_check_constraints), and ON CONFLICT resolution
//! (resolve_on_conflict_columns, apply_on_conflict_assignments, the
//! upsert key-lookup helpers). All free functions taking an explicit
//! catalog so callers with an active `&mut Table` borrow can use them;
//! the DML / DDL execution paths in `dml.rs` / `ddl.rs` drive them.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::Expr;
use spg_storage::{Catalog, ColumnSchema, Row, StorageError, Value};

use crate::aggregate;
use crate::eval::{self, EvalError};
use crate::{Engine, EngineError, check_unsigned_range, coerce_value, value_to_literal_expr};

/// v7.38 — builds an index key string for a row, or `None` when the row is
/// absent from the index (NULL key, or a false partial predicate).
type KeyStrFn<'a> = dyn Fn(&[Value<'static>]) -> Result<Option<String>, EngineError> + 'a;

/// v7.6.1 — resolve a parser-level `ForeignKeyConstraint` (column
/// names + parent table name) into the storage-layer shape (column
/// indices + same parent table). Validates everything the engine
/// needs to know about the FK at CREATE TABLE time:
///
///   - parent table exists (catalog lookup, unless self-referencing)
///   - parent columns exist on the parent table
///   - parent column list matches the local arity (defaults to the
///     parent's primary index column when omitted)
///   - parent columns are covered by a `BTree` UNIQUE-class index
///     (SPG's stand-in for `PRIMARY KEY`/`UNIQUE`) — required so
///     the v7.6.2 INSERT path can do an O(log n) parent lookup
///   - local columns exist on the table being created
pub(crate) fn resolve_foreign_key(
    local_table_name: &str,
    local_cols: &[ColumnSchema],
    fk: spg_sql::ast::ForeignKeyConstraint,
    catalog: &Catalog,
) -> Result<spg_storage::ForeignKeyConstraint, EngineError> {
    // Resolve local columns.
    let mut local_columns = Vec::with_capacity(fk.columns.len());
    for name in &fk.columns {
        let pos = local_cols
            .iter()
            .position(|c| c.name == *name)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "FOREIGN KEY references unknown local column {name:?}"
                ))
            })?;
        local_columns.push(pos);
    }
    // Self-referencing FK: parent table is the one we're creating.
    // The parent column resolution uses the local column list since
    // the catalog doesn't have this table yet.
    let is_self_ref = fk.parent_table == local_table_name;
    let (parent_cols_for_lookup, parent_table_str): (&[ColumnSchema], &str) = if is_self_ref {
        (local_cols, local_table_name)
    } else {
        let parent_table = catalog.get(&fk.parent_table).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound {
                name: fk.parent_table.clone(),
            })
        })?;
        (
            parent_table.schema().columns.as_slice(),
            fk.parent_table.as_str(),
        )
    };
    // Resolve parent column names → positions. If the FK omitted the
    // parent column list, fall back to the parent's primary index
    // column (single-column only — composite default is rejected
    // because there's no unambiguous "PK" in SPG's index list).
    let parent_columns: Vec<usize> = if fk.parent_columns.is_empty() {
        if fk.columns.len() != 1 {
            return Err(EngineError::Unsupported(
                "composite FOREIGN KEY without explicit parent column list is not supported \
                 — list the parent columns explicitly"
                    .into(),
            ));
        }
        // Find a single BTree index on the parent and use its column.
        let pos = pick_pk_index_column(catalog, parent_table_str, is_self_ref, local_cols)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "parent table {parent_table_str:?} has no PRIMARY-key / UNIQUE BTree index \
                     to default the FOREIGN KEY against"
                ))
            })?;
        alloc::vec![pos]
    } else {
        let mut out = Vec::with_capacity(fk.parent_columns.len());
        for name in &fk.parent_columns {
            let pos = parent_cols_for_lookup
                .iter()
                .position(|c| c.name == *name)
                .ok_or_else(|| {
                    EngineError::Unsupported(alloc::format!(
                        "FOREIGN KEY references unknown parent column \
                         {name:?} on table {parent_table_str:?}"
                    ))
                })?;
            out.push(pos);
        }
        out
    };
    if parent_columns.len() != local_columns.len() {
        return Err(EngineError::Unsupported(alloc::format!(
            "FOREIGN KEY arity mismatch: {} local columns vs {} parent columns",
            local_columns.len(),
            parent_columns.len()
        )));
    }
    // For non-self-referencing FKs, verify the parent column set is
    // covered by a BTree index. SPG doesn't have a `PRIMARY KEY`
    // declaration; the convention is "the parent column for FK
    // purposes must have a BTree index" — which the user creates via
    // `CREATE INDEX ... USING btree (col)` (the default). We accept
    // any single-column BTree index that covers a parent column;
    // composite parent column lists require an index whose `column_position`
    // matches the first parent column (multi-column BTree indices
    // are not in the v7.x roadmap).
    if !is_self_ref {
        let parent_table = catalog.get(&fk.parent_table).expect("checked above");
        let primary_parent_col = parent_columns[0];
        let has_btree = parent_table
            .schema()
            .columns
            .get(primary_parent_col)
            .is_some()
            && parent_table.indices().iter().any(|idx| {
                matches!(idx.kind, spg_storage::IndexKind::BTree(_))
                    && idx.column_position == primary_parent_col
                    && idx.partial_predicate.is_none()
            });
        if !has_btree {
            return Err(EngineError::Unsupported(alloc::format!(
                "FOREIGN KEY parent column on {:?} is not covered by an unconditional BTree \
                 index — create one with `CREATE INDEX ... ON {} ({})` first",
                parent_table_str,
                parent_table_str,
                parent_table.schema().columns[primary_parent_col].name,
            )));
        }
    }
    let on_delete = fk_action_sql_to_storage(fk.on_delete);
    let on_update = fk_action_sql_to_storage(fk.on_update);
    let match_type = match fk.match_type {
        spg_sql::ast::MatchType::Simple => spg_storage::MatchType::Simple,
        spg_sql::ast::MatchType::Full => spg_storage::MatchType::Full,
    };
    Ok(spg_storage::ForeignKeyConstraint {
        name: fk.name,
        local_columns,
        parent_table: fk.parent_table,
        parent_columns,
        on_delete,
        on_update,
        match_type,
    })
}

/// v7.6.1 — pick a sentinel "primary key" column from the parent
/// table when the FK didn't name parent columns. Picks the first
/// single-column unconditional BTree index — that's the closest
/// thing SPG has to a PRIMARY KEY today. Self-referencing FKs use
/// `local_cols` as the column source.
fn pick_pk_index_column(
    catalog: &Catalog,
    parent_name: &str,
    is_self_ref: bool,
    local_cols: &[ColumnSchema],
) -> Option<usize> {
    if is_self_ref {
        // Self-ref FK omitted parent columns: pick column 0 by
        // convention (no catalog entry yet). Engine will widen this
        // when v7.6.7 lands; v7.6.1 only handles the explicit form.
        let _ = local_cols;
        return Some(0);
    }
    let parent = catalog.get(parent_name)?;
    parent.indices().iter().find_map(|idx| {
        if matches!(idx.kind, spg_storage::IndexKind::BTree(_))
            && idx.partial_predicate.is_none()
            && idx.included_columns.is_empty()
            && idx.expression.is_none()
        {
            Some(idx.column_position)
        } else {
            None
        }
    })
}

/// v7.9.8 / v7.9.10 — resolve the column positions that
/// identify a conflict for ON CONFLICT. Returns a Vec of
/// column positions (1 element for single-column form, N for
/// composite). When the user wrote bare `ON CONFLICT DO …`,
/// falls back to the table's first unconditional BTree index
/// (always single-column today).
/// Returns the conflict-key column positions plus whether the
/// matched constraint declares NULLS NOT DISTINCT (v7.29 — a NULL
/// in the key only rules out a conflict under the default
/// NULLS DISTINCT semantics).
pub(crate) fn resolve_on_conflict_columns(
    catalog: &Catalog,
    table_name: &str,
    target: &[String],
) -> Result<(Vec<usize>, bool), EngineError> {
    let table = catalog.get(table_name).ok_or_else(|| {
        EngineError::Storage(StorageError::TableNotFound {
            name: table_name.into(),
        })
    })?;
    if target.is_empty() {
        // v7.13.2 — mailrs round-6 S5 follow-up. Composite UNIQUE
        // constraints carry a multi-column tuple; the prior code
        // path picked only the leading column of the first BTree
        // index, which caused `ON CONFLICT DO NOTHING` to dedup
        // by leading column alone (3 rows with same group_id but
        // different permission collapsed to 1). PG semantics use
        // the full tuple. Prefer a UniquenessConstraint's full
        // column list when one exists; fall back to the leading
        // BTree column for legacy single-column UNIQUE.
        if let Some(uc) = table.schema().uniqueness_constraints.first() {
            return Ok((uc.columns.clone(), uc.nulls_not_distinct));
        }
        let pos = table
            .indices()
            .iter()
            .find_map(|idx| {
                if matches!(idx.kind, spg_storage::IndexKind::BTree(_))
                    && idx.partial_predicate.is_none()
                    && idx.included_columns.is_empty()
                    && idx.expression.is_none()
                {
                    Some(idx.column_position)
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "ON CONFLICT without target requires a UNIQUE BTree index on {table_name:?}"
                ))
            })?;
        return Ok((alloc::vec![pos], false));
    }
    let mut out = Vec::with_capacity(target.len());
    for name in target {
        let pos = table
            .schema()
            .columns
            .iter()
            .position(|c| c.name == *name)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "ON CONFLICT target column {name:?} not found on {table_name:?}"
                ))
            })?;
        out.push(pos);
    }
    // An explicit target matching a UNIQUE constraint inherits its
    // NULLS [NOT] DISTINCT declaration.
    let mut sorted = out.clone();
    sorted.sort_unstable();
    let nnd = table.schema().uniqueness_constraints.iter().any(|uc| {
        let mut u = uc.columns.clone();
        u.sort_unstable();
        u == sorted && uc.nulls_not_distinct
    });
    Ok((out, nnd))
}

/// v7.37.15 (Phase C.3) — does this BTree index locator point at a
/// gate-on tombstone? A `RowLocator::Hot(i)` indexes into
/// `table.headers()`; if that header is `is_deleted()` (`xmax !=
/// XMAX_ALIVE`) the row was DELETE-tombstoned under the in-place
/// write path (kept physically present, index entry left behind), so
/// index-based existence checks (FK parent lookup, ON CONFLICT
/// single-column) must treat it as ABSENT. Cold locators cannot be
/// tombstoned in place, so they always count as present. Under the
/// default gate (physical delete) no header is ever tombstoned, so
/// this returns `false` for every hot locator and the gate-off path
/// is byte-for-byte unchanged.
fn locator_is_tombstoned(table: &spg_storage::Table, loc: &spg_storage::RowLocator) -> bool {
    loc.as_hot()
        .is_some_and(|i| table.headers().get(i).is_some_and(|h| h.is_deleted()))
}

/// v7.9.8 — check whether the BTree index on `column_pos` of
/// `table_name` already has a row with this key.
fn on_conflict_key_exists(
    catalog: &Catalog,
    table_name: &str,
    column_pos: usize,
    key: &Value,
) -> bool {
    let Some(table) = catalog.get(table_name) else {
        return false;
    };
    let Some(idx_key) = spg_storage::IndexKey::from_value(key) else {
        return false;
    };
    table.indices().iter().any(|idx| {
        matches!(idx.kind, spg_storage::IndexKind::BTree(_))
            && idx.column_position == column_pos
            && idx.partial_predicate.is_none()
            // v7.37.15 (Phase C.3) — a tombstoned index hit is not a
            // live conflict: the key was freed by a gate-on DELETE, so
            // re-inserting it must NOT trip ON CONFLICT. Gate-off has no
            // tombstones → every locator counts → unchanged.
            && idx
                .lookup_eq(&idx_key)
                .iter()
                .any(|loc| !locator_is_tombstoned(table, loc))
    })
}

/// v7.9.9 / v7.9.10 — look up an existing row's position by
/// matching all `column_positions` against the incoming `key`
/// tuple. Single-column shape (one column) reduces to the
/// canonical PK lookup; composite shapes scan linearly until
/// every position matches.
pub(crate) fn lookup_row_position_by_keys(
    catalog: &Catalog,
    table_name: &str,
    column_positions: &[usize],
    key: &[&Value],
) -> Option<usize> {
    let table = catalog.get(table_name)?;
    // v7.37.15 (Phase C.3) — skip gate-on tombstones: a DELETE-
    // tombstoned row is not a live conflict target, so ON CONFLICT DO
    // UPDATE must not resolve onto it (it would resurrect a dead row).
    // `.position()` over `.enumerate()` yields the row index, so the
    // header check reuses the same index. `is_deleted()` is never true
    // under the default gate → gate-off path byte-for-byte unchanged.
    table.rows().iter().enumerate().position(|(row_idx, r)| {
        !table.headers().get(row_idx).is_some_and(|h| h.is_deleted())
            && column_positions
                .iter()
                .enumerate()
                .all(|(i, &pos)| r.values.get(pos) == Some(key[i]))
    })
}

/// v7.9.10 — does the table already contain a row whose
/// `column_positions` tuple equals `key`? Single-column shape
/// uses the existing BTree fast path; composite shapes fall
/// back to a row scan.
pub(crate) fn on_conflict_keys_exist(
    catalog: &Catalog,
    table_name: &str,
    column_positions: &[usize],
    key: &[&Value],
) -> bool {
    if column_positions.len() == 1 {
        return on_conflict_key_exists(catalog, table_name, column_positions[0], key[0]);
    }
    let Some(table) = catalog.get(table_name) else {
        return false;
    };
    let matches = |r: &Row<'static>| {
        column_positions
            .iter()
            .enumerate()
            .all(|(i, &pos)| r.values.get(pos) == Some(key[i]))
    };
    // v7.37.15 (Phase C.3) — a gate-on DELETE-tombstoned hot row is not
    // a live conflict, so skip it (else re-inserting the freed composite
    // key would falsely trip ON CONFLICT). Cold rows below cannot be
    // tombstoned in place. `is_deleted()` is never true under the
    // default gate → gate-off path byte-for-byte unchanged.
    let hot_hit = table.rows().iter().enumerate().any(|(row_idx, r)| {
        !table.headers().get(row_idx).is_some_and(|h| h.is_deleted()) && matches(r)
    });
    if hot_hit {
        return true;
    }
    // v7.36 (cold-tier coverage) — composite ON CONFLICT key
    // existence check must also see cold-tier rows; otherwise an
    // INSERT whose unique-key tuple lives only in the cold tier
    // silently bypasses ON CONFLICT and writes a duplicate.
    iter_cold_rows_of_parent(catalog, table)
        .iter()
        .any(&matches)
}

/// v7.9.9 — apply ON CONFLICT DO UPDATE SET assignments to an
/// existing row.
///
/// `incoming` is the rejected INSERT row (used to resolve
/// `EXCLUDED.col` references in the assignment exprs);
/// `target_pos` is the position of the existing row in the table.
/// Each assignment substitutes `EXCLUDED.col` with the matching
/// incoming value, evaluates the resulting expression against
/// the existing row, and writes the new value into the
/// corresponding column of the returned `Vec<Value<'static>>`. If
/// `where_` evaluates falsy, returns Ok(None) — PG behaviour:
/// the conflicting row is silently kept unchanged.
pub(crate) fn apply_on_conflict_assignments(
    catalog: &Catalog,
    table_name: &str,
    target_pos: usize,
    incoming: &[Value<'static>],
    assignments: &[(String, Expr)],
    where_: Option<&Expr>,
) -> Result<Option<Vec<Value<'static>>>, EngineError> {
    let table = catalog.get(table_name).ok_or_else(|| {
        EngineError::Storage(StorageError::TableNotFound {
            name: table_name.into(),
        })
    })?;
    let schema_cols = table.schema().columns.clone();
    let existing = table
        .rows()
        .get(target_pos)
        .ok_or_else(|| {
            EngineError::Unsupported(alloc::format!(
                "ON CONFLICT DO UPDATE: row position {target_pos} out of bounds on {table_name:?}"
            ))
        })?
        .clone();
    let ctx = eval::EvalContext::new(&schema_cols, Some(table_name));
    // Optional WHERE filter on the conflict row.
    if let Some(w) = where_ {
        let pred = w.clone();
        let pred = substitute_excluded_refs(pred, &schema_cols, incoming);
        let v = eval::eval_expr(&pred, &existing, &ctx)?;
        if !matches!(v, Value::Bool(true)) {
            return Ok(None);
        }
    }
    // REPLACE INTO lowering — an empty assignment list means
    // "replace the whole row with the incoming one" (MySQL
    // delete+insert semantics; the PG ON CONFLICT grammar never
    // produces an empty list).
    if assignments.is_empty() {
        return Ok(Some(incoming.to_vec()));
    }
    let mut new_values = existing.values.clone();
    for (col_name, expr) in assignments {
        let target_idx = schema_cols
            .iter()
            .position(|c| c.name == *col_name)
            .ok_or_else(|| {
                EngineError::Eval(EvalError::ColumnNotFound {
                    name: col_name.clone(),
                })
            })?;
        let sub = substitute_excluded_refs(expr.clone(), &schema_cols, incoming);
        let v = eval::eval_expr(&sub, &existing, &ctx)?;
        let coerced = coerce_value(v, schema_cols[target_idx].ty, col_name, target_idx)?;
        check_unsigned_range(&coerced, &schema_cols[target_idx], target_idx)?;
        new_values[target_idx] = coerced;
    }
    Ok(Some(new_values))
}

/// v7.9.9 — walk an `Expr` tree replacing any `Column { qualifier:
/// "EXCLUDED", name }` reference with a `Literal` of the matching
/// value from the incoming-row vec. Resolution against the
/// child-table column list (by name).
fn substitute_excluded_refs(
    expr: Expr,
    schema_cols: &[ColumnSchema],
    incoming: &[Value<'static>],
) -> Expr {
    use spg_sql::ast::ColumnName;
    match expr {
        Expr::Column(ColumnName { qualifier, name })
            if qualifier
                .as_deref()
                .is_some_and(|q| q.eq_ignore_ascii_case("excluded")) =>
        {
            let pos = schema_cols.iter().position(|c| c.name == name);
            match pos {
                Some(p) => {
                    let v = incoming.get(p).cloned().unwrap_or(Value::Null);
                    value_to_literal_expr(v)
                        .unwrap_or_else(|_| Expr::Literal(spg_sql::ast::Literal::Null))
                }
                None => Expr::Column(ColumnName { qualifier, name }),
            }
        }
        Expr::Binary { op, lhs, rhs } => Expr::Binary {
            op,
            lhs: Box::new(substitute_excluded_refs(*lhs, schema_cols, incoming)),
            rhs: Box::new(substitute_excluded_refs(*rhs, schema_cols, incoming)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op,
            expr: Box::new(substitute_excluded_refs(*expr, schema_cols, incoming)),
        },
        Expr::FunctionCall { name, args } => Expr::FunctionCall {
            name,
            args: args
                .into_iter()
                .map(|a| substitute_excluded_refs(a, schema_cols, incoming))
                .collect(),
        },
        // v7.33 (mailrs 7.32.1) — EXCLUDED refs nested inside these
        // value-expression shapes were silently passed through unsubstituted
        // by the old `other => other`, so `display_name = CASE WHEN
        // EXCLUDED.x != '' THEN EXCLUDED.x ELSE … END` reached row eval as a
        // live `excluded.` qualifier and errored. Recurse into every
        // sub-expression an upsert SET RHS can carry.
        Expr::Cast { expr, target } => Expr::Cast {
            expr: Box::new(substitute_excluded_refs(*expr, schema_cols, incoming)),
            target,
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(substitute_excluded_refs(*expr, schema_cols, incoming)),
            negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => Expr::Like {
            expr: Box::new(substitute_excluded_refs(*expr, schema_cols, incoming)),
            pattern: Box::new(substitute_excluded_refs(*pattern, schema_cols, incoming)),
            negated,
            case_insensitive,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(substitute_excluded_refs(*expr, schema_cols, incoming)),
            list: list
                .into_iter()
                .map(|e| substitute_excluded_refs(e, schema_cols, incoming))
                .collect(),
            negated,
        },
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => Expr::Case {
            operand: operand.map(|o| Box::new(substitute_excluded_refs(*o, schema_cols, incoming))),
            branches: branches
                .into_iter()
                .map(|(w, t)| {
                    (
                        substitute_excluded_refs(w, schema_cols, incoming),
                        substitute_excluded_refs(t, schema_cols, incoming),
                    )
                })
                .collect(),
            else_branch: else_branch
                .map(|e| Box::new(substitute_excluded_refs(*e, schema_cols, incoming))),
        },
        // Leaves (Literal / Placeholder / non-excluded Column) and
        // subquery-bearing nodes (a separate scope where `excluded` does not
        // apply) pass through unchanged.
        other => other,
    }
}

/// v7.6.2 / v7.6.7 — INSERT-side FK enforcement. For every row
/// about to be inserted into `child_table`, every FK declared on
/// that table is checked: the row's FK columns must either be
/// NULL (SQL spec skip) or match an existing parent row via the
/// parent's BTree PK / UNIQUE index.
///
/// Returns `EngineError::Unsupported` with a `FOREIGN KEY violation`
/// payload on first failure.
///
/// **Self-referencing FKs (v7.6.7 widening):** when `fk.parent_table
/// == child_table`, the parent rows visible to this check are
///  (a) rows already committed to the table, plus
///  (b) earlier rows from the *same* `rows` batch.
/// This makes `INSERT INTO tree VALUES (1, NULL), (2, 1), (3, 2)`
/// work in a single statement — common pattern for bulk-loading
/// hierarchies.
/// v7.9.19 — enforce table-level UNIQUE / PRIMARY KEY tuple
/// constraints at INSERT time. For each constraint declared on
/// the target table, check that no existing row + no earlier row
/// in the same batch has the same full-column tuple. NULL in
/// any column lifts the row out of the check (SQL spec: NULL
/// ≠ NULL for uniqueness). mailrs G1 + G6.

/// v7.39 (round 166, write-path attack A1) — column types whose non-NULL
/// values ALWAYS produce an `IndexKey` (`IndexKey::from_value` is total
/// for them), so every live row is guaranteed to be present in a btree
/// over that column. Types outside this list (Float / Numeric / arrays /
/// …) may skip the index and MUST NOT be probed for uniqueness.
fn indexkeyable_type(ty: &spg_storage::DataType) -> bool {
    use spg_storage::DataType as D;
    matches!(
        ty,
        D::SmallInt
            | D::Int
            | D::BigInt
            | D::Text
            | D::Varchar(_)
            | D::Char(_)
            | D::Bool
            | D::Uuid
            | D::Date
            | D::Timestamp
    )
}

/// v7.39 (round 166) — find a btree over `leading_pos` usable as a
/// uniqueness PROBE index (candidate filter only — the caller re-checks
/// candidates with the collated fold, so any plain btree on the leading
/// column works, unique or not). Expression / partial indexes key on
/// something other than the raw column and are skipped.
fn probe_btree<'t>(
    table: &'t spg_storage::Table,
    leading_pos: usize,
) -> Option<&'t spg_storage::Index> {
    table.indices().iter().find(|i| {
        matches!(i.kind, spg_storage::IndexKind::BTree(_))
            && i.column_position == leading_pos
            && i.expression.is_none()
            && i.partial_predicate.is_none()
    })
}

/// v7.39 (round 166) — can `uc` be enforced by probing a btree instead
/// of folding the whole table into a HashSet (the r164/r165 write-path
/// loss: O(table) per STATEMENT made every single-row write pay ~5-6ms
/// on a 50k-row table)? Requirements, all mirroring the fold semantics:
///  * a plain btree over the leading column exists (candidate source);
///  * `NULLS NOT DISTINCT` is off (NULL keys never enter a btree);
///  * no key column is case-insensitive collated (the btree keys raw
///    values, so a collation-folded duplicate under a DIFFERENT raw
///    key would be missed);
///  * the leading column's type always produces an IndexKey (otherwise
///    rows could be absent from the btree entirely).
fn uc_probe_index<'t>(
    table: &'t spg_storage::Table,
    columns: &[usize],
    nulls_not_distinct: bool,
) -> Option<&'t spg_storage::Index> {
    if nulls_not_distinct || columns.is_empty() {
        return None;
    }
    let schema = table.schema();
    let collation_ok = columns.iter().all(|&i| {
        schema
            .columns
            .get(i)
            .is_some_and(|c| !matches!(c.collation, spg_storage::Collation::CaseInsensitive))
    });
    if !collation_ok {
        return None;
    }
    if !schema
        .columns
        .get(columns[0])
        .is_some_and(|c| indexkeyable_type(&c.ty))
    {
        return None;
    }
    probe_btree(table, columns[0])
}

/// v7.39 (round 166) — probe `idx` for a live row whose collated key
/// equals `key` (the fold of the row being written). Returns the row
/// position of the first conflicting live row. `fold` recomputes the
/// collated key of a candidate row so collation / bpchar semantics stay
/// byte-identical with the HashSet path; tombstoned rows are skipped the
/// same way; Cold locators are skipped because the fold path only ever
/// scanned hot rows.
fn probe_key_conflict(
    table: &spg_storage::Table,
    idx: &spg_storage::Index,
    leading_val: &Value<'static>,
    key: &[Value<'static>],
    fold: &dyn Fn(&[Value<'static>]) -> Vec<Value<'static>>,
) -> Option<usize> {
    let ik = spg_storage::IndexKey::from_value(leading_val)?;
    for loc in idx.lookup_eq(&ik) {
        let spg_storage::RowLocator::Hot(ri) = loc else {
            continue;
        };
        if table.headers().get(*ri).is_some_and(|h| h.is_deleted()) {
            continue;
        }
        let Some(prow) = table.rows().get(*ri) else {
            continue;
        };
        if fold(&prow.values) == key {
            return Some(*ri);
        }
    }
    None
}

pub(crate) fn enforce_uniqueness_inserts(
    catalog: &Catalog,
    child_table: &str,
    constraints: &[spg_storage::UniquenessConstraint],
    rows: &[Vec<Value<'static>>],
) -> Result<(), EngineError> {
    if constraints.is_empty() {
        return Ok(());
    }
    let table = catalog.get(child_table).ok_or_else(|| {
        EngineError::Storage(StorageError::TableNotFound {
            name: child_table.into(),
        })
    })?;
    let schema = table.schema();
    // v7.29 (mailrs round-23b) — set-based: ONE O(table) pass folds
    // existing keys into a hash set, then each batch row is a probe
    // + insert. The previous shape scanned the WHOLE table per
    // inserted row (and earlier batch rows per row), which made
    // bulk import O(n²) — a 104 MB dump extrapolated to ~1 hour
    // (PG: 2 min). Collation folding (Phase 3.P0-45) and
    // NULLS [NOT] DISTINCT semantics are unchanged: keys fold via
    // collated_key_cell before encoding, NULL-bearing keys skip the
    // set unless nulls_not_distinct.
    for uc in constraints {
        let fold_key = |values: &[Value<'static>]| -> Vec<Value<'static>> {
            uc.columns
                .iter()
                .map(|&i| {
                    let v = values.get(i).cloned().unwrap_or(Value::Null);
                    collated_key_cell(&v, i, schema)
                })
                .collect()
        };
        // v7.39 (round 166, attack A1) — btree probe instead of the
        // per-statement O(table) fold when the constraint qualifies.
        // The implicit PK/UNIQUE leading-column btree (create-table
        // installs it) is maintained incrementally on every write, so
        // a probe is O(log n) per row — this was the 6.3ms/row (94%)
        // component of the r164 write losses.
        if let Some(idx) = uc_probe_index(table, &uc.columns, uc.nulls_not_distinct) {
            let mut batch_seen: hashbrown::HashSet<String> =
                hashbrown::HashSet::with_capacity(rows.len());
            let mut probe_ok = true;
            for row_values in rows.iter() {
                let key = fold_key(row_values);
                if key.iter().any(|v| matches!(v, Value::Null)) && !uc.nulls_not_distinct {
                    continue;
                }
                let leading = row_values
                    .get(uc.columns[0])
                    .cloned()
                    .unwrap_or(Value::Null);
                if spg_storage::IndexKey::from_value(&leading).is_none() {
                    // A value the btree can't key (shouldn't happen for
                    // the whitelisted types) — fall back to the fold.
                    probe_ok = false;
                    break;
                }
                let dup_in_batch = !batch_seen.insert(aggregate::encode_key(&key));
                if dup_in_batch
                    || probe_key_conflict(table, idx, &leading, &key, &fold_key).is_some()
                {
                    let conname = crate::system_catalog::pg_unique_conname(table, uc, child_table);
                    let detail = unique_key_detail(
                        &uc.columns
                            .iter()
                            .map(|&i| table.schema().columns[i].name.clone())
                            .collect::<Vec<_>>(),
                        &key,
                    );
                    return Err(EngineError::Unsupported(alloc::format!(
                        "duplicate key value violates unique constraint \"{conname}\" \
                         on table \"{child_table}\"{detail}"
                    )));
                }
            }
            if probe_ok {
                continue;
            }
        }
        let mut seen: hashbrown::HashSet<String> =
            hashbrown::HashSet::with_capacity(table.rows().len() + rows.len());
        for (row_idx, prow) in table.rows().iter().enumerate() {
            // v7.37.15 (Phase C.3) — under the gate-on in-place write
            // path a DELETE tombstones the row (xmax stamped, row kept
            // physically present) instead of removing it. A tombstoned
            // key is freed, so it must NOT count toward the uniqueness
            // set — otherwise re-inserting that key raises a false
            // violation. `is_deleted()` is `xmax != XMAX_ALIVE`; under
            // the default gate (physical delete) no header is ever
            // tombstoned, so this skip is never taken and the gate-off
            // path is byte-for-byte unchanged.
            if table.headers().get(row_idx).is_some_and(|h| h.is_deleted()) {
                continue;
            }
            let key = fold_key(&prow.values);
            if key.iter().any(|v| matches!(v, Value::Null)) && !uc.nulls_not_distinct {
                continue;
            }
            seen.insert(aggregate::encode_key(&key));
        }
        for (batch_idx, row_values) in rows.iter().enumerate() {
            let key = fold_key(row_values);
            if key.iter().any(|v| matches!(v, Value::Null)) && !uc.nulls_not_distinct {
                continue;
            }
            if !seen.insert(aggregate::encode_key(&key)) {
                // v7.39 (SQLSTATE fidelity) — PG's exact 23505 phrasing;
                // ORMs regex the constraint name out of this message and
                // the wire layer lifts it into the PG_DIAG fields.
                let conname = crate::system_catalog::pg_unique_conname(table, uc, child_table);
                let detail = unique_key_detail(
                    &uc.columns
                        .iter()
                        .map(|&i| table.schema().columns[i].name.clone())
                        .collect::<Vec<_>>(),
                    &key,
                );
                return Err(EngineError::Unsupported(alloc::format!(
                    "duplicate key value violates unique constraint \"{conname}\" \
                     on table \"{child_table}\"{detail}"
                )));
            }
        }
    }
    Ok(())
}

/// v7.39 (round 210) — map an EXCLUDE element's stored operator spelling to
/// its `BinOp`. Only the operators the parser accepts land here.
fn exclude_op_binop(op: &str) -> Option<spg_sql::ast::BinOp> {
    use spg_sql::ast::BinOp;
    Some(match op {
        "&&" => BinOp::InetOverlap,
        "=" => BinOp::Eq,
        "@>" => BinOp::JsonContains,
        "<@" => BinOp::JsonContainedBy,
        "&<" => BinOp::OverLeft,
        "&>" => BinOp::OverRight,
        _ => return None,
    })
}

/// v7.39 (round 210/215) — do two DISTINCT rows conflict under `ex`? True iff
/// EVERY element's operator holds (`new op old`). A NULL in any element column
/// exempts the row (returns false). Shared by the O(n) scan and the O(log n)
/// index probe so both decide identically.
fn excl_rows_conflict(
    ex: &spg_storage::ExclusionConstraint,
    newr: &[Value<'static>],
    oldr: &[Value<'static>],
) -> Result<bool, EngineError> {
    for (pos, op) in &ex.elements {
        let a = newr.get(*pos).cloned().unwrap_or(Value::Null);
        let b = oldr.get(*pos).cloned().unwrap_or(Value::Null);
        if matches!(a, Value::Null) || matches!(b, Value::Null) {
            return Ok(false);
        }
        let binop = exclude_op_binop(op).ok_or_else(|| {
            EngineError::Unsupported(alloc::format!("unsupported EXCLUDE operator {op:?}"))
        })?;
        // `&&` / `@>` / range / geo operators need owned semantics (the by-ref
        // path only answers comparisons); `a`/`b` are already owned clones.
        match eval::apply_binary(binop, a, b)? {
            Value::Bool(true) => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// v7.39 (round 215) — outcome of probing the range-exclusion index for one
/// candidate against the existing committed rows.
enum ExclProbe {
    /// A live existing row conflicts; carries its values for the DETAIL.
    Conflict(Vec<Value<'static>>),
    /// No existing row overlaps — the candidate is definitively clear (skip
    /// the O(n) scan).
    NoOverlap,
    /// The index couldn't decide (unkeyable candidate, or a probe key whose
    /// only locators are tombstoned under gate-on MVCC) — the caller runs the
    /// exact O(n) scan, which is always correct.
    Inconclusive,
}

/// One map-key probe result.
enum KeyProbe {
    Conflict(Vec<Value<'static>>),
    /// The key has ≥1 live locator, none of which conflict.
    LiveClear,
    /// The key exists but every locator is tombstoned.
    AllDead,
    /// No such key.
    Absent,
}

/// v7.39 (round 215) — O(log n) overlap probe for one candidate against the
/// range-exclusion index on `index_col`. Under a valid `EXCLUDE (col WITH &&)`
/// the stored ranges are pairwise disjoint, so a candidate can overlap only
/// its predecessor (the range whose lower sits just below) or the FIRST
/// successor (the smallest lower ≥ the candidate's): if the first LIVE
/// successor doesn't overlap, its lower is ≥ the candidate's upper and no
/// later one can either. Two `predecessor`/`range` probes, each O(log n). A
/// probe key whose only locators are tombstoned (gate-on) is inconclusive —
/// the real live neighbour may be further out, so fall back to the O(n) scan.
fn excl_probe_existing(
    table: &spg_storage::Table,
    ex: &spg_storage::ExclusionConstraint,
    index_col: usize,
    newr: &[Value<'static>],
) -> Result<ExclProbe, EngineError> {
    let Some(map) = table.excl_range_index(index_col) else {
        return Ok(ExclProbe::Inconclusive);
    };
    let cand = newr.get(index_col).cloned().unwrap_or(Value::Null);
    if matches!(cand, Value::Null) {
        return Ok(ExclProbe::NoOverlap); // NULL range never conflicts (exempt)
    }
    let Some(cand_key) = spg_storage::range_excl_index_key(&cand) else {
        return Ok(ExclProbe::Inconclusive); // unkeyable range → O(n)
    };
    let probe_entry = |entry: Option<(&(i128, u8), &Vec<spg_storage::RowLocator>)>|
     -> Result<KeyProbe, EngineError> {
        let Some((_, locs)) = entry else {
            return Ok(KeyProbe::Absent);
        };
        let mut saw_live = false;
        for loc in locs {
            if locator_is_tombstoned(table, loc) {
                continue;
            }
            let spg_storage::RowLocator::Hot(ri) = loc else {
                continue; // cold-tier rows aren't in the hot scan either (parity)
            };
            let Some(prow) = table.rows().get(*ri) else {
                continue;
            };
            saw_live = true;
            if excl_rows_conflict(ex, newr, &prow.values)? {
                return Ok(KeyProbe::Conflict(prow.values.clone()));
            }
        }
        Ok(if saw_live {
            KeyProbe::LiveClear
        } else {
            KeyProbe::AllDead
        })
    };
    let pred = probe_entry(map.predecessor(&cand_key))?;
    if let KeyProbe::Conflict(old) = pred {
        return Ok(ExclProbe::Conflict(old));
    }
    let succ = probe_entry(
        map.range(core::ops::Bound::Included(&cand_key), core::ops::Bound::Unbounded)
            .next(),
    )?;
    if let KeyProbe::Conflict(old) = succ {
        return Ok(ExclProbe::Conflict(old));
    }
    if matches!(pred, KeyProbe::AllDead) || matches!(succ, KeyProbe::AllDead) {
        Ok(ExclProbe::Inconclusive)
    } else {
        Ok(ExclProbe::NoOverlap)
    }
}

/// v7.39 (round 210) — enforce `EXCLUDE` constraints for a batch of incoming
/// rows. An exclusion constraint forbids two DISTINCT rows r,s from
/// satisfying `(r.c1 op1 s.c1) AND (r.c2 op2 s.c2) AND …` for every element.
/// A NULL in any element column exempts the row (PG / UNIQUE NULL semantics).
///
/// Enforcement is a full live-row scan re-evaluating each element's operator
/// (an equality index can't answer overlap; a real GiST index that does is a
/// later perf phase), plus an intra-batch pairwise check so two overlapping
/// rows inserted in one statement collide too. PG's exact 23P01 message +
/// the auto-/user-named constraint.
pub(crate) fn enforce_exclusion_inserts(
    catalog: &Catalog,
    child_table: &str,
    constraints: &[spg_storage::ExclusionConstraint],
    rows: &[Vec<Value<'static>>],
) -> Result<(), EngineError> {
    if constraints.is_empty() {
        return Ok(());
    }
    let table = catalog.get(child_table).ok_or_else(|| {
        EngineError::Storage(StorageError::TableNotFound {
            name: child_table.into(),
        })
    })?;
    let conflicts = excl_rows_conflict;
    for ex in constraints {
        // v7.39 (round 215) — the `&&` element with a range-overlap index, if
        // one was built (single-`&&` / multi-col `=`+`&&` on an integer-keyable
        // range column). Lets each candidate probe O(log n) instead of scanning
        // every existing row (measured O(N²), r213).
        let idx_col = ex
            .elements
            .iter()
            .find(|(pos, op)| op == "&&" && table.excl_range_index(*pos).is_some())
            .map(|(pos, _)| *pos);
        // Each candidate vs the existing committed rows: index probe when
        // possible, exact O(n) scan otherwise.
        for newr in rows.iter() {
            let mut proved_clear = false;
            if let Some(col) = idx_col {
                match excl_probe_existing(table, ex, col, newr)? {
                    ExclProbe::Conflict(old) => {
                        return Err(exclusion_violation(table, ex, child_table, newr, &old));
                    }
                    ExclProbe::NoOverlap => proved_clear = true,
                    ExclProbe::Inconclusive => {} // fall through to the O(n) scan
                }
            }
            if proved_clear {
                continue;
            }
            // O(n) fallback (no index, unkeyable candidate, or an all-dead
            // probe key under gate-on tombstones — always correct).
            for (row_idx, prow) in table.rows().iter().enumerate() {
                if table.headers().get(row_idx).is_some_and(|h| h.is_deleted()) {
                    continue;
                }
                if conflicts(ex, newr, &prow.values)? {
                    return Err(exclusion_violation(table, ex, child_table, newr, &prow.values));
                }
            }
        }
        // Intra-batch: two incoming rows that overlap each other.
        // v7.39 (round 214) — the naive pairwise scan is O(N²); a single
        // multi-row INSERT / COPY of a booking table hits it hard (measured
        // O(N²), r213). For the common single-`&&` form the sorted-adjacency
        // test proves disjointness in O(N log N): sort the candidates by
        // range lower bound and check only adjacent pairs (a non-adjacent
        // overlap always implies an adjacent one). When that PROVES no
        // overlap the O(N²) loop is skipped entirely. When it can't (an
        // overlap exists, or a candidate is a kind the fast key doesn't
        // cover), fall through to the exact loop so the error stays
        // byte-identical to PG. This touches no cross-statement state, so it
        // is MVCC-trivially correct — the per-write existing-row scan above
        // (single-row INSERT streams) still needs the persistent index.
        if !(ex.elements.len() == 1
            && ex.elements[0].1 == "&&"
            && intra_batch_proven_disjoint(ex.elements[0].0, rows)?)
        {
            for i in 0..rows.len() {
                for j in (i + 1)..rows.len() {
                    if conflicts(ex, &rows[j], &rows[i])? {
                        return Err(exclusion_violation(
                            table, ex, child_table, &rows[j], &rows[i],
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// v7.39 (round 214) — extract a range's lower-bound sort key: the bound as
/// an `i128` (unbounded = i128::MIN, sorting first) plus an inclusivity rank
/// (inclusive lower sorts before exclusive at the same value, `[3` before
/// `(3`). Returns `None` for range kinds whose bound isn't an integer scalar
/// (numrange's numeric/bignum) — the caller then forces the exact O(N²) loop
/// rather than risk an unsound order. Int4/Int8/Date/Ts/TsTz all reduce here.
fn range_lower_sort_key(v: &Value<'_>) -> Option<(i128, u8)> {
    let Value::Range {
        lower,
        lower_inc,
        empty,
        ..
    } = v
    else {
        return None;
    };
    if *empty {
        return None;
    }
    let key = match lower {
        None => i128::MIN,
        Some(b) => match b.as_ref() {
            Value::SmallInt(n) => i128::from(*n),
            Value::Int(n) => i128::from(*n),
            Value::BigInt(n) => i128::from(*n),
            // daterange (days since epoch) + ts/tstzrange (micros since epoch)
            // — both totally ordered as their raw integer.
            Value::Date(n) => i128::from(*n),
            Value::Timestamp(n) => i128::from(*n),
            _ => return None,
        },
    };
    Some((key, u8::from(!*lower_inc)))
}

/// v7.39 (round 214) — PROVE (soundly) that no two candidate rows' ranges at
/// `pos` overlap, in O(N log N). Returns `true` only when disjointness is
/// certain; returns `false` if an overlap exists OR any candidate can't be
/// keyed (non-range, empty handled as exempt, numrange, short row) — in which
/// case the caller runs the exact pairwise loop. NULL and empty ranges never
/// conflict, so they leave the candidate set. The authoritative overlap
/// decision on each adjacent pair delegates to `&&` (`apply_binary`), so the
/// only thing the fast path relies on is the sort order being correct — which
/// the integer key guarantees for the kinds it accepts.
fn intra_batch_proven_disjoint(
    pos: usize,
    rows: &[Vec<Value<'static>>],
) -> Result<bool, EngineError> {
    let mut keyed: Vec<((i128, u8), usize)> = Vec::with_capacity(rows.len());
    for (i, r) in rows.iter().enumerate() {
        match r.get(pos) {
            None => return Ok(false), // short row — let the exact loop handle it
            Some(Value::Null) => continue, // NULL exempts the row
            Some(v @ Value::Range { empty, .. }) => {
                if *empty {
                    continue; // empty range never overlaps
                }
                match range_lower_sort_key(v) {
                    Some(k) => keyed.push((k, i)),
                    None => return Ok(false), // unkeyable range kind → exact loop
                }
            }
            Some(_) => return Ok(false), // not a range → exact loop
        }
    }
    if keyed.len() < 2 {
        return Ok(true); // 0 or 1 candidate ranges can't overlap each other
    }
    keyed.sort_by_key(|k| k.0);
    for w in keyed.windows(2) {
        let a = rows[w[0].1][pos].clone();
        let b = rows[w[1].1][pos].clone();
        // overlap → let the exact loop produce PG's byte-identical error
        if let Value::Bool(true) = eval::apply_binary(spg_sql::ast::BinOp::InetOverlap, a, b)? {
            return Ok(false);
        }
    }
    Ok(true) // adjacency proved the whole set disjoint
}

/// v7.39 (round 210) — enforce `EXCLUDE` constraints for an UPDATE. Each
/// planned `(row_pos, new_values)` is checked against every live row EXCEPT
/// the rows being updated in this same statement (their pre-images leave the
/// set — otherwise a no-op UPDATE would collide with itself), plus pairwise
/// among the planned new rows.
pub(crate) fn enforce_exclusion_updates(
    catalog: &Catalog,
    table_name: &str,
    constraints: &[spg_storage::ExclusionConstraint],
    planned: &[(usize, Vec<Value<'static>>)],
) -> Result<(), EngineError> {
    if constraints.is_empty() || planned.is_empty() {
        return Ok(());
    }
    let table = catalog.get(table_name).ok_or_else(|| {
        EngineError::Storage(StorageError::TableNotFound {
            name: table_name.into(),
        })
    })?;
    let updated: hashbrown::HashSet<usize> = planned.iter().map(|(p, _)| *p).collect();
    let conflicts = |ex: &spg_storage::ExclusionConstraint,
                     newr: &[Value<'static>],
                     oldr: &[Value<'static>]|
     -> Result<bool, EngineError> {
        for (pos, op) in &ex.elements {
            let a = newr.get(*pos).cloned().unwrap_or(Value::Null);
            let b = oldr.get(*pos).cloned().unwrap_or(Value::Null);
            if matches!(a, Value::Null) || matches!(b, Value::Null) {
                return Ok(false);
            }
            let binop = exclude_op_binop(op).ok_or_else(|| {
                EngineError::Unsupported(alloc::format!("unsupported EXCLUDE operator {op:?}"))
            })?;
            // `&&` / `@>` / range / geo operators need owned semantics
            // (the by-ref path only answers comparisons); `a`/`b` are
            // already owned clones here.
            match eval::apply_binary(binop, a, b)? {
                Value::Bool(true) => {}
                _ => return Ok(false),
            }
        }
        Ok(true)
    };
    for ex in constraints {
        for (row_idx, prow) in table.rows().iter().enumerate() {
            if table.headers().get(row_idx).is_some_and(|h| h.is_deleted()) {
                continue;
            }
            if updated.contains(&row_idx) {
                continue;
            }
            for (_pos, newr) in planned {
                if conflicts(ex, newr, &prow.values)? {
                    return Err(exclusion_violation(table, ex, table_name, newr, &prow.values));
                }
            }
        }
        for i in 0..planned.len() {
            for j in (i + 1)..planned.len() {
                if conflicts(ex, &planned[j].1, &planned[i].1)? {
                    return Err(exclusion_violation(
                        table,
                        ex,
                        table_name,
                        &planned[j].1,
                        &planned[i].1,
                    ));
                }
            }
        }
    }
    Ok(())
}

/// v7.39 (round 210) — PG's 23P01 exclusion-violation error + DETAIL. PG:
/// `conflicting key value violates exclusion constraint "<name>"` with
/// `DETAIL: Key (during)=([3,7)) conflicts with existing key (during)=([1,5)).`
/// The ` on table "…"` suffix mirrors the uniqueness path; the pgwire layer
/// strips it (PG's message has none) and lifts the name into PG_DIAG `n`.
fn exclusion_violation(
    table: &spg_storage::Table,
    ex: &spg_storage::ExclusionConstraint,
    child_table: &str,
    newr: &[Value<'static>],
    oldr: &[Value<'static>],
) -> EngineError {
    let render = |vals: &[Value<'static>]| -> (String, String) {
        let cols = ex
            .elements
            .iter()
            .map(|(p, _)| table.schema().columns[*p].name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let rendered = ex
            .elements
            .iter()
            .map(|(p, _)| {
                let v = vals.get(*p).cloned().unwrap_or(Value::Null);
                match v {
                    Value::Text(s) => s.to_string(),
                    other => crate::eval::value_to_text(&other),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        (cols, rendered)
    };
    let (cols, new_vals) = render(newr);
    let (_, old_vals) = render(oldr);
    EngineError::Unsupported(alloc::format!(
        "conflicting key value violates exclusion constraint \"{}\" \
         on table \"{child_table}\" DETAIL: Key ({cols})=({new_vals}) \
         conflicts with existing key ({cols})=({old_vals}).",
        ex.name
    ))
}

/// v7.39 (SQLSTATE fidelity) — PG's 23505 DETAIL body:
/// ` DETAIL: Key (a, b)=(1, x) already exists.` Appended to the main
/// message (the engine error is a single string; psql-style separate
/// DETAIL packets are a wire-layer follow-up).
fn unique_key_detail(cols: &[String], key: &[Value<'_>]) -> String {
    let vals = key
        .iter()
        .map(|v| match v {
            Value::Text(s) => s.to_string(),
            other => crate::eval::value_to_text(other),
        })
        .collect::<Vec<_>>()
        .join(", ");
    alloc::format!(
        " DETAIL: Key ({})=({vals}) already exists.",
        cols.join(", ")
    )
}

/// v7.39 (SQLSTATE fidelity) — PG's 23503 phrasing helper: the FK
/// constraint name by PG convention plus the local-column key DETAIL.
fn fk_violation_message(
    child: &spg_storage::Table,
    child_table: &str,
    fk: &spg_storage::ForeignKeyConstraint,
    key_vals: &[&Value<'_>],
) -> String {
    let conname = crate::system_catalog::pg_fk_conname(child, fk, child_table);
    let cols = fk
        .local_columns
        .iter()
        .map(|&p| {
            child
                .schema()
                .columns
                .get(p)
                .map_or_else(|| alloc::format!("col{p}"), |c| c.name.clone())
        })
        .collect::<Vec<_>>()
        .join(", ");
    let vals = key_vals
        .iter()
        .map(|v| match v {
            Value::Text(s) => s.to_string(),
            other => crate::eval::value_to_text(other),
        })
        .collect::<Vec<_>>()
        .join(", ");
    alloc::format!(
        "insert or update on table \"{child_table}\" violates foreign key \
         constraint \"{conname}\" DETAIL: Key ({cols})=({vals}) is not present \
         in table \"{}\".",
        fk.parent_table
    )
}

/// v7.39 (SQLSTATE fidelity) — PG's parent-side 23503 phrasing:
/// `update or delete on table "p" violates foreign key constraint
/// "c_col_fkey" on table "c"` with the still-referenced key DETAIL.
fn fk_restrict_message(
    catalog: &Catalog,
    parent_name: &str,
    child: &spg_storage::Table,
    child_name: &str,
    fk: &spg_storage::ForeignKeyConstraint,
    parent_key: &[&Value<'_>],
) -> String {
    let conname = crate::system_catalog::pg_fk_conname(child, fk, child_name);
    let pcols = match catalog.get(parent_name) {
        Some(parent) => fk
            .parent_columns
            .iter()
            .map(|&p| {
                parent
                    .schema()
                    .columns
                    .get(p)
                    .map_or_else(|| alloc::format!("col{p}"), |c| c.name.clone())
            })
            .collect::<Vec<_>>()
            .join(", "),
        None => "?".into(),
    };
    let vals = parent_key
        .iter()
        .map(|v| match v {
            Value::Text(s) => s.to_string(),
            other => crate::eval::value_to_text(other),
        })
        .collect::<Vec<_>>()
        .join(", ");
    alloc::format!(
        "update or delete on table \"{parent_name}\" violates foreign key \
         constraint \"{conname}\" on table \"{child_name}\" \
         DETAIL: Key ({pcols})=({vals}) is still referenced from table \"{child_name}\"."
    )
}

/// v7.17.0 Phase 3.P0-45 — return a key cell folded by its column's
/// declared `Collation`. For `CaseInsensitive`, fold Text payloads to
/// ASCII lowercase (matches Phase 2.5's `*_ci` semantics: ASCII case-
/// fold only, non-ASCII bytes stay byte-wise). For `Binary` or non-Text
/// values, the cell passes through unchanged. The caller compares the
/// folded values with `==`.
fn collated_key_cell(
    v: &spg_storage::Value,
    column_position: usize,
    schema: &spg_storage::TableSchema,
) -> spg_storage::Value<'static> {
    match (v, schema.columns.get(column_position).map(|c| c.collation)) {
        (spg_storage::Value::Text(s), Some(spg_storage::Collation::CaseInsensitive)) => {
            spg_storage::Value::text(s.to_ascii_lowercase())
        }
        _ => v.clone().into_owned(),
    }
}

/// v7.9.29 — `true` iff `v` counts as a truthy SQL value for a
/// WHERE-style predicate. NULL → false (three-valued logic
/// collapses to "skip this row" for index inclusion). Numeric
/// non-zero, BIGINT non-zero, TINYINT non-zero, BOOLEAN true → true.
/// Everything else (strings, vectors, JSON, …) is not a valid
/// predicate result and surfaces as `false` so a malformed
/// predicate degrades to "row not in index" rather than panicking.
fn predicate_truthy(v: &spg_storage::Value) -> bool {
    use spg_storage::Value as V;
    match v {
        V::Bool(b) => *b,
        V::Int(n) => *n != 0,
        V::BigInt(n) => *n != 0,
        V::SmallInt(n) => *n != 0,
        _ => false,
    }
}

/// v7.9.29 — at CREATE UNIQUE INDEX time, scan the table's
/// committed rows for pre-existing duplicates. If any pair of rows
/// matches the predicate AND has the same index key, refuse to
/// create the index so the user fixes the data before retrying.
pub(crate) fn check_existing_unique_violation(
    idx: &spg_storage::Index,
    schema: &spg_storage::TableSchema,
    rows: &[spg_storage::Row<'static>],
) -> Result<(), EngineError> {
    let predicate_expr = match idx.partial_predicate.as_deref() {
        Some(s) => Some(spg_sql::parser::parse_expression(s).map_err(|e| {
            EngineError::Unsupported(alloc::format!(
                "stored partial predicate {s:?} failed to re-parse: {e:?}"
            ))
        })?),
        None => None,
    };
    let ctx = eval::EvalContext::new(&schema.columns, None);
    let key_positions = unique_key_positions(idx);
    let mut seen: alloc::vec::Vec<alloc::vec::Vec<spg_storage::Value<'static>>> =
        alloc::vec::Vec::new();
    for row in rows {
        if let Some(expr) = &predicate_expr {
            let v = eval::eval_expr(expr, row, &ctx).map_err(|e| {
                EngineError::Unsupported(alloc::format!(
                    "evaluating UNIQUE INDEX predicate against existing row: {e:?}"
                ))
            })?;
            if !predicate_truthy(&v) {
                continue;
            }
        }
        let key: alloc::vec::Vec<spg_storage::Value<'static>> = key_positions
            .iter()
            .map(|&p| {
                let v = row
                    .values
                    .get(p)
                    .cloned()
                    .unwrap_or(spg_storage::Value::Null);
                collated_key_cell(&v, p, schema)
            })
            .collect();
        // v7.39 (read01 round 52) — NULLS NOT DISTINCT keeps NULL keys in the
        // check, so CREATE UNIQUE INDEX … NULLS NOT DISTINCT over two all-NULL
        // rows is rejected (PG: "could not create unique index").
        if !idx.nulls_not_distinct && key.iter().any(|v| matches!(v, spg_storage::Value::Null)) {
            continue;
        }
        if seen.iter().any(|other| *other == key) {
            // v7.39 (read01 round 52) — PG wording (23505 at the wire).
            return Err(EngineError::Unsupported(alloc::format!(
                "could not create unique index {:?}",
                idx.name
            )));
        }
        seen.push(key);
    }
    Ok(())
}

/// v7.9.29 — full key tuple for a UNIQUE INDEX (leading +
/// extra positions). For single-column indexes this is just
/// `[column_position]`.
fn unique_key_positions(idx: &spg_storage::Index) -> alloc::vec::Vec<usize> {
    let mut out = alloc::vec::Vec::with_capacity(1 + idx.extra_column_positions.len());
    out.push(idx.column_position);
    out.extend_from_slice(&idx.extra_column_positions);
    out
}

/// v7.9.29 — at INSERT time, walk every `is_unique` index on the
/// target table. For each, eval the index's optional predicate
/// against (a) the candidate row and (b) every committed row plus
/// earlier batch rows; only rows where the predicate is truthy
/// participate. A duplicate key among predicate-matching rows is a
/// uniqueness violation. NULL keys lift the row out of the check
/// (matching PG's "UNIQUE allows multiple NULLs" semantics).
pub(crate) fn enforce_unique_index_inserts(
    catalog: &Catalog,
    table_name: &str,
    rows: &[alloc::vec::Vec<spg_storage::Value<'static>>],
) -> Result<(), EngineError> {
    let table = catalog.get(table_name).ok_or_else(|| {
        EngineError::Storage(StorageError::TableNotFound {
            name: table_name.into(),
        })
    })?;
    let schema = table.schema();
    let ctx = eval::EvalContext::new(&schema.columns, None);
    for idx in table.indices() {
        if !idx.is_unique {
            continue;
        }
        // Re-parse the predicate once per index per batch.
        let predicate_expr = match idx.partial_predicate.as_deref() {
            Some(s) => Some(spg_sql::parser::parse_expression(s).map_err(|e| {
                EngineError::Unsupported(alloc::format!(
                    "UNIQUE INDEX {:?} predicate {s:?} failed to re-parse: {e:?}",
                    idx.name
                ))
            })?),
            None => None,
        };
        // v7.38 (read01 U1) — an expression index (`CREATE UNIQUE INDEX ON
        // t (lower(email))`) carries its key as a parseable expression, not
        // a column position. Re-parse once per batch and evaluate per row so
        // the key reflects the expression; without this the uniqueness was
        // silently not enforced (duplicate `lower(email)` values slipped in).
        let expr_key = match idx.expression.as_deref() {
            Some(s) => Some(spg_sql::parser::parse_expression(s).map_err(|e| {
                EngineError::Unsupported(alloc::format!(
                    "UNIQUE INDEX {:?} expression {s:?} failed to re-parse: {e:?}",
                    idx.name
                ))
            })?),
            None => None,
        };
        let key_positions = unique_key_positions(idx);
        let key_of = |values: &[spg_storage::Value<'static>]| -> Result<alloc::vec::Vec<spg_storage::Value<'static>>, EngineError> {
            if let Some(expr) = &expr_key {
                let tmp_row = spg_storage::Row {
                    values: values.to_vec(),
                };
                let v = eval::eval_expr(expr, &tmp_row, &ctx).map_err(|e| {
                    EngineError::Unsupported(alloc::format!(
                        "UNIQUE INDEX {:?} expression eval: {e:?}",
                        idx.name
                    ))
                })?;
                return Ok(alloc::vec![v]);
            }
            Ok(key_positions
                .iter()
                .map(|&p| {
                    let v = values.get(p).cloned().unwrap_or(spg_storage::Value::Null);
                    collated_key_cell(&v, p, schema)
                })
                .collect())
        };
        let participates = |values: &[spg_storage::Value<'static>]| -> Result<bool, EngineError> {
            let Some(expr) = &predicate_expr else {
                return Ok(true);
            };
            let tmp_row = spg_storage::Row {
                values: values.to_vec(),
            };
            let v = eval::eval_expr(expr, &tmp_row, &ctx).map_err(|e| {
                EngineError::Unsupported(alloc::format!(
                    "UNIQUE INDEX {:?} predicate eval: {e:?}",
                    idx.name
                ))
            })?;
            Ok(predicate_truthy(&v))
        };
        // v7.39 (round 166, attack A2) — a plain (non-expression,
        // non-partial) unique index IS its own probe btree: check each
        // batch row via lookup_eq instead of folding the whole table.
        // Same qualification rules as the constraint path (A1).
        if idx.expression.is_none()
            && idx.partial_predicate.is_none()
            && !idx.nulls_not_distinct
            && matches!(idx.kind, spg_storage::IndexKind::BTree(_))
        {
            let positions = unique_key_positions(idx);
            let schema_ok = positions.iter().all(|&i| {
                schema.columns.get(i).is_some_and(|c| {
                    !matches!(c.collation, spg_storage::Collation::CaseInsensitive)
                })
            }) && schema
                .columns
                .get(idx.column_position)
                .is_some_and(|c| indexkeyable_type(&c.ty));
            if schema_ok {
                let fold = |values: &[spg_storage::Value<'static>]| -> Vec<spg_storage::Value<'static>> {
                    positions
                        .iter()
                        .map(|&p| {
                            let v = values.get(p).cloned().unwrap_or(spg_storage::Value::Null);
                            collated_key_cell(&v, p, schema)
                        })
                        .collect()
                };
                let mut batch_seen: hashbrown::HashSet<String> =
                    hashbrown::HashSet::with_capacity(rows.len());
                let mut probe_ok = true;
                for row_values in rows.iter() {
                    let key = fold(row_values);
                    if key.iter().any(|v| matches!(v, spg_storage::Value::Null)) {
                        continue;
                    }
                    let leading = row_values
                        .get(idx.column_position)
                        .cloned()
                        .unwrap_or(spg_storage::Value::Null);
                    if spg_storage::IndexKey::from_value(&leading).is_none() {
                        probe_ok = false;
                        break;
                    }
                    if !batch_seen.insert(aggregate::encode_key(&key))
                        || probe_key_conflict(table, idx, &leading, &key, &fold).is_some()
                    {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "duplicate key value violates unique constraint \"{}\" \
                             on table \"{table_name}\"",
                            idx.name
                        )));
                    }
                }
                if probe_ok {
                    continue;
                }
            }
        }
        // v7.29 (mailrs round-23b) — set-based: one O(table) pass
        // (predicate evaluated once per existing row instead of once
        // per row PAIR), then probe per batch row. The previous
        // nested scans made bulk import O(n²).
        let mut seen: hashbrown::HashSet<String> =
            hashbrown::HashSet::with_capacity(table.rows().len() + rows.len());
        for (row_idx, prow) in table.rows().iter().enumerate() {
            // v7.37.15 (Phase C.3) — skip gate-on tombstones so a
            // re-insert of a freed key succeeds. See the twin guard in
            // `enforce_uniqueness_inserts`; `is_deleted()` is never true
            // under the default gate (physical delete), so the gate-off
            // path is byte-for-byte unchanged.
            if table.headers().get(row_idx).is_some_and(|h| h.is_deleted()) {
                continue;
            }
            if !participates(&prow.values)? {
                continue;
            }
            let key = key_of(&prow.values)?;
            // v7.39 (read01 round 52) — NULLS NOT DISTINCT keeps NULL keys in
            // the uniqueness check (PG 15+); the default exempts them.
            if !idx.nulls_not_distinct && key.iter().any(|v| matches!(v, spg_storage::Value::Null))
            {
                continue;
            }
            seen.insert(aggregate::encode_key(&key));
        }
        for (batch_idx, row_values) in rows.iter().enumerate() {
            if !participates(row_values)? {
                continue;
            }
            let key = key_of(row_values)?;
            if !idx.nulls_not_distinct && key.iter().any(|v| matches!(v, spg_storage::Value::Null))
            {
                continue;
            }
            if !seen.insert(aggregate::encode_key(&key)) {
                // v7.39 (SQLSTATE fidelity) — a unique INDEX is a unique
                // constraint to clients; same PG 23505 phrasing.
                return Err(EngineError::Unsupported(alloc::format!(
                    "duplicate key value violates unique constraint \"{}\" \
                     on table \"{table_name}\"",
                    idx.name
                )));
            }
        }
    }
    Ok(())
}

/// v7.38 (read01 U1) — UPDATE-time uniqueness enforcement. INSERT has
/// `enforce_uniqueness_inserts` + `enforce_unique_index_inserts`, but the
/// UPDATE path checked FK / CHECK / NOT NULL and silently skipped every
/// UNIQUE constraint and unique index — so an UPDATE could move a row onto
/// a key another row already holds (`UPDATE t SET x=1 WHERE x=2` with a
/// second row at `x=1`, or `UPDATE t SET email='A' ...` colliding on
/// `lower(email)`). PG rejects these; SPG now does too.
///
/// `planned` is the update batch as `(row_position, new_values)`. The key
/// difference from the INSERT check is that the pre-image of every updated
/// row must be *excluded* from the "existing keys" set — otherwise a row
/// whose key is unchanged would collide with its own old key, and a valid
/// key swap would false-positive. So the existing-key scan skips the
/// updated positions, then the new values probe against the remainder and
/// against each other.
///
/// `changed_cols` is the set of column positions the UPDATE may have
/// altered (SET targets + ON UPDATE overrides + stored-generated columns).
/// A UNIQUE constraint or plain unique index whose key columns are all
/// untouched cannot gain a new duplicate, so it is skipped — this keeps a
/// hot `UPDATE … WHERE id=$1 SET non_key=…` off the O(table) scan.
/// Expression / partial indexes may depend on any column, so they are
/// always checked when present.
///
/// The check models PG's non-deferrable (immediate) semantics: it seeds a
/// key set from every current row, then replays each update as
/// remove-old-key + insert-new-key. Inserting a key that is still present
/// is a violation — so a straight duplicate, a two-row swap
/// (`SET x = CASE …`), and a shift (`SET x = x + 1` over adjacent keys)
/// are all rejected exactly as PG rejects them, while a row whose key is
/// unchanged, or reassigned to a genuinely free value, passes.

/// v7.39 (round 166, attack A3) — probe-based twin of the UPDATE
/// `replay` closure: instead of seeding a HashSet from the whole table,
/// membership(k) is modelled as `(table \ removed) ∪ added` with the
/// table part answered by a btree probe. Semantically identical to the
/// fold replay (same key function, same ordering); returns Ok(false)
/// when an unprobeable value forces the caller back onto the fold path.
#[allow(clippy::too_many_lines)]
fn probe_replay(
    table: &spg_storage::Table,
    idx: &spg_storage::Index,
    columns: &[usize],
    planned: &[(usize, Vec<Value<'static>>)],
    schema: &spg_storage::TableSchema,
    key_str: &KeyStrFn<'_>,
    on_conflict: &dyn Fn(usize) -> EngineError,
) -> Result<bool, EngineError> {
    let fold = |values: &[Value<'static>]| -> Vec<Value<'static>> {
        columns
            .iter()
            .map(|&i| {
                let v = values.get(i).cloned().unwrap_or(Value::Null);
                collated_key_cell(&v, i, schema)
            })
            .collect()
    };
    let mut added: hashbrown::HashSet<String> = hashbrown::HashSet::new();
    let mut removed: hashbrown::HashSet<String> = hashbrown::HashSet::new();
    for (pos, new_vals) in planned {
        let old_key = match table.rows().get(*pos) {
            Some(r) => key_str(&r.values)?,
            None => None,
        };
        let new_key = key_str(new_vals)?;
        if old_key == new_key {
            continue;
        }
        if let Some(ok) = old_key {
            if !added.remove(&ok) {
                removed.insert(ok);
            }
        }
        if let Some(nk) = new_key {
            if added.contains(&nk) {
                return Err(on_conflict(*pos));
            }
            if !removed.contains(&nk) {
                let key_vec = fold(new_vals);
                let leading = new_vals.get(columns[0]).cloned().unwrap_or(Value::Null);
                if spg_storage::IndexKey::from_value(&leading).is_none() {
                    return Ok(false);
                }
                if let Some(ri) = probe_key_conflict(table, idx, &leading, &key_vec, &fold)
                    && ri != *pos
                {
                    return Err(on_conflict(*pos));
                }
            }
            added.insert(nk);
        }
    }
    Ok(true)
}

pub(crate) fn enforce_unique_updates(
    catalog: &Catalog,
    table_name: &str,
    planned: &[(usize, Vec<Value<'static>>)],
    changed_cols: &hashbrown::HashSet<usize>,
) -> Result<(), EngineError> {
    if planned.is_empty() {
        return Ok(());
    }
    let table = catalog.get(table_name).ok_or_else(|| {
        EngineError::Storage(StorageError::TableNotFound {
            name: table_name.into(),
        })
    })?;
    let schema = table.schema();

    // Seed the key set from all current rows, then replay each update as
    // remove-old + insert-new; `key_str` returns None for a row that isn't
    // in the index (NULL key, or partial-predicate false) so it neither
    // seeds nor conflicts.
    let replay = |key_str: &KeyStrFn<'_>,
                  on_conflict: &dyn Fn(usize) -> EngineError|
     -> Result<(), EngineError> {
        let mut index: hashbrown::HashSet<String> =
            hashbrown::HashSet::with_capacity(table.rows().len());
        for (row_idx, prow) in table.rows().iter().enumerate() {
            if table.headers().get(row_idx).is_some_and(|h| h.is_deleted()) {
                continue;
            }
            if let Some(k) = key_str(&prow.values)? {
                index.insert(k);
            }
        }
        for (pos, new_vals) in planned {
            let old_key = match table.rows().get(*pos) {
                Some(r) => key_str(&r.values)?,
                None => None,
            };
            let new_key = key_str(new_vals)?;
            if old_key == new_key {
                continue; // key unchanged (incl. both absent) — no effect
            }
            if let Some(ok) = &old_key {
                index.remove(ok);
            }
            if let Some(nk) = new_key
                && !index.insert(nk)
            {
                return Err(on_conflict(*pos));
            }
        }
        Ok(())
    };

    // ── composite / column UNIQUE + PRIMARY KEY constraints ──
    for uc in &schema.uniqueness_constraints {
        if !uc.columns.iter().any(|c| changed_cols.contains(c)) {
            continue;
        }
        let key_str = |values: &[Value<'static>]| -> Result<Option<String>, EngineError> {
            let key: Vec<Value<'static>> = uc
                .columns
                .iter()
                .map(|&i| {
                    let v = values.get(i).cloned().unwrap_or(Value::Null);
                    collated_key_cell(&v, i, schema)
                })
                .collect();
            if key.iter().any(|v| matches!(v, Value::Null)) && !uc.nulls_not_distinct {
                return Ok(None);
            }
            Ok(Some(aggregate::encode_key(&key)))
        };
        let on_conflict = |_pos: usize| -> EngineError {
            // v7.39 (SQLSTATE fidelity) — PG's 23505 phrasing (see the
            // INSERT-path twin above).
            let conname = if uc.is_primary_key {
                alloc::format!("{table_name}_pkey")
            } else {
                let cols = uc
                    .columns
                    .iter()
                    .map(|&i| schema.columns[i].name.clone())
                    .collect::<Vec<_>>()
                    .join("_");
                alloc::format!("{table_name}_{cols}_key")
            };
            EngineError::Unsupported(alloc::format!(
                "duplicate key value violates unique constraint \"{conname}\" \
                 on table \"{table_name}\""
            ))
        };
        // v7.39 (round 166, attack A3) — probe path first.
        if let Some(pidx) = uc_probe_index(table, &uc.columns, uc.nulls_not_distinct)
            && probe_replay(table, pidx, &uc.columns, planned, schema, &key_str, &on_conflict)?
        {
            continue;
        }
        replay(&key_str, &on_conflict)?;
    }

    // ── CREATE UNIQUE INDEX (incl. expression / partial) ──
    let ctx = eval::EvalContext::new(&schema.columns, None);
    for idx in table.indices() {
        if !idx.is_unique {
            continue;
        }
        let is_expr_or_partial = idx.expression.is_some() || idx.partial_predicate.is_some();
        let key_positions = unique_key_positions(idx);
        // A plain unique index whose key columns are untouched can't gain
        // a duplicate; an expression/partial index may read any column.
        if !is_expr_or_partial && !key_positions.iter().any(|c| changed_cols.contains(c)) {
            continue;
        }
        let predicate_expr = match idx.partial_predicate.as_deref() {
            Some(s) => Some(spg_sql::parser::parse_expression(s).map_err(|e| {
                EngineError::Unsupported(alloc::format!(
                    "UNIQUE INDEX {:?} predicate {s:?} failed to re-parse: {e:?}",
                    idx.name
                ))
            })?),
            None => None,
        };
        let expr_key = match idx.expression.as_deref() {
            Some(s) => Some(spg_sql::parser::parse_expression(s).map_err(|e| {
                EngineError::Unsupported(alloc::format!(
                    "UNIQUE INDEX {:?} expression {s:?} failed to re-parse: {e:?}",
                    idx.name
                ))
            })?),
            None => None,
        };
        let key_str = |values: &[Value<'static>]| -> Result<Option<String>, EngineError> {
            // Partial index: rows failing the predicate are not indexed.
            if let Some(pred) = &predicate_expr {
                let tmp_row = spg_storage::Row {
                    values: values.to_vec(),
                };
                let v = eval::eval_expr(pred, &tmp_row, &ctx).map_err(|e| {
                    EngineError::Unsupported(alloc::format!(
                        "UNIQUE INDEX {:?} predicate eval: {e:?}",
                        idx.name
                    ))
                })?;
                if !predicate_truthy(&v) {
                    return Ok(None);
                }
            }
            let key: Vec<Value<'static>> = if let Some(expr) = &expr_key {
                let tmp_row = spg_storage::Row {
                    values: values.to_vec(),
                };
                let v = eval::eval_expr(expr, &tmp_row, &ctx).map_err(|e| {
                    EngineError::Unsupported(alloc::format!(
                        "UNIQUE INDEX {:?} expression eval: {e:?}",
                        idx.name
                    ))
                })?;
                alloc::vec![v]
            } else {
                key_positions
                    .iter()
                    .map(|&p| {
                        let v = values.get(p).cloned().unwrap_or(Value::Null);
                        collated_key_cell(&v, p, schema)
                    })
                    .collect()
            };
            if key.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(None);
            }
            Ok(Some(aggregate::encode_key(&key)))
        };
        let on_conflict = |pos: usize| -> EngineError {
            EngineError::Unsupported(alloc::format!(
                "UNIQUE INDEX {:?} violation on {table_name:?}: \
                 UPDATE of row #{pos} duplicates an existing key",
                idx.name
            ))
        };
        // v7.39 (round 166, attack A3) — a plain unique index probes its
        // own btree (expression / partial / NULLS-NOT-DISTINCT / collated
        // shapes stay on the fold replay).
        if !is_expr_or_partial
            && !idx.nulls_not_distinct
            && matches!(idx.kind, spg_storage::IndexKind::BTree(_))
            && key_positions.iter().all(|&i| {
                schema.columns.get(i).is_some_and(|c| {
                    !matches!(c.collation, spg_storage::Collation::CaseInsensitive)
                })
            })
            && schema
                .columns
                .get(idx.column_position)
                .is_some_and(|c| indexkeyable_type(&c.ty))
            && probe_replay(
                table,
                idx,
                &key_positions,
                planned,
                schema,
                &key_str,
                &on_conflict,
            )?
        {
            continue;
        }
        replay(&key_str, &on_conflict)?;
    }
    Ok(())
}

/// v7.13.0 — `UPDATE OF cols` filter helper (mailrs round-5 G7).
/// Returns `true` when at least one of `filter_cols` has a
/// different value in `new_row` vs `old_row`. Column lookup is
/// case-insensitive against `schema_cols`; unknown filter columns
/// are treated as "not changed" (the trigger therefore won't
/// fire on them — surfacing a parse-time error would be too
/// strict for catalog reloads where the schema may have drifted).
pub(crate) fn any_column_changed(
    filter_cols: &[String],
    schema_cols: &[ColumnSchema],
    old_row: &Row<'static>,
    new_row: &Row<'static>,
) -> bool {
    for col_name in filter_cols {
        let Some(pos) = schema_cols
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(col_name))
        else {
            continue;
        };
        let old_v = old_row.values.get(pos);
        let new_v = new_row.values.get(pos);
        if old_v != new_v {
            return true;
        }
    }
    false
}

/// v7.39 (read01 round 117) — PG's "Failing row contains (...)" tuple text,
/// shared by the 23514 (CHECK) and 23502 (NOT NULL) DETAIL lines. Each cell is
/// rendered as PG prints it in a row constructor: a JSON `null` → `null`, text
/// verbatim (unquoted, commas and all), everything else via `value_to_text`.
pub(crate) fn format_failing_row(row_values: &[Value<'static>]) -> String {
    row_values
        .iter()
        .map(|v| match v {
            Value::Null => "null".to_string(),
            Value::Text(s) => s.to_string(),
            other => crate::eval::value_to_text(other),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// v7.39 (read01 round 117) — PG's 23502 NOT NULL check over a batch of
/// fully-assembled rows (defaults / generated columns already applied).
/// Raised PRE-WRITE alongside the FK / CHECK guards, so a violating row aborts
/// the whole statement before any row is written (no partial rows) and carries
/// PG's `DETAIL: Failing row contains (...)`. Nullability is the schema's own
/// per-column flag — the same one the storage insert path checks — so this is a
/// pre-write mirror with the row context, not a second policy.
pub(crate) fn enforce_not_null(
    catalog: &Catalog,
    table_name: &str,
    rows: &[alloc::vec::Vec<Value<'static>>],
) -> Result<(), EngineError> {
    let table = catalog.get(table_name).ok_or_else(|| {
        EngineError::Storage(StorageError::TableNotFound {
            name: table_name.into(),
        })
    })?;
    let cols = &table.schema().columns;
    for row in rows {
        for (val, col) in row.iter().zip(cols) {
            if val.is_null() && !col.nullable {
                return Err(EngineError::Unsupported(alloc::format!(
                    "null value in column \"{}\" of relation \"{table_name}\" \
                     violates not-null constraint DETAIL: Failing row contains ({}).",
                    col.name,
                    format_failing_row(row)
                )));
            }
        }
    }
    Ok(())
}

/// v7.13.0 — evaluate every CHECK predicate on the schema against
/// each candidate row. Mirrors PG semantics: a `false` result
/// rejects the mutation; a NULL result *passes* (CHECK rejects
/// only on definite-false, not on unknown). mailrs round-5 G3.
pub(crate) fn enforce_check_constraints(
    catalog: &Catalog,
    table_name: &str,
    rows: &[alloc::vec::Vec<spg_storage::Value<'static>>],
) -> Result<(), EngineError> {
    let table = catalog.get(table_name).ok_or_else(|| {
        EngineError::Storage(StorageError::TableNotFound {
            name: table_name.into(),
        })
    })?;
    let schema = table.schema();
    // v7.17.0 Phase 1.5 — domain-level CHECKs are enforced in
    // parallel with table-level CHECKs. Collect both lists up
    // front; if neither exists we early-out.
    let mut domain_checks_per_col: alloc::vec::Vec<(usize, alloc::vec::Vec<Expr>)> =
        alloc::vec::Vec::new();
    for (idx, col) in schema.columns.iter().enumerate() {
        let Some(dname) = &col.user_domain_type else {
            continue;
        };
        let Some(dom) = catalog.domain_types().get(dname) else {
            continue;
        };
        let mut parsed_for_col: alloc::vec::Vec<Expr> =
            alloc::vec::Vec::with_capacity(dom.checks.len());
        for src in &dom.checks {
            let expr = spg_sql::parser::parse_expression(src).map_err(|e| {
                EngineError::Unsupported(alloc::format!(
                    "DOMAIN {dname:?} CHECK ({src:?}) on column {:?}: re-parse failed: {e:?}",
                    col.name
                ))
            })?;
            parsed_for_col.push(expr);
        }
        if !parsed_for_col.is_empty() {
            domain_checks_per_col.push((idx, parsed_for_col));
        }
    }
    if schema.checks.is_empty() && domain_checks_per_col.is_empty() {
        return Ok(());
    }
    let ctx = eval::EvalContext::new(&schema.columns, None);
    let mut parsed: alloc::vec::Vec<(usize, Expr)> = alloc::vec::Vec::new();
    for (i, src) in schema.checks.iter().enumerate() {
        let expr = spg_sql::parser::parse_expression(&src.expr).map_err(|e| {
            let pred = &src.expr;
            EngineError::Unsupported(alloc::format!(
                "CHECK constraint #{i} on {table_name:?} ({pred:?}) failed to re-parse: {e:?}"
            ))
        })?;
        parsed.push((i, expr));
    }
    for (batch_idx, row_values) in rows.iter().enumerate() {
        let tmp_row = spg_storage::Row {
            values: row_values.clone(),
        };
        for (i, expr) in &parsed {
            let v = eval::eval_expr(expr, &tmp_row, &ctx).map_err(|e| {
                EngineError::Unsupported(alloc::format!(
                    "CHECK constraint #{i} on {table_name:?} eval at row #{batch_idx}: {e:?}"
                ))
            })?;
            // PG: NULL passes (CHECK rejects on definite-false only).
            if matches!(v, spg_storage::Value::Bool(false)) {
                // v7.39 (SQLSTATE fidelity) — PG's exact 23514 phrasing.
                let names =
                    crate::system_catalog::pg_check_connames(table, table_name, &schema.checks);
                let conname = names
                    .get(*i)
                    .cloned()
                    .unwrap_or_else(|| alloc::format!("{table_name}_check"));
                let failing = format_failing_row(row_values);
                return Err(EngineError::Unsupported(alloc::format!(
                    "new row for relation \"{table_name}\" violates check constraint \
                     \"{conname}\" DETAIL: Failing row contains ({failing})."
                )));
            }
        }
        // v7.17.0 Phase 1.5 — domain-level CHECKs. Each CHECK
        // expression references VALUE as a column-name; we
        // substitute the per-row cell into the eval context by
        // synthesising a single-column row of just that value
        // under a temporary `value` column schema.
        for (col_idx, checks) in &domain_checks_per_col {
            let cell = row_values
                .get(*col_idx)
                .cloned()
                .unwrap_or(spg_storage::Value::Null);
            let synth_cols = alloc::vec![spg_storage::ColumnSchema::new(
                "value",
                schema.columns[*col_idx].ty,
                schema.columns[*col_idx].nullable,
            )];
            let synth_ctx = eval::EvalContext::new(&synth_cols, None);
            let synth_row = spg_storage::Row {
                values: alloc::vec![cell],
            };
            for (ci, expr) in checks.iter().enumerate() {
                let v = eval::eval_expr(expr, &synth_row, &synth_ctx).map_err(|e| {
                    EngineError::Unsupported(alloc::format!(
                        "DOMAIN CHECK #{ci} on column {:?} eval at row #{batch_idx}: {e:?}",
                        schema.columns[*col_idx].name
                    ))
                })?;
                if matches!(v, spg_storage::Value::Bool(false)) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "DOMAIN CHECK violation on column {:?} (row #{batch_idx})",
                        schema.columns[*col_idx].name
                    )));
                }
            }
        }
    }
    Ok(())
}

/// v7.36 — enumerate cold-tier rows of `parent` for FK / UNIQUE
/// validation paths that can't reach `Engine::iter_cold_rows_of_table`
/// (free-function callers with a `&Catalog` instead of `&Engine`).
/// Same shape: PK-backed BTree iteration + `resolve_cold_locator`
/// per cold locator, no dedup state because the PK uniqueness
/// contract gives per-row uniqueness.
pub(crate) fn iter_cold_rows_of_parent(
    catalog: &Catalog,
    parent: &spg_storage::Table,
) -> Vec<Row<'static>> {
    let schema = parent.schema();
    let Some(pk_col_pos) = schema
        .uniqueness_constraints
        .iter()
        .find(|u| u.is_primary_key && u.columns.len() == 1)
        .map(|u| u.columns[0])
    else {
        return Vec::new();
    };
    let Some(idx) = parent.indices().iter().find(|i| {
        i.column_position == pk_col_pos && matches!(i.kind, spg_storage::IndexKind::BTree(_))
    }) else {
        return Vec::new();
    };
    let table_name = schema.name.as_str();
    let mut out = Vec::new();
    for (key, locators) in idx.iter_asc() {
        for loc in locators {
            if let spg_storage::RowLocator::Cold { segment_id, .. } = loc
                && let Some(row) = catalog.resolve_cold_locator(table_name, *segment_id, key)
            {
                out.push(row);
            }
        }
    }
    out
}

/// v7.36 — companion to `iter_cold_rows_of_parent` that also
/// surfaces the PK key alongside each cold-tier row. Used by
/// UPDATE / DELETE non-PK WHERE paths to promote / shadow each
/// matching cold-tier row by its PK key (the only key
/// `Catalog::promote_cold_row` and `shadow_cold_row` accept).
/// v7.36 — companion to `iter_cold_rows_of_parent` that also
/// builds a `(segment_id, page_offset) → cold_offset` map for the
/// INL probe. Walking the PK BTree yields one cold row per
/// uniquely-identified locator (the PK uniqueness contract gives
/// per-row dedup), so the offset assigned during materialisation
/// is the row's index in the returned Vec. The map is then used
/// by `JoinSrc::Mixed::cold_locator_offset` to translate a Cold
/// locator coming from ANY index on the same table — locators
/// across indices share the same `(segment_id, page_offset)` for
/// the same row.
pub(crate) fn iter_cold_rows_with_locator_map(
    catalog: &Catalog,
    table: &spg_storage::Table,
) -> (Vec<Row<'static>>, hashbrown::HashMap<i64, usize>) {
    let schema = table.schema();
    let Some(pk_col_pos) = schema
        .uniqueness_constraints
        .iter()
        .find(|u| u.is_primary_key && u.columns.len() == 1)
        .map(|u| u.columns[0])
    else {
        return (Vec::new(), hashbrown::HashMap::new());
    };
    let Some(idx) = table.indices().iter().find(|i| {
        i.column_position == pk_col_pos && matches!(i.kind, spg_storage::IndexKind::BTree(_))
    }) else {
        return (Vec::new(), hashbrown::HashMap::new());
    };
    let table_name = schema.name.as_str();
    let mut rows = Vec::new();
    let mut map: hashbrown::HashMap<i64, usize> = hashbrown::HashMap::new();
    for (key, locators) in idx.iter_asc() {
        // Keyed by the integer PK value — the cold-tier architecture
        // already requires an integer PK (`index_key_as_u64` is what
        // `resolve_cold_locator` calls), so locators whose
        // `IndexKey` isn't `Int` never resolve and are skipped.
        let spg_storage::IndexKey::Int(pk_value) = key else {
            continue;
        };
        for loc in locators {
            if let spg_storage::RowLocator::Cold { segment_id, .. } = loc
                && let Some(row) = catalog.resolve_cold_locator(table_name, *segment_id, key)
            {
                let offset = rows.len();
                rows.push(row);
                map.insert(*pk_value, offset);
            }
        }
    }
    (rows, map)
}

pub(crate) fn iter_cold_rows_with_pk_key(
    catalog: &Catalog,
    table: &spg_storage::Table,
) -> Vec<(spg_storage::IndexKey, Row<'static>)> {
    let schema = table.schema();
    let Some(pk_col_pos) = schema
        .uniqueness_constraints
        .iter()
        .find(|u| u.is_primary_key && u.columns.len() == 1)
        .map(|u| u.columns[0])
    else {
        return Vec::new();
    };
    let Some(idx) = table.indices().iter().find(|i| {
        i.column_position == pk_col_pos && matches!(i.kind, spg_storage::IndexKind::BTree(_))
    }) else {
        return Vec::new();
    };
    let table_name = schema.name.as_str();
    let mut out = Vec::new();
    for (key, locators) in idx.iter_asc() {
        for loc in locators {
            if let spg_storage::RowLocator::Cold { segment_id, .. } = loc
                && let Some(row) = catalog.resolve_cold_locator(table_name, *segment_id, key)
            {
                out.push((key.clone(), row));
            }
        }
    }
    out
}

/// v7.36 — name of the PK BTree index on `table` if there's a
/// single-column PRIMARY KEY. Used by UPDATE / DELETE cold-tier
/// fixup paths to thread the PK index name into
/// `Catalog::promote_cold_row` / `shadow_cold_row`.
pub(crate) fn pk_btree_index_name(table: &spg_storage::Table) -> Option<String> {
    let schema = table.schema();
    let pk_col_pos = schema
        .uniqueness_constraints
        .iter()
        .find(|u| u.is_primary_key && u.columns.len() == 1)
        .map(|u| u.columns[0])?;
    table.indices().iter().find_map(|i| {
        if i.column_position == pk_col_pos && matches!(i.kind, spg_storage::IndexKind::BTree(_)) {
            Some(i.name.clone())
        } else {
            None
        }
    })
}

pub(crate) fn enforce_fk_inserts(
    catalog: &Catalog,
    child_table: &str,
    fks: &[spg_storage::ForeignKeyConstraint],
    rows: &[Vec<Value<'static>>],
) -> Result<(), EngineError> {
    for fk in fks {
        let parent_is_self = fk.parent_table == child_table;
        let parent = if parent_is_self {
            // Self-ref: read the current state of the same table.
            // The mut borrow on child has been dropped by the caller.
            catalog.get(child_table).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: child_table.into(),
                })
            })?
        } else {
            catalog.get(&fk.parent_table).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: fk.parent_table.clone(),
                })
            })?
        };
        // v7.36 (cold-tier coverage) — composite FK check walks
        // `parent.rows().iter()` looking for a tuple match. That
        // skipped cold-tier parent rows, so a child INSERT whose
        // matching parent had been frozen to cold raised
        // `FOREIGN KEY violation: no parent row` falsely. Materialise
        // the cold parent rows ONCE per FK (the composite path only
        // — single-column FKs already ride `idx.lookup_eq` which
        // surfaces both tiers).
        let cold_parent_rows: alloc::vec::Vec<Row<'static>> = if fk.local_columns.len() == 1 {
            Vec::new()
        } else {
            iter_cold_rows_of_parent(catalog, parent)
        };
        for (batch_idx, row_values) in rows.iter().enumerate() {
            // Single-column FK fast path: try the parent's BTree
            // index for an O(log n) lookup. Composite FKs fall back
            // to a parent-row scan.
            if fk.local_columns.len() == 1 {
                let v = &row_values[fk.local_columns[0]];
                if matches!(v, Value::Null) {
                    continue;
                }
                let parent_col = fk.parent_columns[0];
                let key = spg_storage::IndexKey::from_value(v).ok_or_else(|| {
                    EngineError::Unsupported(alloc::format!(
                        "FOREIGN KEY column value of type {:?} is not index-eligible",
                        v.data_type()
                    ))
                })?;
                let present_committed = parent.indices().iter().any(|idx| {
                    matches!(idx.kind, spg_storage::IndexKind::BTree(_))
                        && idx.column_position == parent_col
                        && idx.partial_predicate.is_none()
                        // v7.37.15 (Phase C.3) — a tombstoned parent index
                        // hit means the parent was DELETE-tombstoned under
                        // the gate-on in-place path; the parent is gone, so
                        // the child FK insert must FAIL "no parent" (PG
                        // agrees — a deleted parent violates the FK). Gate-off
                        // has no tombstones → every locator counts → unchanged.
                        && idx
                            .lookup_eq(&key)
                            .iter()
                            .any(|loc| !locator_is_tombstoned(parent, loc))
                });
                // v7.6.7 self-ref widening: also accept a match
                // against earlier rows in this same batch when the
                // FK points at the table being inserted into.
                let present_in_batch = parent_is_self
                    && rows[..batch_idx]
                        .iter()
                        .any(|earlier| earlier.get(parent_col) == Some(v));
                if !(present_committed || present_in_batch) {
                    // v7.39 (SQLSTATE fidelity) — PG's exact 23503 phrasing.
                    let child = catalog.get(child_table).ok_or_else(|| {
                        EngineError::Storage(StorageError::TableNotFound {
                            name: child_table.into(),
                        })
                    })?;
                    return Err(EngineError::Unsupported(fk_violation_message(
                        child,
                        child_table,
                        fk,
                        &[v],
                    )));
                }
            } else {
                // Composite FK: scan parent rows. v7.6.7 also
                // accepts a match against earlier rows in the same
                // batch (self-ref bulk-loading of hierarchies).
                // v7.38 (read01, T29) — MATCH SIMPLE skips the check when ANY
                // referencing column is NULL; MATCH FULL skips only when they
                // are ALL NULL, and a mixed-NULL key is an error.
                let null_cnt = fk
                    .local_columns
                    .iter()
                    .filter(|&&i| matches!(row_values.get(i), Some(Value::Null)))
                    .count();
                match fk.match_type {
                    spg_storage::MatchType::Simple => {
                        if null_cnt > 0 {
                            continue;
                        }
                    }
                    spg_storage::MatchType::Full => {
                        if null_cnt == fk.local_columns.len() {
                            continue;
                        }
                        if null_cnt > 0 {
                            return Err(EngineError::Unsupported(
                                "insert or update violates foreign key constraint: MATCH FULL \
                                 does not allow mixing of null and nonnull key values"
                                    .into(),
                            ));
                        }
                    }
                }
                let local: Vec<&Value> = fk.local_columns.iter().map(|&i| &row_values[i]).collect();
                let matches_parent_row = |prow: &Row<'static>| {
                    fk.parent_columns
                        .iter()
                        .enumerate()
                        .all(|(i, &pi)| prow.values.get(pi) == Some(local[i]))
                };
                // v7.37.15 (Phase C.3) — a gate-on DELETE-tombstoned hot
                // parent row is gone, so it must not satisfy the composite
                // FK (mirror of the single-column fast path above). Cold
                // parent rows cannot be tombstoned in place. `is_deleted()`
                // is never true under the default gate → gate-off unchanged.
                let hot_parent_match = parent.rows().iter().enumerate().any(|(row_idx, prow)| {
                    !parent
                        .headers()
                        .get(row_idx)
                        .is_some_and(|h| h.is_deleted())
                        && matches_parent_row(prow)
                });
                let parent_match_committed =
                    hot_parent_match || cold_parent_rows.iter().any(&matches_parent_row);
                let parent_match_in_batch = parent_is_self
                    && rows[..batch_idx].iter().any(|earlier| {
                        fk.parent_columns
                            .iter()
                            .enumerate()
                            .all(|(i, &pi)| earlier.get(pi) == Some(local[i]))
                    });
                if !(parent_match_committed || parent_match_in_batch) {
                    let child = catalog.get(child_table).ok_or_else(|| {
                        EngineError::Storage(StorageError::TableNotFound {
                            name: child_table.into(),
                        })
                    })?;
                    return Err(EngineError::Unsupported(fk_violation_message(
                        child,
                        child_table,
                        fk,
                        &local,
                    )));
                }
            }
        }
    }
    Ok(())
}

/// v7.6.4 / v7.6.5 — one step of the FK action plan computed for a
/// DELETE on a parent. The plan is a list of these steps, stacked
/// across the FK graph by `plan_fk_parent_deletions`.
#[derive(Debug, Clone)]
pub(crate) struct FkChildStep {
    child_table: String,
    action: FkChildAction,
}

#[derive(Debug, Clone)]
pub(crate) enum FkChildAction {
    /// CASCADE — remove these rows. Sorted, deduplicated positions.
    Delete { positions: Vec<usize> },
    /// SET NULL — for each (row, column) in the flat list, write
    /// NULL into that child cell. Multiple FKs on the same row may
    /// produce overlapping entries (deduped at plan time).
    SetNull {
        positions: Vec<usize>,
        columns: Vec<usize>,
    },
    /// SET DEFAULT — same shape as SetNull but writes the column's
    /// declared DEFAULT value (resolved at plan time). Columns
    /// without a DEFAULT raise an error during planning.
    SetDefault {
        positions: Vec<usize>,
        columns: Vec<usize>,
        defaults: Vec<Value<'static>>,
    },
}

/// v7.6.3 → v7.6.5 — plan FK fallout for a DELETE on a parent table.
///
/// Walks every table in the catalog looking for FKs whose
/// `parent_table` is `parent_table_name`. For each such FK + each
/// to-be-deleted parent row:
///
///   - RESTRICT / NoAction → error, no plan returned
///   - CASCADE → child rows get scheduled for deletion; recursive
///   - SetNull → child FK column(s) scheduled to be NULL-ed.
///     Verified NULL-able at plan time.
///   - SetDefault → child FK column(s) scheduled to be reset to
///     their declared DEFAULT. Columns without a DEFAULT raise.
///
/// SET NULL / SET DEFAULT do NOT cascade further — the child row
/// stays; only one of its columns mutates.
/// v7.37.16 — does ANY table in the catalog declare a foreign key whose
/// parent is `table_name`? Cheap per-statement pre-check that lets the
/// DELETE path skip snapshotting old-row values when no FK enforcement
/// (and no trigger / RETURNING) will ever read them.
pub(crate) fn any_fk_child_references(catalog: &Catalog, table_name: &str) -> bool {
    catalog.table_names().into_iter().any(|child_name| {
        catalog.get(&child_name).is_some_and(|c| {
            c.schema()
                .foreign_keys
                .iter()
                .any(|fk| fk.parent_table == table_name)
        })
    })
}

pub(crate) fn plan_fk_parent_deletions(
    catalog: &Catalog,
    parent_table_name: &str,
    to_delete_positions: &[usize],
    to_delete_rows: &[Vec<Value<'static>>],
) -> Result<Vec<FkChildStep>, EngineError> {
    use alloc::collections::{BTreeMap, BTreeSet};
    if to_delete_rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut delete_plan: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    // setnull / setdefault keyed by child_table → (row_idx, col_idx) → optional default
    let mut setnull_plan: BTreeMap<String, BTreeSet<(usize, usize)>> = BTreeMap::new();
    let mut setdefault_plan: BTreeMap<String, BTreeMap<(usize, usize), Value>> = BTreeMap::new();
    let mut visited: BTreeSet<(String, usize)> = BTreeSet::new();
    for &p in to_delete_positions {
        visited.insert((parent_table_name.to_string(), p));
    }
    let mut work: Vec<(String, Vec<Value<'static>>)> = to_delete_rows
        .iter()
        .map(|r| (parent_table_name.to_string(), r.clone()))
        .collect();
    while let Some((cur_parent, parent_row)) = work.pop() {
        for child_name in catalog.table_names() {
            let child = catalog
                .get(&child_name)
                .expect("table_names → catalog.get round-trip is total");
            for fk in &child.schema().foreign_keys {
                if fk.parent_table != cur_parent {
                    continue;
                }
                let parent_key: Vec<&Value> = fk
                    .parent_columns
                    .iter()
                    .map(|&pi| &parent_row[pi])
                    .collect();
                if parent_key.iter().any(|v| matches!(v, Value::Null)) {
                    continue;
                }
                // v7.36 (cold-tier coverage) — DELETE-cascade FK
                // planner walked `child.rows()` only. Any cold-tier
                // child referencing the doomed parent was silently
                // skipped: with RESTRICT/NoAction the violation went
                // undetected (lost integrity); with Cascade/SetNull/
                // SetDefault the child row was orphaned (cold rows
                // can't be mutated in-place by this planner). Raise
                // explicitly when a cold child reference exists so
                // the operator sees the architectural gap rather than
                // silent corruption.
                if iter_cold_rows_of_parent(catalog, child).iter().any(|crow| {
                    fk.local_columns
                        .iter()
                        .enumerate()
                        .all(|(i, &li)| crow.values.get(li) == Some(parent_key[i]))
                }) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "DELETE on {cur_parent:?}: cold-tier child row in {child_name:?} \
                         references the doomed parent key; cold-tier mutation by this \
                         FK action is a v7.37 candidate. Run COMPACT or move the cold \
                         rows back to the hot tier and retry."
                    )));
                }
                for (child_row_idx, child_row) in child.rows().iter().enumerate() {
                    if child_name == cur_parent
                        && visited.contains(&(child_name.clone(), child_row_idx))
                    {
                        continue;
                    }
                    let matches_key = fk
                        .local_columns
                        .iter()
                        .enumerate()
                        .all(|(i, &li)| child_row.values.get(li) == Some(parent_key[i]));
                    if !matches_key {
                        continue;
                    }
                    match fk.on_delete {
                        spg_storage::FkAction::Restrict | spg_storage::FkAction::NoAction => {
                            // v7.39 (SQLSTATE fidelity) — PG's exact phrasing.
                            return Err(EngineError::Unsupported(fk_restrict_message(
                                catalog,
                                &cur_parent,
                                child,
                                &child_name,
                                fk,
                                &parent_key,
                            )));
                        }
                        spg_storage::FkAction::Cascade => {
                            if visited.insert((child_name.clone(), child_row_idx)) {
                                delete_plan
                                    .entry(child_name.clone())
                                    .or_default()
                                    .insert(child_row_idx);
                                work.push((child_name.clone(), child_row.values.clone()));
                            }
                        }
                        spg_storage::FkAction::SetNull => {
                            // Verify every local FK column is NULL-able.
                            for &li in &fk.local_columns {
                                let col = child.schema().columns.get(li).ok_or_else(|| {
                                    EngineError::Unsupported(alloc::format!(
                                        "FK local column {li} missing in {child_name:?}"
                                    ))
                                })?;
                                if !col.nullable {
                                    return Err(EngineError::Unsupported(alloc::format!(
                                        "FOREIGN KEY ON DELETE SET NULL: column \
                                         {child_name:?}.{:?} is NOT NULL — cannot SET NULL",
                                        col.name,
                                    )));
                                }
                            }
                            let entry = setnull_plan.entry(child_name.clone()).or_default();
                            for &li in &fk.local_columns {
                                entry.insert((child_row_idx, li));
                            }
                        }
                        spg_storage::FkAction::SetDefault => {
                            // Resolve the DEFAULT for every local FK col.
                            let entry = setdefault_plan.entry(child_name.clone()).or_default();
                            for &li in &fk.local_columns {
                                let col = child.schema().columns.get(li).ok_or_else(|| {
                                    EngineError::Unsupported(alloc::format!(
                                        "FK local column {li} missing in {child_name:?}"
                                    ))
                                })?;
                                let default = col.default.clone().ok_or_else(|| {
                                    EngineError::Unsupported(alloc::format!(
                                        "FOREIGN KEY ON DELETE SET DEFAULT: column \
                                         {child_name:?}.{:?} has no DEFAULT declared",
                                        col.name,
                                    ))
                                })?;
                                entry.insert((child_row_idx, li), default);
                            }
                        }
                    }
                }
            }
        }
    }
    // Flatten the three plans into the ordered `FkChildStep` list.
    // Deletes are applied last per child (after any null/default
    // re-writes on the same child) so a child row that's both
    // re-written and then cascade-deleted only ends up deleted —
    // but in v7.6.5 SetNull/Cascade never overlap on the same row
    // (a single FK chooses exactly one action), so the order is
    // mostly a precaution.
    let mut steps: Vec<FkChildStep> = Vec::new();
    for (child_table, entries) in setnull_plan {
        let (positions, columns): (Vec<usize>, Vec<usize>) = entries.into_iter().unzip();
        steps.push(FkChildStep {
            child_table,
            action: FkChildAction::SetNull { positions, columns },
        });
    }
    for (child_table, entries) in setdefault_plan {
        let mut positions = Vec::with_capacity(entries.len());
        let mut columns = Vec::with_capacity(entries.len());
        let mut defaults = Vec::with_capacity(entries.len());
        for ((p, c), v) in entries {
            positions.push(p);
            columns.push(c);
            defaults.push(v);
        }
        steps.push(FkChildStep {
            child_table,
            action: FkChildAction::SetDefault {
                positions,
                columns,
                defaults,
            },
        });
    }
    for (child_table, positions) in delete_plan {
        steps.push(FkChildStep {
            child_table,
            action: FkChildAction::Delete {
                positions: positions.into_iter().collect(),
            },
        });
    }
    Ok(steps)
}

/// v7.6.6 — plan FK fallout for an UPDATE that mutates parent-side
/// PK/UNIQUE columns. Walks every other table whose FK references
/// `parent_table_name`; for each FK whose parent_columns overlap a
/// mutated column, decides the action by `fk.on_update`.
///
///   - RESTRICT / NoAction → error if any child references the OLD
///     value
///   - CASCADE → child FK columns get rewritten to the NEW parent
///     value (a SetNull-style update step with the new value)
///   - SetNull → child FK columns set to NULL
///   - SetDefault → child FK columns set to declared default
///
/// `plan_with_old` is `(row_position, old_values, new_values)` so
/// the planner can detect "did this row's parent key actually
/// change?" — only rows where at least one referenced parent
/// column moved trigger inbound work.
pub(crate) fn plan_fk_parent_updates(
    catalog: &Catalog,
    parent_table_name: &str,
    plan_with_old: &[(usize, Vec<Value<'static>>, Vec<Value<'static>>)],
) -> Result<Vec<FkChildStep>, EngineError> {
    use alloc::collections::BTreeMap;
    if plan_with_old.is_empty() {
        return Ok(Vec::new());
    }
    // For each child table we may touch, build per-child step
    // lists. UPDATE never deletes children — `delete_plan` stays
    // empty here but is kept structurally aligned with
    // `plan_fk_parent_deletions` for future use.
    let delete_plan: BTreeMap<String, alloc::collections::BTreeSet<usize>> = BTreeMap::new();
    let mut setnull_plan: BTreeMap<String, alloc::collections::BTreeSet<(usize, usize)>> =
        BTreeMap::new();
    let mut setdefault_plan: BTreeMap<String, BTreeMap<(usize, usize), Value>> = BTreeMap::new();
    // Cascade-update plan: child_table → row_idx → col_idx → new_value
    let mut cascade_plan: BTreeMap<String, BTreeMap<(usize, usize), Value>> = BTreeMap::new();

    for child_name in catalog.table_names() {
        let child = catalog
            .get(&child_name)
            .expect("table_names → catalog.get total");
        for fk in &child.schema().foreign_keys {
            if fk.parent_table != parent_table_name {
                continue;
            }
            for (_pos, old_row, new_row) in plan_with_old {
                // Did any parent FK column change?
                let key_changed = fk
                    .parent_columns
                    .iter()
                    .any(|&pi| old_row.get(pi) != new_row.get(pi));
                if !key_changed {
                    continue;
                }
                // The OLD parent key — used to find referring children.
                let old_key: Vec<&Value> =
                    fk.parent_columns.iter().map(|&pi| &old_row[pi]).collect();
                if old_key.iter().any(|v| matches!(v, Value::Null)) {
                    // NULL parent has no children — skip.
                    continue;
                }
                let new_key: Vec<&Value> =
                    fk.parent_columns.iter().map(|&pi| &new_row[pi]).collect();
                // v7.36 (cold-tier coverage) — UPDATE-cascade FK
                // planner mirrors DELETE: any cold child referencing
                // the OLD parent key would be silently skipped, so
                // RESTRICT misses violations and Cascade/SetNull/
                // SetDefault orphans the cold child. Raise explicitly.
                if iter_cold_rows_of_parent(catalog, child).iter().any(|crow| {
                    fk.local_columns
                        .iter()
                        .enumerate()
                        .all(|(i, &li)| crow.values.get(li) == Some(old_key[i]))
                }) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "UPDATE on {parent_table_name:?}: cold-tier child row in \
                         {child_name:?} references the changing parent key; cold-tier \
                         mutation by this FK action is a v7.37 candidate. Run COMPACT \
                         or move the cold rows back to the hot tier and retry."
                    )));
                }
                for (child_row_idx, child_row) in child.rows().iter().enumerate() {
                    // Self-ref same-row updates: a row updating its
                    // own PK doesn't restrict itself.
                    if child_name == parent_table_name
                        && plan_with_old.iter().any(|(p, _, _)| *p == child_row_idx)
                    {
                        continue;
                    }
                    let matches_key = fk
                        .local_columns
                        .iter()
                        .enumerate()
                        .all(|(i, &li)| child_row.values.get(li) == Some(old_key[i]));
                    if !matches_key {
                        continue;
                    }
                    match fk.on_update {
                        spg_storage::FkAction::Restrict | spg_storage::FkAction::NoAction => {
                            return Err(EngineError::Unsupported(fk_restrict_message(
                                catalog,
                                parent_table_name,
                                child,
                                &child_name,
                                fk,
                                &old_key,
                            )));
                        }
                        spg_storage::FkAction::Cascade => {
                            // Rewrite child FK columns to new key.
                            let entry = cascade_plan.entry(child_name.clone()).or_default();
                            for (i, &li) in fk.local_columns.iter().enumerate() {
                                entry.insert((child_row_idx, li), new_key[i].clone());
                            }
                        }
                        spg_storage::FkAction::SetNull => {
                            for &li in &fk.local_columns {
                                let col = child.schema().columns.get(li).ok_or_else(|| {
                                    EngineError::Unsupported(alloc::format!(
                                        "FK local column {li} missing in {child_name:?}"
                                    ))
                                })?;
                                if !col.nullable {
                                    return Err(EngineError::Unsupported(alloc::format!(
                                        "FOREIGN KEY ON UPDATE SET NULL: column \
                                         {child_name:?}.{:?} is NOT NULL",
                                        col.name,
                                    )));
                                }
                            }
                            let entry = setnull_plan.entry(child_name.clone()).or_default();
                            for &li in &fk.local_columns {
                                entry.insert((child_row_idx, li));
                            }
                        }
                        spg_storage::FkAction::SetDefault => {
                            let entry = setdefault_plan.entry(child_name.clone()).or_default();
                            for &li in &fk.local_columns {
                                let col = child.schema().columns.get(li).ok_or_else(|| {
                                    EngineError::Unsupported(alloc::format!(
                                        "FK local column {li} missing in {child_name:?}"
                                    ))
                                })?;
                                let default = col.default.clone().ok_or_else(|| {
                                    EngineError::Unsupported(alloc::format!(
                                        "FOREIGN KEY ON UPDATE SET DEFAULT: column \
                                         {child_name:?}.{:?} has no DEFAULT",
                                        col.name,
                                    ))
                                })?;
                                entry.insert((child_row_idx, li), default);
                            }
                        }
                    }
                }
            }
        }
    }
    // Flatten into FkChildStep list. UPDATE doesn't produce
    // DeleteSteps (CASCADE on UPDATE just rewrites FK values).
    let mut steps: Vec<FkChildStep> = Vec::new();
    for (child_table, entries) in cascade_plan {
        let mut positions = Vec::with_capacity(entries.len());
        let mut columns = Vec::with_capacity(entries.len());
        let mut defaults = Vec::with_capacity(entries.len());
        for ((p, c), v) in entries {
            positions.push(p);
            columns.push(c);
            defaults.push(v);
        }
        // We reuse `FkChildAction::SetDefault` for cascade-update:
        // both shapes are "write a known value into specific cells"
        // — `apply_per_cell_writes` doesn't care whether the value
        // came from a DEFAULT declaration or a new parent key.
        steps.push(FkChildStep {
            child_table,
            action: FkChildAction::SetDefault {
                positions,
                columns,
                defaults,
            },
        });
    }
    for (child_table, entries) in setnull_plan {
        let (positions, columns): (Vec<usize>, Vec<usize>) = entries.into_iter().unzip();
        steps.push(FkChildStep {
            child_table,
            action: FkChildAction::SetNull { positions, columns },
        });
    }
    for (child_table, entries) in setdefault_plan {
        let mut positions = Vec::with_capacity(entries.len());
        let mut columns = Vec::with_capacity(entries.len());
        let mut defaults = Vec::with_capacity(entries.len());
        for ((p, c), v) in entries {
            positions.push(p);
            columns.push(c);
            defaults.push(v);
        }
        steps.push(FkChildStep {
            child_table,
            action: FkChildAction::SetDefault {
                positions,
                columns,
                defaults,
            },
        });
    }
    let _ = delete_plan; // UPDATE never deletes children.
    Ok(steps)
}

/// v7.6.5 — apply one FK child step to the catalog. Encapsulates
/// the three action variants so the DELETE executor stays a
/// simple loop over the planned steps.
pub(crate) fn apply_fk_child_step(
    catalog: &mut Catalog,
    step: &FkChildStep,
) -> Result<(), EngineError> {
    let child = catalog.get_mut(&step.child_table).ok_or_else(|| {
        EngineError::Storage(StorageError::TableNotFound {
            name: step.child_table.clone(),
        })
    })?;
    match &step.action {
        FkChildAction::Delete { positions } => {
            let _ = child.delete_rows(positions);
        }
        FkChildAction::SetNull { positions, columns } => {
            apply_per_cell_writes(child, positions, columns, |_| Value::Null)?;
        }
        FkChildAction::SetDefault {
            positions,
            columns,
            defaults,
        } => {
            apply_per_cell_writes(child, positions, columns, |i| defaults[i].clone())?;
        }
    }
    Ok(())
}

/// v7.6.5 — write new values into selected child cells via
/// `Table::update_row` (the catalog's existing UPDATE entry).
/// Groups writes by row position so multi-column updates on the
/// same row only call `update_row` once. `value_for(i)` produces
/// the new value for the i-th (position, column) entry.
fn apply_per_cell_writes(
    child: &mut spg_storage::Table,
    positions: &[usize],
    columns: &[usize],
    mut value_for: impl FnMut(usize) -> Value<'static>,
) -> Result<(), EngineError> {
    use alloc::collections::BTreeMap;
    let mut by_row: BTreeMap<usize, Vec<(usize, Value<'static>)>> = BTreeMap::new();
    for i in 0..positions.len() {
        by_row
            .entry(positions[i])
            .or_default()
            .push((columns[i], value_for(i)));
    }
    for (pos, mutations) in by_row {
        let mut new_values = child.rows()[pos].values.clone();
        for (col, v) in mutations {
            if let Some(slot) = new_values.get_mut(col) {
                *slot = v;
            }
        }
        child
            .update_row(pos, new_values)
            .map_err(EngineError::Storage)?;
    }
    Ok(())
}

fn fk_action_sql_to_storage(a: spg_sql::ast::FkAction) -> spg_storage::FkAction {
    match a {
        spg_sql::ast::FkAction::Restrict => spg_storage::FkAction::Restrict,
        spg_sql::ast::FkAction::Cascade => spg_storage::FkAction::Cascade,
        spg_sql::ast::FkAction::SetNull => spg_storage::FkAction::SetNull,
        spg_sql::ast::FkAction::SetDefault => spg_storage::FkAction::SetDefault,
        spg_sql::ast::FkAction::NoAction => spg_storage::FkAction::NoAction,
    }
}

impl Engine {
    /// v7.14.0 — resolve every queued FK whose installation was
    /// deferred (`SET FOREIGN_KEY_CHECKS=0` window). Called by
    /// `set_session_param` when checks flip back on and by the
    /// drop-import release gate. Each FK is resolved against the
    /// current catalog; remaining missing-parent errors propagate
    /// up so the caller knows the import was incomplete.
    pub(crate) fn drain_pending_foreign_keys(&mut self) -> Result<(), EngineError> {
        let pending = core::mem::take(&mut self.pending_foreign_keys);
        for (child, fk) in pending {
            // Resolve against the current catalog. Skip silently
            // when the child table itself was dropped between
            // queue + drain.
            let cols_snapshot = match self.active_catalog().get(&child) {
                Some(t) => t.schema().columns.clone(),
                None => continue,
            };
            let storage_fk =
                resolve_foreign_key(&child, &cols_snapshot, fk, self.active_catalog())?;
            let table = self
                .active_catalog_mut()
                .get_mut(&child)
                .expect("checked above");
            table.schema_mut().foreign_keys.push(storage_fk);
        }
        Ok(())
    }
}
