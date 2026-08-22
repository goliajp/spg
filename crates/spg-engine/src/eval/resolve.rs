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

/// The declared collation NAME of a column reference, resolved the same
/// way [`column_collation`] resolves the enum.
///
/// v7.38.18 — the enum answers whether a column FOLDS; only the name
/// answers whether it PADS, and the two are independent:
/// `utf8mb4_bin` is byte-wise AND `PAD SPACE`, which is the
/// combination easiest to get backwards.
pub(crate) fn column_collation_name(e: &Expr, ctx: &EvalContext<'_>) -> Option<String> {
    let Expr::Column(c) = e else {
        return None;
    };
    let ends_with_dot_name = |s: &str| {
        s.len() > c.name.len()
            && s.ends_with(c.name.as_str())
            && s.as_bytes()[s.len() - c.name.len() - 1] == b'.'
    };
    ctx.columns
        .iter()
        .find(|s| s.name == c.name || ends_with_dot_name(&s.name))
        .and_then(|s| s.collation_name.clone())
}

/// How this comparison's collation behaves — fold and pad, decided
/// together and once.
///
/// v7.38.18 — the comparison has ONE collation, taken from whichever
/// side declares one (a literal declares none and takes the column's,
/// which is what makes `WHERE bin_col = 'alpha'` match a row holding
/// `'alpha  '` on both engines). `BINARY x` and an explicitly byte-wise
/// column suppress the case fold but NOT the padding rule, because
/// those are different questions about the same collation.
pub(crate) fn text_compare_of(
    lhs: &Expr,
    rhs: &Expr,
    ctx: &EvalContext<'_>,
) -> crate::collate::TextCompare {
    let byte_wise = is_binary_coerced(lhs)
        || is_binary_coerced(rhs)
        || operand_is_binary_column(lhs, ctx)
        || operand_is_binary_column(rhs, ctx);
    let ci = matches!(
        column_collation(lhs, ctx),
        Some(spg_storage::Collation::CaseInsensitive)
    ) || matches!(
        column_collation(rhs, ctx),
        Some(spg_storage::Collation::CaseInsensitive)
    );
    let name = column_collation_name(lhs, ctx).or_else(|| column_collation_name(rhs, ctx));
    crate::collate::TextCompare {
        fold_case: !byte_wise && (ci || ctx.mysql_dialect),
        pads: crate::collate::pads_space(name.as_deref()),
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
    l: Value<'static>,
    r: Value<'static>,
    ctx: &EvalContext<'_>,
) -> (Value<'static>, Value<'static>) {
    // v7.39 (round 390, epic P5) — a MySQL SET column reads as its bitmask
    // under an arith / bitwise op (`s + 0` is 5, `WHERE s & flag` filters by
    // membership); SPG stores SET as text, so the numeric path saw `'a,c'`
    // and coerced it to 0. Fold it here, reusing this already-out-of-line
    // call site so `eval_expr`'s maxed recursion frame gains nothing (the
    // round-390 frame cliff). Comparison ops fall through — `s = 'a,c'`
    // stays a text compare.
    if ctx.mysql_dialect && super::is_mysql_numeric_binop(op) {
        // v7.39 (round 402) — an inline ENUM column reads as its 1-based
        // ordinal in the same numeric context (`e + 0` is 1 for the first
        // member), like the SET bitmask above.
        let fold_set = |expr: &Expr, v: Value<'static>| -> Value<'static> {
            match &v {
                Value::Text(s) => {
                    if let Some(variants) = super::expr_set_variants(expr, ctx.columns) {
                        Value::BigInt(super::set_text_to_bitmask(s, variants))
                    } else if let Some(variants) =
                        super::expr_inline_enum_variants(expr, ctx.columns)
                    {
                        Value::BigInt(super::enum_text_to_ordinal(s, variants))
                    } else {
                        v
                    }
                }
                _ => v,
            }
        };
        return (fold_set(lhs, l), fold_set(rhs, r));
    }
    if !matches!(
        op,
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
    ) {
        return (l, r);
    }
    // v7.39 (round 364, M4 P2) — a MySQL session's default collation is
    // accent- AND case-insensitive, so EVERY text comparison folds, not
    // only those touching a `COLLATE case_insensitive` column. `BINARY x`
    // (or `CAST(x AS BINARY)`) still forces byte-wise, which is why the
    // dialect fold is suppressed when either side is binary-coerced.
    // v7.39 (round 370, M4 P4a) — an explicit `COLLATE utf8mb4_bin` column
    // (stored `Binary`) is byte-wise even under the dialect.
    // v7.38.18 — one resolver for both bits; the compiled path uses the
    // same one, so the two cannot drift.
    let tc = text_compare_of(lhs, rhs, ctx);
    if tc.is_plain_bytes() {
        return (l, r);
    }
    let fold_one = |v: Value<'static>| -> Value<'static> {
        // A CHAR's padding belongs to the TYPE — both engines ignore it
        // whatever the collation says. A TEXT's belongs to the
        // collation, which `tc.pads` carries.
        let (text, pad) = match v {
            Value::BpChar(s) => (s.into_owned(), true),
            Value::Text(s) => (s.into_owned(), tc.pads),
            other => return other,
        };
        let base = if pad {
            text.trim_end_matches(' ')
        } else {
            text.as_str()
        };
        Value::text(if tc.fold_case {
            if ctx.mysql_dialect {
                spg_storage::mysql_ci_fold(base)
            } else {
                base.to_ascii_lowercase()
            }
        } else {
            alloc::string::ToString::to_string(base)
        })
    };
    (fold_one(l), fold_one(r))
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
    row: &'r Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Cow<'r, Value<'static>>, EvalError> {
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
    // v7.39 (round 355, M13) — MySQL's `BINARY x` forces the binary
    // collation, so a comparison touching one is byte-wise even when the
    // other side is a CI column. Measured on MariaDB 11: `'a' = 'A'` is 1
    // under the default collation and `BINARY 'a' = 'A'` is 0.
    if is_binary_coerced(lhs) || is_binary_coerced(rhs) {
        return false;
    }
    // v7.39 (round 364, M4 P2) — a MySQL session folds every text
    // comparison (see `collation_fold_for_compare`), so it must take the
    // owned path where the fold runs. Non-text operands fold to
    // themselves, so this only costs the mysql dialect the owned route.
    // v7.39 (round 370, M4 P4a) — EXCEPT when an operand is a column with
    // an explicit `COLLATE utf8mb4_bin` (stored `Collation::Binary`): that
    // column is byte-wise, so the comparison does not fold. A folding
    // default column stores `CaseInsensitive`, so only the explicit binary
    // column reaches here as `Binary`.
    if ctx.mysql_dialect {
        return !operand_is_binary_column(lhs, ctx) && !operand_is_binary_column(rhs, ctx);
    }
    matches!(
        column_collation(lhs, ctx),
        Some(spg_storage::Collation::CaseInsensitive)
    ) || matches!(
        column_collation(rhs, ctx),
        Some(spg_storage::Collation::CaseInsensitive)
    )
}

/// v7.39 (round 364, M4 P2) — does a MySQL session's default-collation
/// fold apply to this comparison? True on the MySQL dialect unless either
/// side is `BINARY`-coerced (which forces byte-wise). Shared by the
/// interpreter and the compiled stepper so they cannot disagree.
pub(super) fn mysql_text_fold_applies(lhs: &Expr, rhs: &Expr, ctx: &EvalContext<'_>) -> bool {
    ctx.mysql_dialect
        && !is_binary_coerced(lhs)
        && !is_binary_coerced(rhs)
        // v7.39 (round 370, M4 P4a) — an explicit `COLLATE utf8mb4_bin`
        // column stays byte-wise even on the compiled comparison path.
        && !operand_is_binary_column(lhs, ctx)
        && !operand_is_binary_column(rhs, ctx)
}

/// Is this expression coerced to the binary collation — `BINARY x` or
/// `CAST(x AS BINARY[(n)])`?
/// v7.39 (round 370, M4 P4a) — is `e` a column REFERENCE whose stored
/// collation is the explicit byte-wise `Binary` (an explicit `COLLATE
/// utf8mb4_bin`)? A MySQL folding default column stores `CaseInsensitive`,
/// and a literal / expression has no column collation, so only a column
/// deliberately declared binary answers true — and it suppresses the
/// dialect's default fold.
pub(super) fn operand_is_binary_column(e: &Expr, ctx: &EvalContext<'_>) -> bool {
    if matches!(
        column_collation(e, ctx),
        Some(spg_storage::Collation::Binary)
    ) {
        return true;
    }
    // v7.38.14 — and through an EXPRESSION, not only a bare column.
    //
    // A byte-wise column stopped being byte-wise the moment anything
    // wrapped it: `GREATEST(s,'A') = 'A'` and `CONCAT(s,'') = 'A'` both
    // answered 2 on a `COLLATE utf8mb4_bin` column where MySQL 9.7.1
    // answers 1, because this test recognised a column reference and
    // nothing else, so an expression looked like a value with no
    // collation at all — and "no collation" reads here as "fold it".
    //
    // `collate_derive` already answers exactly this question and ORDER BY
    // has used it since round 692. Two sites deciding one question by
    // different rules is how the answer came to depend on whether the
    // column was wrapped.
    operand_derives_binary(e, ctx)
}

/// v7.38.14 — does this expression DERIVE a byte-wise collation?
///
/// `None` means the expression declares nothing, which keeps today's
/// behaviour (the dialect default folds). A derived name that this build
/// cannot perform is also left alone rather than guessed at.
fn operand_derives_binary(e: &Expr, ctx: &EvalContext<'_>) -> bool {
    // A bare column is already answered above; only expressions reach here,
    // and a literal derives nothing, so this is not paid on the common shape.
    if matches!(e, Expr::Column(_) | Expr::Literal(_)) {
        return false;
    }
    let resolve = |c: &spg_sql::ast::ColumnName| -> Option<alloc::string::String> {
        let pos = find_column_pos(c, ctx)?;
        ctx.columns.get(pos)?.collation_name.clone()
    };
    crate::collate_derive::derive(e, &resolve)
        .name()
        .is_some_and(crate::collate::is_byte_wise)
}

pub(crate) fn is_binary_coerced(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Cast {
            target: spg_sql::ast::CastTarget::Named(n),
            ..
        } if n.eq_ignore_ascii_case("binary") || n.to_ascii_lowercase().starts_with("binary(")
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
    if let Some(pos) = ctx.columns.iter().position(|s| s.name == c.name) {
        return Some(pos);
    }
    // v7.37 (round 823) — the bare-name fallback `resolve_column` has carried
    // since the joined schemas landed was MISSING here, so the two disagreed
    // on exactly one shape: an unqualified column in a joined/deferred
    // context, where the synthesised schema names columns "alias.column" and
    // the plain `s.name == c.name` above therefore never matches.
    //
    // The disagreement was not cosmetic. `try_exec_joined_streaming` binds its
    // projection through this function, so `SELECT pad FROM big b` — the
    // commonest projection there is — failed to bind, fell back to the
    // materialising path, and stopped honouring statement_timeout: measured at
    // 400000 rows / 0.81s with a 120ms timeout set, while `b.pad` over the same
    // table cancelled at ~65k rows in 0.14s.
    //
    // Same rule as `resolve_column`: match a single composite column ending in
    // ".<name>". Ambiguity returns None rather than picking one, which sends
    // the caller down the general path — that path raises the ambiguity error
    // PG raises. Zero-alloc suffix compare, like `composite_eq` next door,
    // because the bind-once callers are on hot-path setup.
    let suffix_at = |s: &str| s.len().checked_sub(c.name.len() + 1);
    let mut matches = ctx.columns.iter().enumerate().filter(|(_, s)| {
        suffix_at(&s.name)
            .is_some_and(|dot| s.name.as_bytes()[dot] == b'.' && s.name[dot + 1..] == *c.name)
    });
    match (matches.next(), matches.next()) {
        (Some((pos, _)), None) => Some(pos),
        _ => None,
    }
}

pub(super) fn resolve_column_borrowed<'r, 'a>(
    c: &ColumnName,
    row: &'r Row<'a>,
    ctx: &EvalContext<'_>,
) -> Result<Option<&'r Value<'a>>, EvalError> {
    // v7.39 (read01 round 56) — a COMPOSITE column cannot be served through the
    // borrow channel: it is stored as JSON and has to be REHYDRATED into a
    // `Value::Composite`, which produces a new value and so cannot be borrowed
    // out of the row. Returning None here makes `eval_expr_cow` fall back to
    // the owned `resolve_column`, which rehydrates.
    //
    // This was the last hole: the comparison fast path (v7.32's borrow channel)
    // reads its operands through here, so `WHERE p = ROW(2,'b')::pt` compared
    // the raw stored Json against a Composite and errored — while `(p).x`, which
    // is not a bare comparison operand, went through the owned path and worked.
    let is_composite = |pos: usize| {
        ctx.columns
            .get(pos)
            .is_some_and(|s| s.user_composite_type.is_some())
    };
    if let Some(q) = &c.qualifier {
        if let Some(pos) = ctx
            .columns
            .iter()
            .position(|s| composite_eq(&s.name, q, &c.name))
        {
            if is_composite(pos) {
                return Ok(None);
            }
            return Ok(row.values.get(pos));
        }
    }
    if let Some(pos) = ctx.columns.iter().position(|s| s.name == c.name) {
        if is_composite(pos) {
            return Ok(None);
        }
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

/// v7.37 (round 957) — where a column reference lands, without reading a
/// row. `Ok(Some(pos))` is a plain column at that position; `Ok(None)` is
/// the whole-row reference below, which has to build a composite from the
/// row and so cannot be reduced to a position.
///
/// This exists so that a caller wanting to resolve ONCE for a whole scan
/// (the projection binding in `try_stream_single_table`) runs the same
/// lookup order, the same fallbacks and the same errors as the per-row
/// path — `resolve_column` is now literally this function plus a fetch.
/// The alternative, a second resolver written to match, is exactly what
/// round 823 spent a day repairing: `find_column_pos` had been missing
/// `resolve_column`'s bare-name fallback, so one shape bound on one path
/// and not the other, and the difference was silent.
pub(crate) fn locate_column(
    c: &ColumnName,
    ctx: &EvalContext<'_>,
) -> Result<Option<usize>, EvalError> {
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
            return Ok(Some(pos));
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
        return Ok(Some(pos));
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
        (Some((pos, _)), None) => Ok(Some(pos)),
        (Some(_), Some(_)) => Err(EvalError::TypeMismatch {
            detail: alloc::format!("column reference \"{}\" is ambiguous", c.name),
        }),
        _ => {
            // v7.38 (read01, T9) — whole-row reference: a bare name equal to
            // the FROM alias (real table or subquery) with no matching column
            // resolves to the composite record of every column, exactly as PG
            // treats `row_to_json(e)` / `to_jsonb(e)` / a bare `SELECT e`.
            // Column resolution above wins, so a real column named like the
            // alias is unaffected.
            // The whole-row reference. Two schema shapes carry it: a
            // single-table / subquery / CTE scan, which knows its alias
            // and has bare column names, and a JOIN's combined schema,
            // which has no alias at all and qualifies every column
            // `alias.col` — there the alias is identified by the prefix,
            // which is what `whole_row_composite` already keys on to pick
            // the fields out. Only the first shape could reach it before,
            // so `SELECT wr FROM wr JOIN jb ON …` — `(7,z)` on PG18.4 —
            // raised here (round 961).
            if c.qualifier.is_none()
                && (ctx.table_alias == Some(c.name.as_str()) || {
                    let prefix = alloc::format!("{name}.", name = c.name);
                    ctx.columns.iter().any(|s| s.name.starts_with(&prefix))
                })
            {
                return Ok(None);
            }
            Err(EvalError::ColumnNotFound {
                name: c.name.clone(),
            })
        }
    }
}

/// The cell a located column holds, rehydrated to the column's declared
/// shape. Split out of `resolve_column` so a bind-once caller can keep
/// the position from `locate_column` and still fetch through exactly the
/// same rehydration (a stored composite arrives as JSON and has to be
/// rebuilt; reading `row.values[pos]` raw would hand back the JSON).
pub(crate) fn column_at(
    pos: usize,
    row: &Row<'_>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    rehydrate_cell(pos, row, ctx)
}

pub(super) fn resolve_column(
    c: &ColumnName,
    row: &Row<'_>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    match locate_column(c, ctx)? {
        Some(pos) => rehydrate_cell(pos, row, ctx),
        // `locate_column` only declines a name it has already checked is
        // the FROM alias, so this is the whole-row reference.
        None => whole_row_composite(row, ctx, &c.name),
    }
}

/// v7.38 (read01, T9) — build the whole-row `Value::Composite` for `alias`
/// from the current row. In a single-table / subquery scan the schema column
/// names are already bare, so every column becomes a field. In a joined
/// schema the columns are `alias.col` composites; keep only this alias's and
/// strip the prefix so the composite field names match PG's (the base column
/// names).
fn whole_row_composite(
    row: &Row<'_>,
    ctx: &EvalContext<'_>,
    alias: &str,
) -> Result<Value<'static>, EvalError> {
    let prefix = alloc::format!("{alias}.");
    let joined: Vec<(usize, &str)> = ctx
        .columns
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.name.strip_prefix(&prefix).map(|bare| (i, bare)))
        .collect();
    // v7.39 (read01 round 78) — a FROM item that calls a function returning a
    // BASE type has that scalar AS its row type, so a whole-row reference is the
    // value itself, not a one-field composite. `SELECT j FROM
    // jsonb_array_elements('[1,2]') AS j` is `1`, `2` in PG; SPG answered
    // `(1)`, `(2)`. A one-column TABLE or subquery does NOT collapse — hence the
    // marker, set only where the parser desugared a function item.
    if ctx.columns.len() == 1 && ctx.columns[0].scalar_row_source {
        return rehydrate_cell(0, row, ctx);
    }
    // v7.39 (round 487) — each field goes through `rehydrate_cell`, not a
    // raw cell read. A composite-typed COLUMN inside a whole-row reference
    // used to come back as the stored JSON: PG18 answers `SELECT t FROM t`
    // with `(1,10,x,"(1,one)")` and SPG answered
    // `(1,10,x,"{""a"":1,""b"":""one""}")`. Every other route to a cell
    // already rehydrated; this one read the row directly and skipped it.
    let fields: Vec<(String, Value<'static>)> = if joined.is_empty() {
        ctx.columns
            .iter()
            .enumerate()
            .map(|(i, s)| Ok((s.name.clone(), rehydrate_cell(i, row, ctx)?)))
            .collect::<Result<_, EvalError>>()?
    } else {
        joined
            .into_iter()
            .map(|(i, bare)| Ok((bare.to_string(), rehydrate_cell(i, row, ctx)?)))
            .collect::<Result<_, EvalError>>()?
    };
    Ok(Value::Composite(fields))
}

