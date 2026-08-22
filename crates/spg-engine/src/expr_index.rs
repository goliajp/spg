//! Keys for indexes that key on an expression.
//!
//! `CREATE INDEX ON t (lower(s))` names a key that is not in the row. The
//! storage crate has no expression evaluator, so it cannot derive one;
//! before v7.38.16 it filled the index with the leading column's values
//! instead — keys nothing could match, which is why every lookup path
//! guarded itself with `expression.is_none()`. Measured, that cost 1.93x a
//! plain insert to maintain and returned nothing: on a 20,000-row table an
//! insert ran 5.20 us against 2.70 us with no index at all, and the same
//! SELECT ran within one percent of itself with the index and without it.
//!
//! This module is the missing half. It parses each index's stored
//! expression once per statement, evaluates it per row, and hands the
//! keys to storage, which owns the invariant that a map and its
//! completeness flag move together.

use crate::EngineError;
use crate::eval;
use alloc::string::String;
use alloc::vec::Vec;
use spg_storage::{Table, Value};

/// The parsed expression of every B-tree expression index on a table, one
/// slot per entry of `table.indices()`, `None` where the index keys on a
/// column of its own.
pub(crate) struct ExprKeyPlan {
    exprs: Vec<Option<spg_sql::ast::Expr>>,
    /// v7.38.18 (S0) — slot-parallel too: the collation an index keys
    /// under when it keys on a column that collates by a locale. The two
    /// are exclusive by construction (`Table::index_collation` answers
    /// `None` for an index that has an expression).
    collations: Vec<Option<alloc::string::String>>,
    /// Slot-parallel key column position, for the collated slots.
    key_columns: Vec<usize>,
}

impl ExprKeyPlan {
    /// `None` when the table has no expression index — the ordinary case,
    /// and the one that must stay free.
    pub(crate) fn for_table(table: &Table) -> Result<Option<Self>, EngineError> {
        // v7.38.18 (S0) — a locale-collated column index takes a supplied
        // key too, so the plan has to cover it or its slot arrives empty
        // and the index retires on the first insert.
        let collations: Vec<Option<alloc::string::String>> = table
            .indices()
            .iter()
            .map(|i| table.index_collation(i).map(alloc::string::String::from))
            .collect();
        if !table.indices().iter().any(|i| i.expression.is_some())
            && collations.iter().all(Option::is_none)
        {
            return Ok(None);
        }
        let mut exprs = Vec::with_capacity(table.indices().len());
        for idx in table.indices() {
            let parsed = match &idx.expression {
                Some(src) => Some(spg_sql::parser::parse_expression(src).map_err(|e| {
                    EngineError::Unsupported(alloc::format!(
                        "index {:?} expression {src:?} failed to re-parse: {e:?}",
                        idx.name
                    ))
                })?),
                None => None,
            };
            exprs.push(parsed);
        }
        let key_columns: Vec<usize> = table.indices().iter().map(|i| i.column_position).collect();
        Ok(Some(Self {
            exprs,
            collations,
            key_columns,
        }))
    }

    /// The expression values for one row, slot-parallel to
    /// `table.indices()`.
    ///
    /// A slot is `None` when the index keys on a column, and also when
    /// the expression evaluated to NULL — a NULL enters no entry, which
    /// is what a NULL column value does too.
    pub(crate) fn keys_for(
        &self,
        values: &[Value<'static>],
        ctx: &eval::EvalContext<'_>,
    ) -> Result<Vec<Option<Value<'static>>>, EngineError> {
        let row = spg_storage::Row {
            values: values.to_vec(),
        };
        let mut out = Vec::with_capacity(self.exprs.len());
        for (slot, expr) in self.exprs.iter().enumerate() {
            // v7.38.18 (S0) — the collated slot: this column's value
            // encoded as an ICU sort key, which is what makes the
            // byte-ordered B-tree order it by the locale.
            if let Some(Some(coll)) = self.collations.get(slot) {
                let pos = self.key_columns[slot];
                out.push(collated_key(coll, values.get(pos)));
                continue;
            }
            out.push(match expr {
                Some(e) => {
                    let v = eval::eval_expr(e, &row, ctx)
                        .map_err(|e| EngineError::Unsupported(alloc::format!("{e:?}")))?;
                    (!v.is_null()).then_some(v)
                }
                None => None,
            });
        }
        Ok(out)
    }
}

