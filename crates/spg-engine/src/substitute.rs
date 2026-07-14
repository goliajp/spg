//! Statement AST rewriting split out of `lib.rs` (lib.rs split 3):
//! `$N` placeholder substitution (substitute_placeholders /
//! walk_select_exprs_mut / substitute_select / substitute_expr and the
//! LIMIT/OFFSET placeholder resolvers), runtime-Value → AST-Literal
//! conversion (value_to_literal_expr / value_to_literal_expr_permissive
//! plus the timestamp / numeric rendering helpers used to make values
//! round-trip through `Literal`), and column-reference rewriting
//! (rewrite_column_in_source / rewrite_column_in_expr). Free functions;
//! the prepare/bind and view-expansion paths in the crate root and
//! `dml.rs` / `ddl.rs` drive them.

use alloc::string::{String, ToString};

use spg_sql::ast::{Expr, Literal, SelectItem, SelectStatement, Statement};
use spg_storage::Value;

use crate::eval::EvalError;
use crate::{EngineError, value_to_literal};

/// v4.10 helper: materialise a runtime `Value` back into an AST
/// `Expr::Literal` for the subquery-rewrite path. Supports the
/// types `Literal` can represent (Integer / Float / Text / Bool /
/// Null). Date / Timestamp / Numeric / Vector / Interval / JSON
/// would lose precision through Literal and aren't supported in
/// uncorrelated-subquery results; they error with a clear hint.
pub(crate) fn value_to_literal_expr(v: Value) -> Result<Expr, EngineError> {
    let lit = match v {
        Value::Null => Literal::Null,
        Value::SmallInt(n) => Literal::Integer(i64::from(n)),
        Value::Int(n) => Literal::Integer(i64::from(n)),
        Value::BigInt(n) => Literal::Integer(n),
        Value::Float(x) => Literal::Float(x),
        // v7.38 (read01) — a scalar subquery returning NUMERIC (common now that
        // decimal literals are numeric) materialises back through Literal::Numeric.
        Value::Numeric { scaled, scale, .. } => Literal::Numeric {
            unscaled: scaled,
            scale,
        },
        Value::Text(s) | Value::Json(s) => Literal::String(s.into_owned()),
        Value::Bool(b) => Literal::Bool(b),
        // v7.39 (read01 round 54) — the oid-shaped catalog scalars. A subquery
        // like `WHERE indrelid IN (SELECT 't'::regclass)` used to die on
        // "subquery result type None not yet materialisable"; a regclass IS an
        // oid, so it materialises back as the integer the outer comparison wants.
        Value::RegClass(oid, _) => Literal::Integer(oid),
        // v7.37 D.27 — an array-returning scalar subquery (`(SELECT
        // array_agg(...) FROM …)`) materialises through an `Expr::Array` of
        // element literals so the outer query re-evaluates it to the same array.
        Value::IntArray(items) => {
            return Ok(Expr::Array(
                items
                    .into_iter()
                    .map(|o| {
                        Expr::Literal(o.map_or(Literal::Null, |n| Literal::Integer(i64::from(n))))
                    })
                    .collect(),
            ));
        }
        Value::BigIntArray(items) => {
            return Ok(Expr::Array(
                items
                    .into_iter()
                    .map(|o| Expr::Literal(o.map_or(Literal::Null, Literal::Integer)))
                    .collect(),
            ));
        }
        Value::SmallIntArray(items) => {
            return Ok(Expr::Array(
                items
                    .into_iter()
                    .map(|o| {
                        Expr::Literal(o.map_or(Literal::Null, |n| Literal::Integer(i64::from(n))))
                    })
                    .collect(),
            ));
        }
        Value::FloatArray(items) => {
            return Ok(Expr::Array(
                items
                    .into_iter()
                    .map(|o| Expr::Literal(o.map_or(Literal::Null, Literal::Float)))
                    .collect(),
            ));
        }
        Value::TextArray(items) => {
            return Ok(Expr::Array(
                items
                    .into_iter()
                    .map(|o| Expr::Literal(o.map_or(Literal::Null, Literal::String)))
                    .collect(),
            ));
        }
        other => {
            return Err(EngineError::Unsupported(alloc::format!(
                "subquery result type {:?} not yet materialisable; cast to text or integer in the inner SELECT",
                other.data_type()
            )));
        }
    };
    Ok(Expr::Literal(lit))
}

