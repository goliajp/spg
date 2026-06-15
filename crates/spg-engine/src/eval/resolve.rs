//! Column resolution + comparison borrow-channel split out of `eval.rs`
//! (cut 35): everything `eval_expr` needs to turn a `ColumnName` into a
//! schema position / value and to run the borrowed comparison fast path.
//! Covers `resolve_column` / `resolve_column_borrowed` / `find_column_pos`
//! / `composite_eq` / `text_prefix_chars`, the collation lookup
//! (`column_collation` / `collation_fold_for_compare`), and the P4 borrow
//! channel (`eval_expr_cow` / `is_owned_compare_value` /
//! `compare_is_case_insensitive`). These sit on the interpreter hot path
//! and lean on `eval_expr` itself plus `apply_binary` / `compare`; the
//! glob `use super::*` keeps that core-facing surface (and the shared
//! types) reachable without enumerating it.

use super::*;

/// v7.17.0 Phase 2.5 — look up the collation of a column reference
/// in the current evaluation context. Returns `None` when the
/// expression is not a column reference (e.g. literal / function
/// call) or the column can't be resolved (caller falls back to
/// `Collation::Binary` semantics).
pub(crate) fn column_collation(e: &Expr, ctx: &EvalContext<'_>) -> Option<spg_storage::Collation> {
    let Expr::Column(c) = e else {
        return None;
    };
    // v7.31 (perf 3e) — zero-allocation segment matching (the
    // composite_eq pattern). This runs once per comparison eval —
    // 24k × per-row format! calls showed up as an allocator line
    // item in the inbox profile for a value that never changes
    // across rows.
    let matches_composite = |s: &str| {
        c.qualifier.as_deref().is_some_and(|q| {
            s.len() == q.len() + 1 + c.name.len()
                && s.as_bytes()[q.len()] == b'.'
                && s.starts_with(q)
                && s.ends_with(c.name.as_str())
        })
    };
    if c.qualifier.is_some()
        && let Some(s) = ctx.columns.iter().find(|s| matches_composite(&s.name))
    {
        return Some(s.collation);
    }
    if let Some(s) = ctx.columns.iter().find(|s| s.name == c.name) {
        return Some(s.collation);
    }
    // Bare-name fallback for joined schemas (same shape as
    // resolve_column): match a single composite ending in
    // ".<name>".
    let ends_with_dot_name = |s: &str| {
        // usize: `len > name.len()` ≡ `len >= name.len() + 1`
        // (rust 1.96 clippy::int_plus_one sweep).
        s.len() > c.name.len()
            && s.ends_with(c.name.as_str())
            && s.as_bytes()[s.len() - c.name.len() - 1] == b'.'
    };
    let mut matches = ctx.columns.iter().filter(|s| ends_with_dot_name(&s.name));
    let first = matches.next();
    let extra = matches.next();
    match (first, extra) {
        (Some(s), None) => Some(s.collation),
        _ => None,
    }
}

/// v7.17.0 Phase 2.5 — if the comparison op is text-equality and
/// either operand references a CaseInsensitive column, return
/// ASCII-folded copies of both Text values; otherwise pass
/// through. Only Eq / NotEq / Lt / LtEq / Gt / GtEq trigger the
/// fold — relational operators on text still honour collation
/// the same way (PG semantics). Non-Text values pass through.
pub(super) fn collation_fold_for_compare(
    op: BinOp,
    lhs: &Expr,
    rhs: &Expr,
    l: Value,
    r: Value,
    ctx: &EvalContext<'_>,
) -> (Value, Value) {
    if !matches!(
        op,
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
    ) {
        return (l, r);
    }
    let lhs_col = column_collation(lhs, ctx);
    let rhs_col = column_collation(rhs, ctx);
    let ci = matches!(lhs_col, Some(spg_storage::Collation::CaseInsensitive))
        || matches!(rhs_col, Some(spg_storage::Collation::CaseInsensitive));
    if !ci {
        return (l, r);
    }
    let fold = |v: Value| match v {
        Value::Text(s) => Value::Text(s.to_ascii_lowercase()),
        other => other,
    };
    (fold(l), fold(r))
}