/// v7.38.18 (S0) — one collated index key: the ICU sort key of a text
/// value under `coll`, as `Value::Bytes`, which storage turns into
/// `IndexKey::Bytes` and its B-tree then orders by the locale.
///
/// `None` for NULL (a NULL enters no entry), for a non-text value, and
/// for a collation this build cannot perform — the last of which retires
/// the index rather than filling it with keys of the wrong shape, which
/// is the same choice the expression path makes.
fn collated_key(coll: &str, v: Option<&Value<'static>>) -> Option<Value<'static>> {
    let text = match v? {
        Value::Text(t) => t.as_ref(),
        Value::BpChar(t) => t.as_ref(),
        _ => return None,
    };
    crate::collate::sort_key(coll, text).map(|k| Value::Bytes(alloc::borrow::Cow::Owned(k)))
}

/// Bring every unusable expression index on `table` back into service by
/// evaluating its expression over every stored row.
///
/// Called where a statement is about to depend on one. Cheap to call when
/// there is nothing to do: the common table reports no stale index and
/// this returns without touching a row.
pub(crate) fn refresh(table: &mut Table) -> Result<(), EngineError> {
    // v7.38.18 (S0) — the collated column indexes first: they are the
    // same mechanism, and `add_index` deliberately leaves theirs empty
    // because this crate is the only one that can encode their keys.
    for (name, pos, coll) in table.stale_collated_indices() {
        let mut keys: Vec<Option<Value<'static>>> = Vec::with_capacity(table.stored_row_count());
        for i in 0..table.stored_row_count() {
            let Some(values) = table.row_values_at(i) else {
                return Ok(());
            };
            keys.push(collated_key(&coll, values.get(pos)));
        }
        table
            .rebuild_expression_index(&name, &keys)
            .map_err(EngineError::Storage)?;
    }
    let stale = table.stale_expression_indices();
    if stale.is_empty() {
        return Ok(());
    }
    let schema = table.schema().clone();
    let ctx = eval::EvalContext::new(&schema.columns, None);
    for (name, src) in stale {
        let expr = spg_sql::parser::parse_expression(&src).map_err(|e| {
            EngineError::Unsupported(alloc::format!(
                "index {name:?} expression {src:?} failed to re-parse: {e:?}"
            ))
        })?;
        let mut keys: Vec<Option<Value<'static>>> = Vec::with_capacity(table.stored_row_count());
        for i in 0..table.stored_row_count() {
            let Some(values) = table.row_values_at(i) else {
                return Ok(());
            };
            let row = spg_storage::Row {
                values: values.to_vec(),
            };
            let v = eval::eval_expr(&expr, &row, &ctx)
                .map_err(|e| EngineError::Unsupported(alloc::format!("{e:?}")))?;
            keys.push((!v.is_null()).then_some(v));
        }
        table
            .rebuild_expression_index(&name, &keys)
            .map_err(EngineError::Storage)?;
    }
    Ok(())
}

/// The name of a usable index on `table` whose key is exactly `expr`, if
/// one exists.
///
/// Matching is on the canonical Display form, which is what `CREATE INDEX`
/// stored, so `lower(s)` in a WHERE clause finds an index written
/// `LOWER(s)` and one written `lower( s )`.
pub(crate) fn index_for_expression(table: &Table, expr: &spg_sql::ast::Expr) -> Option<String> {
    let wanted = alloc::format!("{expr}");
    table
        .indices()
        .iter()
        .find(|i| {
            i.expression.as_deref() == Some(wanted.as_str())
                && table.expr_index_is_complete(&i.name)
        })
        .map(|i| i.name.clone())
}

/// Bring back into service every expression index on `name`, if the
/// relation exists and has one.
///
/// Called at the end of a write statement that could not maintain them
/// incrementally: UPDATE and DELETE rewrite `rows` wholesale, which moves
/// the positions the index stores, so storage marks them unusable and the
/// keys have to be evaluated again. Without this the first UPDATE on a
/// table would retire its expression index permanently — a cliff no
/// EXPLAIN would show, since the answers stay right either way.
pub(crate) fn refresh_named(
    engine_catalog: &mut spg_storage::Catalog,
    name: &str,
) -> Result<(), EngineError> {
    let Some(table) = engine_catalog.get_mut(name) else {
        return Ok(());
    };
    // v7.38.18 (S0) — a stale COLLATED index counts as stale too, or it
    // stays empty after the UPDATE that retired it and every seek
    // against it answers no rows.
    if table.stale_expression_indices().is_empty() && table.stale_collated_indices().is_empty() {
        return Ok(());
    }
    refresh(table)
}

/// Refill every expression index in a catalog just read off disk.
///
/// The completeness flag is not persisted — deliberately, because a
/// catalog written by any earlier version holds the WRONG keys under an
/// expression index and must not be allowed to answer with them. So a
/// restored table starts unusable, and this is where it stops being
/// unusable: without it an expression index would sit inert from restart
/// until the table's next write, which is a cliff nothing would report.
///
/// A table whose expression fails to parse or to evaluate is left alone
/// rather than failing the restore: an unusable index costs a scan, and a
/// database that will not open costs everything.
pub(crate) fn rebuild_all(cat: &mut spg_storage::Catalog) {
    let names: Vec<alloc::string::String> = cat
        .table_names()
        .into_iter()
        .filter(|n| {
            cat.get(n).is_some_and(|t| {
                // v7.38.18 (S0) — the collated column indexes rebuild at
                // open for the same reason the expression ones do: the
                // completeness flag is not persisted, deliberately,
                // because a catalog written by any earlier version holds
                // keys of the wrong kind under this name.
                !t.stale_expression_indices().is_empty() || !t.stale_collated_indices().is_empty()
            })
        })
        .collect();
    for name in names {
        if let Some(table) = cat.get_mut(&name) {
            let _ = refresh(table);
        }
    }
}