/// v7.13.0 — wider helper used by `INSERT … SELECT` (mailrs
/// round-5 G4). Covers the most common `Value` variants. Types
/// that need lossy textual round-trip (BYTEA, arrays, ts*)
/// surface as an Unsupported error so the caller can add a cast
/// in the inner SELECT.
pub(crate) fn value_to_literal_expr_permissive(v: Value) -> Result<Expr, EngineError> {
    let lit = match v {
        Value::Null => Literal::Null,
        Value::SmallInt(n) => Literal::Integer(i64::from(n)),
        Value::Int(n) => Literal::Integer(i64::from(n)),
        Value::BigInt(n) => Literal::Integer(n),
        Value::Float(x) => Literal::Float(x),
        Value::Text(s) | Value::Json(s) => Literal::String(s.into_owned()),
        Value::Bool(b) => Literal::Bool(b),
        // v7.39 (read01 round 54) — the oid-shaped catalog scalars. A subquery
        // like `WHERE indrelid IN (SELECT 't'::regclass)` used to die on
        // "subquery result type None not yet materialisable"; a regclass IS an
        // oid, so it materialises back as the integer the outer comparison wants.
        Value::RegClass(oid, _) => Literal::Integer(oid),
        Value::Vector(xs) => Literal::Vector(xs.into_owned()),
        // Date / Timestamp / Timestamptz / Numeric round-trip
        // through a TEXT literal that `coerce_value` re-parses
        // against the target column type.
        Value::Date(days) => {
            let micros = crate::conversions::date_days_to_micros(days);
            Literal::String(format_timestamp_micros_as_date(micros))
        }
        Value::Timestamp(us) => Literal::String(format_timestamp_micros(us)),
        Value::Numeric {
            scaled,
            scale,
            kind,
        } => Literal::String(crate::eval::format_numeric_kind(kind, scaled, scale)),
        // v7.37.43-T4 — UUID round-trips through its canonical
        // 8-4-4-4-12 hex text form. `coerce_value` against a UUID
        // target column re-parses the literal back to `Value::Uuid`.
        // Sentori migration 0009_quotas.sql backfills `org_quotas`
        // from `orgs.id` (UUID PK), which exercises this materialise
        // shape on every sqlx-migrate user porting a UUID-keyed
        // schema. Pre-T4 this failed with "INSERT … SELECT cannot
        // materialise value of type Some(Uuid)" and broke the
        // backfill in 0009 plus every analogous DDL+backfill pair.
        Value::Uuid(bytes) => Literal::String(spg_storage::format_uuid(&bytes)),
        // BYTEA: hex-encode through `\x…` literal so re-parse
        // recognises bytea. PG's text form for bytea is `\\xHEX`
        // (single backslash on the wire, escaped here for Rust).
        Value::Bytes(bs) => {
            let mut s = alloc::string::String::with_capacity(2 + bs.len() * 2);
            s.push_str("\\x");
            for b in bs.iter() {
                use core::fmt::Write;
                let _ = write!(&mut s, "{b:02x}");
            }
            Literal::String(s)
        }
        other => {
            return Err(EngineError::Unsupported(alloc::format!(
                "INSERT … SELECT cannot materialise value of type {:?}; \
                 add an explicit CAST in the inner SELECT",
                other.data_type()
            )));
        }
    };
    Ok(Expr::Literal(lit))
}

fn format_timestamp_micros(us: i64) -> String {
    // Same Y/M/D split used by the wire layer; epoch-relative.
    let days = us.div_euclid(86_400_000_000);
    let intra_day = us.rem_euclid(86_400_000_000);
    let date = format_timestamp_micros_as_date(days * 86_400_000_000);
    let secs = intra_day / 1_000_000;
    let us_rem = intra_day % 1_000_000;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    if us_rem == 0 {
        alloc::format!("{date} {h:02}:{m:02}:{s:02}")
    } else {
        alloc::format!("{date} {h:02}:{m:02}:{s:02}.{us_rem:06}")
    }
}

fn format_timestamp_micros_as_date(us: i64) -> String {
    // Days since 1970-01-01 → calendar Y-M-D via the proleptic
    // Gregorian conversion used by spg-engine's date helpers.
    let days = us.div_euclid(86_400_000_000);
    // 1970-01-01 = JDN 2440588.
    let jdn = days + 2_440_588;
    let (y, mo, d) = jdn_to_ymd(jdn);
    alloc::format!("{y:04}-{mo:02}-{d:02}")
}

