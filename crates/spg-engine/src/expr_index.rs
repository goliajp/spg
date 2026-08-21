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
}

impl ExprKeyPlan {
    /// `None` when the table has no expression index — the ordinary case,
    /// and the one that must stay free.
    pub(crate) fn for_table(table: &Table) -> Result<Option<Self>, EngineError> {
        if !table.indices().iter().any(|i| i.expression.is_some()) {
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
        Ok(Some(Self { exprs }))
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
        for expr in &self.exprs {
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

/// Bring every unusable expression index on `table` back into service by
/// evaluating its expression over every stored row.
///
/// Called where a statement is about to depend on one. Cheap to call when
/// there is nothing to do: the common table reports no stale index and
/// this returns without touching a row.
pub(crate) fn refresh(table: &mut Table) -> Result<(), EngineError> {
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
    if table.stale_expression_indices().is_empty() {
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
            cat.get(n)
                .is_some_and(|t| !t.stale_expression_indices().is_empty())
        })
        .collect();
    for name in names {
        if let Some(table) = cat.get_mut(&name) {
            let _ = refresh(table);
        }
    }
}