/// v7.32 (P4 borrow channel) — borrowed-or-owned evaluation. A bare
/// column read borrows its cell (no clone); literals and computed
/// sub-expressions stay owned. Used by the comparison fast path in
/// `eval_expr` so a predicate like `col != ''` reads the cell by
/// reference instead of cloning it per scanned row. Semantically
/// identical to `eval_expr` — a borrowed cell compares equal to its
/// clone — and the fallback to owned `resolve_column` preserves the
/// detailed not-found / unknown-qualifier errors.
pub(super) fn eval_expr_cow<'r>(
    expr: &Expr,
    row: &'r Row,
    ctx: &EvalContext<'_>,
) -> Result<Cow<'r, Value>, EvalError> {
    match expr {
        Expr::Column(c) => match resolve_column_borrowed(c, row, ctx)? {
            Some(v) => Ok(Cow::Borrowed(v)),
            None => resolve_column(c, row, ctx).map(Cow::Owned),
        },
        _ => eval_expr(expr, row, ctx).map(Cow::Owned),
    }
}

/// v7.32 (P4 borrow channel) — operands whose comparison `apply_binary`
/// does NOT route through the plain ref-based `compare`: NUMERIC goes
/// through fixed-point `apply_binary_numeric` and INTERVAL through
/// `apply_binary_interval`. The borrowed comparison fast path falls
/// back to the owned path for these so their semantics are untouched.
#[inline]
pub(super) fn is_owned_compare_value(v: &Value) -> bool {
    matches!(v, Value::Numeric { .. } | Value::Interval { .. })
}

/// v7.32 (P4 borrow channel) — does a comparison need case-insensitive
/// folding? Mirrors the trigger in `collation_fold_for_compare`; when
/// true the fast path defers to the owned path so the fold still runs.
#[inline]
pub(super) fn compare_is_case_insensitive(lhs: &Expr, rhs: &Expr, ctx: &EvalContext<'_>) -> bool {
    matches!(
        column_collation(lhs, ctx),
        Some(spg_storage::Collation::CaseInsensitive)
    ) || matches!(
        column_collation(rhs, ctx),
        Some(spg_storage::Collation::CaseInsensitive)
    )
}

/// v7.29 - borrow a column cell without cloning (the prefix fast
/// path for LEFT). Mirrors resolve_column's lookup; returns Ok(None)
/// when the reference can't be attributed (caller falls back to the
/// generic owned path, which will surface the proper error).
/// v7.30 (perf campaign) - zero-allocation composite-name match:
/// does `schema_name` equal `qualifier ++ '.' ++ name`? The old path
/// FORMATTED a fresh String per column reference per row (~290k
/// format+compare pairs per 24k-row aggregate query) - the single
/// hottest residue on the inbox profile.
#[inline]
pub(super) fn composite_eq(schema_name: &str, qualifier: &str, name: &str) -> bool {
    schema_name.len() == qualifier.len() + 1 + name.len()
        && schema_name.as_bytes()[qualifier.len()] == b'.'
        && schema_name[..qualifier.len()] == *qualifier
        && schema_name[qualifier.len() + 1..] == *name
}

/// v7.30 (perf campaign) - position-only resolution for bind-once
/// fast paths (aggregate row loop). Same lookup order as
/// resolve_column's happy paths: composite "alias.col", then the
/// bare name.
pub(crate) fn find_column_pos(c: &ColumnName, ctx: &EvalContext<'_>) -> Option<usize> {
    if let Some(q) = &c.qualifier {
        if let Some(pos) = ctx
            .columns
            .iter()
            .position(|s| composite_eq(&s.name, q, &c.name))
        {
            return Some(pos);
        }
    }
    ctx.columns.iter().position(|s| s.name == c.name)
}