fn jdn_to_ymd(jdn: i64) -> (i64, u32, u32) {
    // Fliegel & Van Flandern (1968) — works for all positive JDNs.
    let l = jdn + 68569;
    let n = (4 * l) / 146_097;
    let l = l - (146_097 * n + 3) / 4;
    let i = (4000 * (l + 1)) / 1_461_001;
    let l = l - (1461 * i) / 4 + 31;
    let j = (80 * l) / 2447;
    let day = (l - (2447 * j) / 80) as u32;
    let l = j / 11;
    let month = (j + 2 - 12 * l) as u32;
    let year = 100 * (n - 49) + i + l;
    (year, month, day)
}

pub(crate) fn format_numeric(scaled: i128, scale: u8) -> String {
    if scale == 0 {
        return alloc::format!("{scaled}");
    }
    let abs = scaled.unsigned_abs();
    let divisor = 10u128.pow(u32::from(scale));
    let whole = abs / divisor;
    let frac = abs % divisor;
    let sign = if scaled < 0 { "-" } else { "" };
    alloc::format!("{sign}{whole}.{frac:0width$}", width = usize::from(scale))
}

/// v6.1.1 — walk the prepared `Statement` AST and replace every
/// `Expr::Placeholder(n)` with `Expr::Literal(value_to_literal(
/// params[n-1]))`. The dispatch downstream sees a `Statement`
/// indistinguishable from a simple-query parse, so the exec path
/// stays unchanged.
///
/// Errors fall into one shape: a `$N` references past the bound
/// `params.len()`. Out-of-range happens when the Bind didn't
/// supply enough values; pgwire surfaces this as a protocol error
/// to the client.
/// v7.15.0 — rewrite every (potentially-qualified) column
/// identifier matching `old` to `new` in a stored SQL source
/// string. Used by `ALTER TABLE … RENAME COLUMN` to patch
/// CHECK predicate sources, partial-index predicate sources,
/// and runtime DEFAULT expression sources before they get
/// re-parsed on the next INSERT/UPDATE.
///
/// Round-trips through the parser, so the rewritten output is
/// the canonical Display form (matches what the engine stores
/// for fresh predicates). If the source doesn't parse, surfaces
/// the parse error — the invariant that stored predicates are
/// in canonical Display form means a parse failure here is a
/// real bug, not a user mistake to swallow.
pub(crate) fn rewrite_column_in_source(
    src: &str,
    old: &str,
    new: &str,
) -> Result<alloc::string::String, EngineError> {
    let mut expr = spg_sql::parser::parse_expression(src).map_err(|e| {
        EngineError::Unsupported(alloc::format!(
            "ALTER TABLE RENAME COLUMN: stored predicate source {src:?} \
             failed to parse for rewrite ({e})"
        ))
    })?;
    rewrite_column_in_expr(&mut expr, old, new);
    Ok(alloc::format!("{expr}"))
}