/// v7.39 (read01 round 56) — read a cell, rehydrating a composite-typed column
/// from its stored JSON into a real `Value::Composite`.
///
/// SPG stores a composite column as JSONB (the on-disk form). Every composite
/// OPERATION — field access `(p).x`, `= ROW(…)`, ordering, the canonical
/// `(2,b)` text form — was already implemented on `Value::Composite`; what was
/// missing is that the value coming out of storage was a `Json`, so all of them
/// failed. Rehydrating here, at the one place a column becomes a Value, makes
/// the whole surface work at once.
///
/// The catalog's type definition supplies the FIELD ORDER (a JSON object is
/// keyed, a composite is positional), which is what PG sorts and renders by.
/// Gated on `user_composite_type.is_some()`, so non-composite columns — every
/// column in almost every schema — pay one Option check.
fn rehydrate_cell(
    pos: usize,
    row: &Row<'_>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let v = row.values[pos].clone().into_owned();
    let Some(cname) = ctx
        .columns
        .get(pos)
        .and_then(|c| c.user_composite_type.as_deref())
    else {
        return Ok(v);
    };
    Ok(json_to_composite(&v, cname, ctx).unwrap_or(v))
}

/// v7.39 (read01 round 56) — rebuild a `Value::Composite` from a stored JSON
/// object, in the composite type's declared field order. `None` when the value
/// isn't a JSON object or the type isn't in the catalog (the caller keeps the
/// raw value, so a pre-FILE_VERSION-63 catalog degrades to the old behaviour
/// rather than erroring).
pub(crate) fn json_to_composite(
    v: &Value<'_>,
    type_name: &str,
    ctx: &EvalContext<'_>,
) -> Option<Value<'static>> {
    let (Value::Json(src) | Value::Text(src)) = v else {
        return None;
    };
    let def = ctx.catalog?.composite_types().get(type_name)?;
    let parsed = crate::json::parse(src.as_ref()).ok()?;
    let crate::json::JsonValue::Object(entries) = parsed else {
        return None;
    };
    let mut fields: alloc::vec::Vec<(alloc::string::String, Value<'static>)> =
        alloc::vec::Vec::with_capacity(def.fields.len());
    for (i, (fname, fty)) in def.fields.iter().enumerate() {
        let found = entries.iter().find(|(k, _)| k == fname);
        // v7.39 (round 264) — a field that is ITSELF a composite rebuilds
        // recursively; otherwise the inner object stayed raw JSON and
        // `(v).inner.street` errored while `row_to_json` nested a string.
        let cell = match (
            found,
            def.field_user_types.get(i).and_then(Option::as_deref),
        ) {
            (None, _) => Value::Null,
            (Some((_, jv)), Some(tn)) => {
                let inner_text = jv.to_json_text();
                json_to_composite(&Value::Json(alloc::borrow::Cow::Owned(inner_text)), tn, ctx)
                    .unwrap_or(Value::Null)
            }
            (Some((_, jv)), None) => json_cell_to_value(jv, *fty),
        };
        fields.push((fname.clone(), cell));
    }
    Some(Value::Composite(fields))
}

/// v7.39 (read01 round 56) — one JSON field of a stored composite, coerced to
/// the field's declared type so `(p).x + 10` is integer arithmetic, not text.
fn json_cell_to_value(jv: &crate::json::JsonValue, ty: spg_storage::DataType) -> Value<'static> {
    use crate::json::JsonValue as J;
    let raw: Value<'static> = match jv {
        J::Null => return Value::Null,
        J::Bool(b) => Value::Bool(*b),
        J::String(s) => Value::text(s.clone()),
        J::Number(n) => Value::Float(*n),
        J::NumberText(t) => Value::text(t.clone()),
        // A nested object / array stays JSON — nested composites and arrays of
        // composites are a recorded residual of this epic.
        other => Value::Json(alloc::borrow::Cow::Owned(other.to_json_text())),
    };
    crate::conversions::coerce_value(raw.clone(), ty, "", 0).unwrap_or(raw)
}