pub(super) fn resolve_column_borrowed<'r>(
    c: &ColumnName,
    row: &'r Row,
    ctx: &EvalContext<'_>,
) -> Result<Option<&'r Value>, EvalError> {
    if let Some(q) = &c.qualifier {
        if let Some(pos) = ctx
            .columns
            .iter()
            .position(|s| composite_eq(&s.name, q, &c.name))
        {
            return Ok(row.values.get(pos));
        }
    }
    if let Some(pos) = ctx.columns.iter().position(|s| s.name == c.name) {
        return Ok(row.values.get(pos));
    }
    Ok(None)
}

/// First `n` CHARACTERS of `t` (PG LEFT semantics; negative n means
/// all but the last |n|), cloning only the prefix bytes.
pub(super) fn text_prefix_chars(t: &str, n: i64) -> String {
    if n >= 0 {
        let n = usize::try_from(n).unwrap_or(usize::MAX);
        match t.char_indices().nth(n) {
            Some((byte_idx, _)) => t[..byte_idx].into(),
            None => t.into(),
        }
    } else {
        let drop_tail = usize::try_from(-n).unwrap_or(usize::MAX);
        let total = t.chars().count();
        let keep = total.saturating_sub(drop_tail);
        match t.char_indices().nth(keep) {
            Some((byte_idx, _)) => t[..byte_idx].into(),
            None => t.into(),
        }
    }
}

pub(super) fn resolve_column(
    c: &ColumnName,
    row: &Row,
    ctx: &EvalContext<'_>,
) -> Result<Value, EvalError> {
    if let Some(q) = &c.qualifier {
        // Multi-table evaluation (joins): the synthesised schema uses
        // composite column names "alias.column" so we look that up
        // directly. Falls back to the single-table case below if the
        // composite isn't present.
        // v7.30 - zero-alloc composite match (was a String format
        // per column reference per row).
        if let Some(pos) = ctx
            .columns
            .iter()
            .position(|s| composite_eq(&s.name, q, &c.name))
        {
            return Ok(row.values[pos].clone());
        }
        // v7.26 (round-20 B) — when the qualifier IS a known table
        // alias in a joined schema (composite "alias.x" columns
        // exist) but THIS column isn't among them, the honest error
        // is "column does not exist", not "unknown table
        // qualifier". The misleading message sent mailrs hunting a
        // resolver bug when their fixture was missing a column.
        let prefix = alloc::format!("{q}.");
        if ctx.columns.iter().any(|sc| sc.name.starts_with(&prefix)) {
            return Err(EvalError::ColumnNotFound {
                name: alloc::format!("{q}.{name}", name = c.name),
            });
        }
        let expected = ctx.table_alias.ok_or_else(|| EvalError::UnknownQualifier {
            qualifier: q.clone(),
        })?;
        if q != expected {
            return Err(EvalError::UnknownQualifier {
                qualifier: q.clone(),
            });
        }
    }
    if let Some(pos) = ctx.columns.iter().position(|s| s.name == c.name) {
        return Ok(row.values[pos].clone());
    }
    // Bare-name fallback for joined schemas: match any single composite
    // column ending in ".<name>"; ambiguity is an error.
    let suffix = alloc::format!(".{name}", name = c.name);
    let mut matches = ctx
        .columns
        .iter()
        .enumerate()
        .filter(|(_, s)| s.name.ends_with(&suffix));
    let first = matches.next();
    let extra = matches.next();
    match (first, extra) {
        (Some((pos, _)), None) => Ok(row.values[pos].clone()),
        (Some(_), Some(_)) => Err(EvalError::TypeMismatch {
            detail: alloc::format!("ambiguous column reference: {}", c.name),
        }),
        _ => Err(EvalError::ColumnNotFound {
            name: c.name.clone(),
        }),
    }
}