/// v7.15.0 — Expr walker that swaps `Expr::Column { name: old, .. }`
/// for `Expr::Column { name: new, .. }`. Qualifier is preserved
/// (e.g. `t.old` → `t.new`); a foreign-table qualifier still
/// gets rewritten because the AST has no way to tell us this
/// predicate is on table T versus table T2 — predicate sources
/// in SPG are always scoped to the owning table, so any
/// qualifier present is either redundant or wrong.
fn rewrite_column_in_expr(e: &mut Expr, old: &str, new: &str) {
    match e {
        Expr::AggregateOrdered { call, order_by, .. } => {
            rewrite_column_in_expr(call, old, new);
            for o in order_by.iter_mut() {
                rewrite_column_in_expr(&mut o.expr, old, new);
            }
        }
        Expr::Column(c) => {
            if c.name.eq_ignore_ascii_case(old) {
                c.name = new.to_string();
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_column_in_expr(lhs, old, new);
            rewrite_column_in_expr(rhs, old, new);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
            rewrite_column_in_expr(expr, old, new);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                rewrite_column_in_expr(a, old, new);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            rewrite_column_in_expr(expr, old, new);
            rewrite_column_in_expr(pattern, old, new);
        }
        Expr::Extract { source, .. } => rewrite_column_in_expr(source, old, new),
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                rewrite_column_in_expr(a, old, new);
            }
            for p in partition_by {
                rewrite_column_in_expr(p, old, new);
            }
            for (o, _, _) in order_by {
                rewrite_column_in_expr(o, old, new);
            }
        }
        Expr::Array(items) => {
            for elem in items {
                rewrite_column_in_expr(elem, old, new);
            }
        }
        Expr::ArraySubscript { target, index } => {
            rewrite_column_in_expr(target, old, new);
            rewrite_column_in_expr(index, old, new);
        }
        Expr::ArraySlice { target, lo, hi } => {
            rewrite_column_in_expr(target, old, new);
            if let Some(l) = lo {
                rewrite_column_in_expr(l, old, new);
            }
            if let Some(h) = hi {
                rewrite_column_in_expr(h, old, new);
            }
        }
        Expr::AnyAll { expr, array, .. } => {
            rewrite_column_in_expr(expr, old, new);
            rewrite_column_in_expr(array, old, new);
        }
        Expr::InList { expr, list, .. } => {
            rewrite_column_in_expr(expr, old, new);
            for item in list {
                rewrite_column_in_expr(item, old, new);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                rewrite_column_in_expr(o, old, new);
            }
            for (w, t) in branches {
                rewrite_column_in_expr(w, old, new);
                rewrite_column_in_expr(t, old, new);
            }
            if let Some(e) = else_branch {
                rewrite_column_in_expr(e, old, new);
            }
        }
        // Stored predicate sources never contain subqueries —
        // CHECK / partial-index / runtime_default are all scalar.
        // If a future feature changes that, recurse here.
        Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. } => {}
        Expr::Literal(_) | Expr::Placeholder(_) => {}
    }
}

