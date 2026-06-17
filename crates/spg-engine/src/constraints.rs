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
    Ok(spg_storage::ForeignKeyConstraint {
        name: fk.name,
        local_columns,
        parent_table: fk.parent_table,
        parent_columns,
        on_delete,
        on_update,
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
            && !idx.lookup_eq(&idx_key).is_empty()
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
    table.rows().iter().position(|r| {
        column_positions
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
    table.rows().iter().any(|r| {
        column_positions
            .iter()
            .enumerate()
            .all(|(i, &pos)| r.values.get(pos) == Some(key[i]))
    })
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
/// corresponding column of the returned `Vec<Value>`. If
/// `where_` evaluates falsy, returns Ok(None) — PG behaviour:
/// the conflicting row is silently kept unchanged.
pub(crate) fn apply_on_conflict_assignments(
    catalog: &Catalog,
    table_name: &str,
    target_pos: usize,
    incoming: &[Value],
    assignments: &[(String, Expr)],
    where_: Option<&Expr>,
) -> Result<Option<Vec<Value>>, EngineError> {
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
fn substitute_excluded_refs(expr: Expr, schema_cols: &[ColumnSchema], incoming: &[Value]) -> Expr {
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
pub(crate) fn enforce_uniqueness_inserts(
    catalog: &Catalog,
    child_table: &str,
    constraints: &[spg_storage::UniquenessConstraint],
    rows: &[Vec<Value>],
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
        let fold_key = |values: &[Value]| -> Vec<Value> {
            uc.columns
                .iter()
                .map(|&i| {
                    let v = values.get(i).cloned().unwrap_or(Value::Null);
                    collated_key_cell(&v, i, schema)
                })
                .collect()
        };
        let mut seen: hashbrown::HashSet<String> =
            hashbrown::HashSet::with_capacity(table.rows().len() + rows.len());
        for prow in table.rows() {
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
                let kind = if uc.is_primary_key {
                    "PRIMARY KEY"
                } else {
                    "UNIQUE"
                };
                let col_names: Vec<String> = uc
                    .columns
                    .iter()
                    .map(|&i| table.schema().columns[i].name.clone())
                    .collect();
                return Err(EngineError::Unsupported(alloc::format!(
                    "{kind} violation on {child_table:?} columns {col_names:?}: \
                     row #{batch_idx} duplicates an existing key"
                )));
            }
        }
    }
    Ok(())
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
) -> spg_storage::Value {
    match (v, schema.columns.get(column_position).map(|c| c.collation)) {
        (spg_storage::Value::Text(s), Some(spg_storage::Collation::CaseInsensitive)) => {
            spg_storage::Value::Text(s.to_ascii_lowercase())
        }
        _ => v.clone(),
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
    rows: &[spg_storage::Row],
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
    let mut seen: alloc::vec::Vec<alloc::vec::Vec<spg_storage::Value>> = alloc::vec::Vec::new();
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
        let key: alloc::vec::Vec<spg_storage::Value> = key_positions
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
        if key.iter().any(|v| matches!(v, spg_storage::Value::Null)) {
            continue;
        }
        if seen.iter().any(|other| *other == key) {
            return Err(EngineError::Unsupported(alloc::format!(
                "CREATE UNIQUE INDEX {:?}: existing rows already violate the constraint",
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
    rows: &[alloc::vec::Vec<spg_storage::Value>],
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
        let key_positions = unique_key_positions(idx);
        let key_of = |values: &[spg_storage::Value]| -> alloc::vec::Vec<spg_storage::Value> {
            key_positions
                .iter()
                .map(|&p| {
                    let v = values.get(p).cloned().unwrap_or(spg_storage::Value::Null);
                    collated_key_cell(&v, p, schema)
                })
                .collect()
        };
        let participates = |values: &[spg_storage::Value]| -> Result<bool, EngineError> {
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
        // v7.29 (mailrs round-23b) — set-based: one O(table) pass
        // (predicate evaluated once per existing row instead of once
        // per row PAIR), then probe per batch row. The previous
        // nested scans made bulk import O(n²).
        let mut seen: hashbrown::HashSet<String> =
            hashbrown::HashSet::with_capacity(table.rows().len() + rows.len());
        for prow in table.rows() {
            if !participates(&prow.values)? {
                continue;
            }
            let key = key_of(&prow.values);
            if key.iter().any(|v| matches!(v, spg_storage::Value::Null)) {
                continue;
            }
            seen.insert(aggregate::encode_key(&key));
        }
        for (batch_idx, row_values) in rows.iter().enumerate() {
            if !participates(row_values)? {
                continue;
            }
            let key = key_of(row_values);
            if key.iter().any(|v| matches!(v, spg_storage::Value::Null)) {
                continue;
            }
            if !seen.insert(aggregate::encode_key(&key)) {
                return Err(EngineError::Unsupported(alloc::format!(
                    "UNIQUE INDEX {:?} violation on {table_name:?}: \
                     row #{batch_idx} duplicates an existing key",
                    idx.name
                )));
            }
        }
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
    old_row: &Row,
    new_row: &Row,
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

/// v7.13.0 — evaluate every CHECK predicate on the schema against
/// each candidate row. Mirrors PG semantics: a `false` result
/// rejects the mutation; a NULL result *passes* (CHECK rejects
/// only on definite-false, not on unknown). mailrs round-5 G3.
pub(crate) fn enforce_check_constraints(
    catalog: &Catalog,
    table_name: &str,
    rows: &[alloc::vec::Vec<spg_storage::Value>],
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
        let expr = spg_sql::parser::parse_expression(src).map_err(|e| {
            EngineError::Unsupported(alloc::format!(
                "CHECK constraint #{i} on {table_name:?} ({src:?}) failed to re-parse: {e:?}"
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
                return Err(EngineError::Unsupported(alloc::format!(
                    "CHECK constraint violation on {table_name:?} (row #{batch_idx}): {:?}",
                    schema.checks[*i]
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
fn iter_cold_rows_of_parent(catalog: &Catalog, parent: &spg_storage::Table) -> Vec<Row> {
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

pub(crate) fn enforce_fk_inserts(
    catalog: &Catalog,
    child_table: &str,
    fks: &[spg_storage::ForeignKeyConstraint],
    rows: &[Vec<Value>],
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
        let cold_parent_rows: alloc::vec::Vec<Row> = if fk.local_columns.len() == 1 {
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
                        && !idx.lookup_eq(&key).is_empty()
                });
                // v7.6.7 self-ref widening: also accept a match
                // against earlier rows in this same batch when the
                // FK points at the table being inserted into.
                let present_in_batch = parent_is_self
                    && rows[..batch_idx]
                        .iter()
                        .any(|earlier| earlier.get(parent_col) == Some(v));
                if !(present_committed || present_in_batch) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "FOREIGN KEY violation: no parent row in {:?} where {} = {:?}",
                        fk.parent_table,
                        parent
                            .schema()
                            .columns
                            .get(parent_col)
                            .map_or("?", |c| c.name.as_str()),
                        v,
                    )));
                }
            } else {
                // Composite FK: scan parent rows. v7.6.7 also
                // accepts a match against earlier rows in the same
                // batch (self-ref bulk-loading of hierarchies).
                if fk
                    .local_columns
                    .iter()
                    .all(|&i| matches!(row_values.get(i), Some(Value::Null)))
                {
                    continue;
                }
                let local: Vec<&Value> = fk.local_columns.iter().map(|&i| &row_values[i]).collect();
                let matches_parent_row = |prow: &Row| {
                    fk.parent_columns
                        .iter()
                        .enumerate()
                        .all(|(i, &pi)| prow.values.get(pi) == Some(local[i]))
                };
                let parent_match_committed = parent.rows().iter().any(&matches_parent_row)
                    || cold_parent_rows.iter().any(&matches_parent_row);
                let parent_match_in_batch = parent_is_self
                    && rows[..batch_idx].iter().any(|earlier| {
                        fk.parent_columns
                            .iter()
                            .enumerate()
                            .all(|(i, &pi)| earlier.get(pi) == Some(local[i]))
                    });
                if !(parent_match_committed || parent_match_in_batch) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "FOREIGN KEY violation: no parent row in {:?} matching composite key",
                        fk.parent_table,
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
        defaults: Vec<Value>,
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
pub(crate) fn plan_fk_parent_deletions(
    catalog: &Catalog,
    parent_table_name: &str,
    to_delete_positions: &[usize],
    to_delete_rows: &[Vec<Value>],
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
    let mut work: Vec<(String, Vec<Value>)> = to_delete_rows
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
                            return Err(EngineError::Unsupported(alloc::format!(
                                "FOREIGN KEY violation: DELETE on {cur_parent:?} is \
                                 restricted by FK from {child_name:?}.{:?}",
                                fk.local_columns,
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
    plan_with_old: &[(usize, Vec<Value>, Vec<Value>)],
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
                            return Err(EngineError::Unsupported(alloc::format!(
                                "FOREIGN KEY violation: UPDATE on {parent_table_name:?} PK is \
                                 restricted by FK from {child_name:?}.{:?}",
                                fk.local_columns,
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
    mut value_for: impl FnMut(usize) -> Value,
) -> Result<(), EngineError> {
    use alloc::collections::BTreeMap;
    let mut by_row: BTreeMap<usize, Vec<(usize, Value)>> = BTreeMap::new();
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