/// v7.16.0 — walks a parsed statement and replaces every
/// `Expr::Placeholder(N)` with the corresponding `params[N-1]`
/// re-encoded as an `Expr::Literal`. Used internally by
/// `Engine::execute_prepared` AND surfaced for the spg-embedded
/// WAL path (which needs the bind-final AST so replay sees a
/// simple-query-shaped statement, not a `$1`-shaped one). Errors
/// when a placeholder references an index past the params slice.
pub fn substitute_placeholders(
    stmt: &mut Statement,
    params: &[Value<'static>],
) -> Result<(), EngineError> {
    match stmt {
        Statement::Select(s) => substitute_select(s, params)?,
        Statement::Insert(ins) => {
            for row in &mut ins.rows {
                for e in row {
                    substitute_expr(e, params)?;
                }
            }
            // ON CONFLICT DO UPDATE assignments / WHERE can carry
            // placeholders too (`… DO UPDATE SET reason = $2` —
            // mailrs embed round-12).
            if let Some(clause) = &mut ins.on_conflict
                && let spg_sql::ast::OnConflictAction::Update {
                    assignments,
                    where_,
                } = &mut clause.action
            {
                for (_, e) in assignments.iter_mut() {
                    substitute_expr(e, params)?;
                }
                if let Some(w) = where_ {
                    substitute_expr(w, params)?;
                }
            }
            // v7.33 (A1) — `INSERT INTO t SELECT … WHERE x = $1`: the
            // inner SELECT carries placeholders too. Without this a
            // prepared INSERT…SELECT hit the inner SELECT with an
            // unbound `$N` (reachable now the server WALs prepared
            // writes — bind-final render walks the same statement).
            if let Some(sel) = &mut ins.select_source {
                substitute_select(sel, params)?;
            }
        }
        Statement::Update(u) => {
            for (_, e) in &mut u.assignments {
                substitute_expr(e, params)?;
            }
            if let Some(w) = &mut u.where_ {
                substitute_expr(w, params)?;
            }
        }
        Statement::Delete(d) => {
            if let Some(w) = &mut d.where_ {
                substitute_expr(w, params)?;
            }
        }
        Statement::Explain(e) => substitute_select(&mut e.inner, params)?,
        // Other statements (CREATE / BEGIN / SHOW / …) have no
        // expression slots; no walk needed.
        _ => {}
    }
    Ok(())
}

/// v7.25.1 (mailrs round-18) — THE canonical mutable traversal of
/// every expression slot in a SelectStatement, including every
/// nested SelectStatement (CTE bodies, UNION peers, LATERAL derived
/// tables) and the JOIN ON conditions. Round-12 #7b and round-18
/// were both "a hand-rolled Select walker forgot one subtree";
/// every whole-statement rewrite pass (placeholders, clock) must go
/// through here so a new AST slot only needs adding once.
/// Expression-INTERNAL recursion (into subquery nodes inside an
/// Expr) stays the visitor's own responsibility.
pub(crate) fn walk_select_exprs_mut(
    s: &mut SelectStatement,
    f: &mut impl FnMut(&mut Expr) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    for cte in &mut s.ctes {
        // v7.37.43-T4.4 — modifying CTE bodies (INSERT/UPDATE/DELETE)
        // have their own expression slots; walk each variant
        // explicitly through the dml-statement helpers.
        match &mut cte.body {
            spg_sql::ast::CteBody::Select(s2) => walk_select_exprs_mut(s2, f)?,
            spg_sql::ast::CteBody::Insert(ins) => walk_insert_exprs_mut(ins, f)?,
            spg_sql::ast::CteBody::Update(upd) => walk_update_exprs_mut(upd, f)?,
            spg_sql::ast::CteBody::Delete(del) => walk_delete_exprs_mut(del, f)?,
        }
    }
    for item in &mut s.items {
        if let SelectItem::Expr { expr, .. } = item {
            f(expr)?;
        }
    }
    if let Some(from) = &mut s.from {
        if let Some(sub) = &mut from.primary.lateral_subquery {
            walk_select_exprs_mut(sub, f)?;
        }
        for j in &mut from.joins {
            if let Some(sub) = &mut j.table.lateral_subquery {
                walk_select_exprs_mut(sub, f)?;
            }
            if let Some(on) = &mut j.on {
                f(on)?;
            }
        }
    }
    if let Some(w) = &mut s.where_ {
        f(w)?;
    }
    if let Some(gs) = &mut s.group_by {
        for g in gs {
            f(g)?;
        }
    }
    if let Some(h) = &mut s.having {
        f(h)?;
    }
    for o in &mut s.order_by {
        f(&mut o.expr)?;
    }
    for (_, peer) in &mut s.unions {
        walk_select_exprs_mut(peer, f)?;
    }
    Ok(())
}

/// v7.37.43-T4.4 — walk every Expr slot in an INSERT body, including
/// per-row VALUES tuples, ON CONFLICT assignments / where, RETURNING
/// projection, and the optional INSERT…SELECT source.
pub(crate) fn walk_insert_exprs_mut(
    ins: &mut spg_sql::ast::InsertStatement,
    f: &mut impl FnMut(&mut Expr) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    for cte in &mut ins.ctes {
        match &mut cte.body {
            spg_sql::ast::CteBody::Select(s2) => walk_select_exprs_mut(s2, f)?,
            spg_sql::ast::CteBody::Insert(i2) => walk_insert_exprs_mut(i2, f)?,
            spg_sql::ast::CteBody::Update(u2) => walk_update_exprs_mut(u2, f)?,
            spg_sql::ast::CteBody::Delete(d2) => walk_delete_exprs_mut(d2, f)?,
        }
    }
    for row in &mut ins.rows {
        for cell in row.iter_mut() {
            f(cell)?;
        }
    }
    if let Some(sel) = &mut ins.select_source {
        walk_select_exprs_mut(sel, f)?;
    }
    if let Some(oc) = &mut ins.on_conflict
        && let spg_sql::ast::OnConflictAction::Update {
            assignments,
            where_,
        } = &mut oc.action
    {
        for (_, e) in assignments.iter_mut() {
            f(e)?;
        }
        if let Some(w) = where_ {
            f(w)?;
        }
    }
    if let Some(items) = &mut ins.returning {
        for it in items.iter_mut() {
            if let SelectItem::Expr { expr, .. } = it {
                f(expr)?;
            }
        }
    }
    Ok(())
}

/// v7.37.43-T4.4 — walk every Expr slot in an UPDATE body.
pub(crate) fn walk_update_exprs_mut(
    upd: &mut spg_sql::ast::UpdateStatement,
    f: &mut impl FnMut(&mut Expr) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    for cte in &mut upd.ctes {
        match &mut cte.body {
            spg_sql::ast::CteBody::Select(s2) => walk_select_exprs_mut(s2, f)?,
            spg_sql::ast::CteBody::Insert(i2) => walk_insert_exprs_mut(i2, f)?,
            spg_sql::ast::CteBody::Update(u2) => walk_update_exprs_mut(u2, f)?,
            spg_sql::ast::CteBody::Delete(d2) => walk_delete_exprs_mut(d2, f)?,
        }
    }
    for (_, e) in upd.assignments.iter_mut() {
        f(e)?;
    }
    if let Some(w) = &mut upd.where_ {
        f(w)?;
    }
    if let Some(items) = &mut upd.returning {
        for it in items.iter_mut() {
            if let SelectItem::Expr { expr, .. } = it {
                f(expr)?;
            }
        }
    }
    Ok(())
}

/// v7.37.43-T4.4 — walk every Expr slot in a DELETE body.
pub(crate) fn walk_delete_exprs_mut(
    del: &mut spg_sql::ast::DeleteStatement,
    f: &mut impl FnMut(&mut Expr) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    for cte in &mut del.ctes {
        match &mut cte.body {
            spg_sql::ast::CteBody::Select(s2) => walk_select_exprs_mut(s2, f)?,
            spg_sql::ast::CteBody::Insert(i2) => walk_insert_exprs_mut(i2, f)?,
            spg_sql::ast::CteBody::Update(u2) => walk_update_exprs_mut(u2, f)?,
            spg_sql::ast::CteBody::Delete(d2) => walk_delete_exprs_mut(d2, f)?,
        }
    }
    if let Some(w) = &mut del.where_ {
        f(w)?;
    }
    if let Some(items) = &mut del.returning {
        for it in items.iter_mut() {
            if let SelectItem::Expr { expr, .. } = it {
                f(expr)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn substitute_select(
    s: &mut SelectStatement,
    params: &[Value<'static>],
) -> Result<(), EngineError> {
    walk_select_exprs_mut(s, &mut |e| substitute_expr(e, params))?;
    // v7.25.1 — LIMIT/OFFSET placeholders inside CTE bodies and
    // UNION peers resolve through their own recursion (the walker
    // above only visits Expr slots), so handle them per nested
    // statement here.
    for cte in &mut s.ctes {
        if let Some(sel) = cte.body.as_select_mut() {
            resolve_limit_offset_placeholders(sel, params)?;
        }
        // Modifying CTE bodies don't carry top-level LIMIT/OFFSET
        // (PG syntax forbids LIMIT/OFFSET on INSERT/DELETE; UPDATE
        // RETURNING similarly); inner SELECT/FROM walks happen
        // through `walk_select_exprs_mut`'s recursion above.
    }
    for (_, peer) in &mut s.unions {
        resolve_limit_offset_placeholders(peer, params)?;
    }
    // v7.9.24 — LIMIT $N / OFFSET $N placeholder resolution.
    // mailrs H2. After this pass each LIMIT/OFFSET that was a
    // Placeholder is rewritten to Literal so the existing
    // `LimitExpr::as_literal` path consumes a concrete u32.
    if let Some(le) = s.limit {
        s.limit = Some(resolve_limit_placeholder(le, params)?);
    }
    if let Some(le) = s.offset {
        s.offset = Some(resolve_limit_placeholder(le, params)?);
    }
    Ok(())
}

/// v7.25.1 — recursive LIMIT/OFFSET placeholder resolution for
/// nested statements (CTE bodies / UNION peers).
fn resolve_limit_offset_placeholders(
    s: &mut SelectStatement,
    params: &[Value<'static>],
) -> Result<(), EngineError> {
    if let Some(le) = s.limit {
        s.limit = Some(resolve_limit_placeholder(le, params)?);
    }
    if let Some(le) = s.offset {
        s.offset = Some(resolve_limit_placeholder(le, params)?);
    }
    for cte in &mut s.ctes {
        if let Some(sel) = cte.body.as_select_mut() {
            resolve_limit_offset_placeholders(sel, params)?;
        }
    }
    for (_, peer) in &mut s.unions {
        resolve_limit_offset_placeholders(peer, params)?;
    }
    Ok(())
}

fn resolve_limit_placeholder(
    le: spg_sql::ast::LimitExpr,
    params: &[Value<'static>],
) -> Result<spg_sql::ast::LimitExpr, EngineError> {
    use spg_sql::ast::LimitExpr;
    match le {
        LimitExpr::Literal(_) => Ok(le),
        LimitExpr::Placeholder(n) => {
            let idx = usize::from(n).saturating_sub(1);
            let v = params.get(idx).ok_or_else(|| {
                EngineError::Eval(EvalError::PlaceholderOutOfRange {
                    n,
                    bound: u16::try_from(params.len()).unwrap_or(u16::MAX),
                })
            })?;
            let int = match v {
                Value::SmallInt(x) => Some(i64::from(*x)),
                Value::Int(x) => Some(i64::from(*x)),
                Value::BigInt(x) => Some(*x),
                _ => None,
            }
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "LIMIT/OFFSET ${n} bound to non-integer {v:?}"
                ))
            })?;
            if int < 0 {
                return Err(EngineError::Unsupported(alloc::format!(
                    "LIMIT/OFFSET ${n} bound to negative value {int}"
                )));
            }
            let bounded = u32::try_from(int).map_err(|_| {
                EngineError::Unsupported(alloc::format!(
                    "LIMIT/OFFSET ${n} value {int} exceeds u32 range"
                ))
            })?;
            Ok(LimitExpr::Literal(bounded))
        }
    }
}

fn substitute_expr(e: &mut Expr, params: &[Value<'static>]) -> Result<(), EngineError> {
    if let Expr::Placeholder(n) = e {
        let idx = usize::from(*n).saturating_sub(1);
        let v = params.get(idx).ok_or_else(|| {
            EngineError::Eval(EvalError::PlaceholderOutOfRange {
                n: *n,
                bound: u16::try_from(params.len()).unwrap_or(u16::MAX),
            })
        })?;
        *e = Expr::Literal(value_to_literal(v.clone()));
        return Ok(());
    }
    match e {
        Expr::AggregateOrdered { call, order_by, .. } => {
            substitute_expr(call, params)?;
            for o in order_by.iter_mut() {
                substitute_expr(&mut o.expr, params)?;
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            substitute_expr(lhs, params)?;
            substitute_expr(rhs, params)?;
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
            substitute_expr(expr, params)?;
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                substitute_expr(a, params)?;
            }
        }
        Expr::Like { expr, pattern, .. } => {
            substitute_expr(expr, params)?;
            substitute_expr(pattern, params)?;
        }
        Expr::Extract { source, .. } => substitute_expr(source, params)?,
        Expr::ScalarSubquery(s) => substitute_select(s, params)?,
        Expr::Exists { subquery, .. } => substitute_select(subquery, params)?,
        Expr::InSubquery { expr, subquery, .. } => {
            substitute_expr(expr, params)?;
            substitute_select(subquery, params)?;
        }
        Expr::RowInSubquery { row, subquery, .. } => {
            for el in row.iter_mut() {
                substitute_expr(el, params)?;
            }
            substitute_select(subquery, params)?;
        }
        Expr::RowCmpSubquery { row, subquery, .. } => {
            for el in row.iter_mut() {
                substitute_expr(el, params)?;
            }
            substitute_select(subquery, params)?;
        }
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                substitute_expr(a, params)?;
            }
            for p in partition_by {
                substitute_expr(p, params)?;
            }
            for (e, _, _) in order_by {
                substitute_expr(e, params)?;
            }
        }
        Expr::Literal(_) | Expr::Column(_) => {}
        // Already handled above.
        Expr::Placeholder(_) => unreachable!("Placeholder handled at top of fn"),
        Expr::Array(items) => {
            for elem in items {
                substitute_expr(elem, params)?;
            }
        }
        Expr::ArraySubscript { target, index } => {
            substitute_expr(target, params)?;
            substitute_expr(index, params)?;
        }
        Expr::ArraySlice { target, lo, hi } => {
            substitute_expr(target, params)?;
            if let Some(l) = lo {
                substitute_expr(l, params)?;
            }
            if let Some(h) = hi {
                substitute_expr(h, params)?;
            }
        }
        Expr::AnyAll { expr, array, .. } => {
            substitute_expr(expr, params)?;
            substitute_expr(array, params)?;
        }
        Expr::InList { expr, list, .. } => {
            substitute_expr(expr, params)?;
            for item in list {
                substitute_expr(item, params)?;
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                substitute_expr(o, params)?;
            }
            for (w, t) in branches {
                substitute_expr(w, params)?;
                substitute_expr(t, params)?;
            }
            if let Some(e) = else_branch {
                substitute_expr(e, params)?;
            }
        }
    }
    Ok(())
}
