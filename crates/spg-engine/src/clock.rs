//! Clock-call rewriting split out of `lib.rs` (lib.rs split 4):
//! folds the zero-argument clock functions (`NOW()` /
//! `CURRENT_TIMESTAMP` / `CURRENT_DATE` / `unix_timestamp()`) and their
//! bare-identifier forms into synthetic `Cast` literals so a single
//! instant is captured per statement and `apply_function` never needs a
//! clock dependency. Walks SELECT / INSERT (rows + ON CONFLICT) /
//! UPDATE / DELETE statement trees, recursing through subqueries, CTEs,
//! window functions, and CASE branches. `value_to_literal` (runtime
//! `Value` → AST `Literal`) lives here too — the substitution path in
//! `substitute.rs` and the view-expansion path in the crate root drive
//! it. Free functions; the prepare/bind path drives `rewrite_clock_calls`.

use spg_sql::ast::{Expr, Literal, SelectStatement, Statement};
use spg_storage::Value;

use crate::eval;
use crate::substitute::walk_select_exprs_mut;

pub(crate) fn value_to_literal(v: Value) -> Literal {
    match v {
        Value::Null => Literal::Null,
        Value::SmallInt(n) => Literal::Integer(i64::from(n)),
        Value::Int(n) => Literal::Integer(i64::from(n)),
        Value::BigInt(n) => Literal::Integer(n),
        Value::Float(x) => Literal::Float(x),
        Value::Text(s) | Value::Json(s) => Literal::String(s.into_owned()),
        Value::Bool(b) => Literal::Bool(b),
        Value::Vector(v) => Literal::Vector(v.into_owned()),
        Value::Numeric { scaled, scale } => Literal::String(eval::format_numeric(scaled, scale)),
        Value::Date(d) => Literal::String(eval::format_date(d)),
        Value::Timestamp(t) => Literal::String(eval::format_timestamp(t)),
        // v7.17.0 Phase 3.P0-69 — UUID round-trips via canonical
        // hyphenated text. Without this arm the fallback below
        // renders `Debug` form ("Uuid([85, …])") which the
        // engine's Text → Uuid coerce can't parse, breaking
        // prepared-bind round-trip from the spg-sqlx adapter.
        Value::Uuid(b) => Literal::String(spg_storage::format_uuid(&b)),
        // v7.16.0 — BYTEA round-trip for the spg-sqlx Bind path.
        // PG-canonical text rep is `\x` + lowercase hex; the
        // engine's coerce_value already accepts that on the
        // text → bytea direction.
        Value::Bytes(b) => Literal::String(eval::format_bytea_hex(&b)),
        // Arrays ride the AST natively (mailrs embed round-12) —
        // the prior `{a,b,c}` text form only worked where a column
        // type drove the re-parse; `= ANY($1)` has no column
        // context and saw a bare Text value.
        Value::TextArray(items) => Literal::TextArray(items),
        Value::IntArray(items) => Literal::IntArray(items),
        Value::BigIntArray(items) => Literal::BigIntArray(items),
        Value::Interval {
            months,
            days,
            micros,
        } => Literal::Interval {
            months,
            days,
            micros,
            text: eval::format_interval(months, days, micros),
        },
        // SQ8 / halfvec cells dequantise to f32 before reaching the
        // substitute walker; pgwire's Bind path handles that.
        Value::Sq8Vector(q) => Literal::Vector(spg_storage::quantize::dequantize(&q)),
        Value::HalfVector(h) => Literal::Vector(h.to_f32_vec()),
        // v7.5.0 — Value is #[non_exhaustive]; future variants
        // render as Debug-form String literal until explicit
        // mapping is added.
        v => Literal::String(alloc::format!("{v:?}")),
    }
}

pub(crate) fn rewrite_clock_calls(stmt: &mut Statement, now_micros: Option<i64>) {
    let Some(now) = now_micros else {
        return;
    };
    match stmt {
        Statement::Select(s) => rewrite_select_clock(s, now),
        Statement::Insert(ins) => {
            for row in &mut ins.rows {
                for e in row {
                    rewrite_expr_clock(e, now);
                }
            }
            // `ON CONFLICT … DO UPDATE SET created_at = NOW()` —
            // the upsert assignments carry clock calls too (mailrs
            // embed round-12).
            if let Some(clause) = &mut ins.on_conflict
                && let spg_sql::ast::OnConflictAction::Update {
                    assignments,
                    where_,
                } = &mut clause.action
            {
                for (_, e) in assignments.iter_mut() {
                    rewrite_expr_clock(e, now);
                }
                if let Some(w) = where_ {
                    rewrite_expr_clock(w, now);
                }
            }
        }
        // `UPDATE … SET seen_at = NOW() WHERE …` / `DELETE … WHERE
        // ts < NOW()` (mailrs embed round-12 — previously only
        // SELECT / INSERT-rows were walked).
        Statement::Update(u) => {
            for (_, e) in &mut u.assignments {
                rewrite_expr_clock(e, now);
            }
            if let Some(w) = &mut u.where_ {
                rewrite_expr_clock(w, now);
            }
        }
        Statement::Delete(d) => {
            if let Some(w) = &mut d.where_ {
                rewrite_expr_clock(w, now);
            }
        }
        _ => {}
    }
}

fn rewrite_select_clock(s: &mut SelectStatement, now: i64) {
    // v7.25.1 (round-18) — shared traversal: CTE bodies, LATERAL
    // subqueries, JOIN ON, and UNION peers all get the clock
    // rewrite (NOW() inside a CTE previously survived to eval as
    // "unknown function `now`").
    let _ = walk_select_exprs_mut(s, &mut |e| {
        rewrite_expr_clock(e, now);
        Ok(())
    });
}

/// v3.0.3 hot path: every recursion lands in exactly one `match` arm.
/// Literal / Column-with-qualifier (the dominant cases on a typical
/// AST) take a single pattern dispatch and exit. The clock-rewrite
/// targets (zero-arg `NOW` / `CURRENT_TIMESTAMP` / `CURRENT_DATE`
/// functions, and bare `CURRENT_TIMESTAMP` / `CURRENT_DATE` column
/// refs) sit on their own arms with match guards so the fall-through
/// to the recursive arms is unambiguous.
fn rewrite_expr_clock(e: &mut Expr, now: i64) {
    // Fast-path test on the no-recursion shapes first. We can't fold
    // them into the big match below because they need to *replace* `e`
    // outright; the recursive arms below match on its sub-fields.
    if let Some(replacement) = clock_replacement_for(e, now) {
        *e = replacement;
        return;
    }
    match e {
        Expr::AggregateOrdered { call, order_by, .. } => {
            rewrite_expr_clock(call, now);
            for o in order_by.iter_mut() {
                rewrite_expr_clock(&mut o.expr, now);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_expr_clock(lhs, now);
            rewrite_expr_clock(rhs, now);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            rewrite_expr_clock(expr, now);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                rewrite_expr_clock(a, now);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            rewrite_expr_clock(expr, now);
            rewrite_expr_clock(pattern, now);
        }
        Expr::Extract { source, .. } => rewrite_expr_clock(source, now),
        // v4.10 subquery nodes — recurse into the inner SELECT's
        // expression slots so e.g. SELECT NOW() in a scalar
        // subquery picks up the same instant as the outer query.
        Expr::ScalarSubquery(s) => rewrite_select_clock(s, now),
        Expr::Exists { subquery, .. } => rewrite_select_clock(subquery, now),
        Expr::InSubquery { expr, subquery, .. } => {
            rewrite_expr_clock(expr, now);
            rewrite_select_clock(subquery, now);
        }
        // v4.12 window functions — args + PARTITION BY + ORDER BY
        // may all reference clock literals.
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                rewrite_expr_clock(a, now);
            }
            for p in partition_by {
                rewrite_expr_clock(p, now);
            }
            for (e, _, _) in order_by {
                rewrite_expr_clock(e, now);
            }
        }
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => {}
        Expr::Array(items) => {
            for elem in items {
                rewrite_expr_clock(elem, now);
            }
        }
        Expr::ArraySubscript { target, index } => {
            rewrite_expr_clock(target, now);
            rewrite_expr_clock(index, now);
        }
        Expr::AnyAll { expr, array, .. } => {
            rewrite_expr_clock(expr, now);
            rewrite_expr_clock(array, now);
        }
        Expr::InList { expr, list, .. } => {
            rewrite_expr_clock(expr, now);
            for item in list {
                rewrite_expr_clock(item, now);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                rewrite_expr_clock(o, now);
            }
            for (w, t) in branches {
                rewrite_expr_clock(w, now);
                rewrite_expr_clock(t, now);
            }
            if let Some(e) = else_branch {
                rewrite_expr_clock(e, now);
            }
        }
    }
}

/// Returns `Some(Expr)` when `e` is one of the clock-call shapes that
/// must be rewritten; otherwise `None` so the caller falls through to
/// the recursive walk. Identifies both function-call forms (`NOW()` /
/// `CURRENT_TIMESTAMP()` / `CURRENT_DATE()`) and bare-identifier forms
/// (`CURRENT_TIMESTAMP` / `CURRENT_DATE` as unqualified column refs,
/// which is how PG accepts them without parens).
fn clock_replacement_for(e: &Expr, now: i64) -> Option<Expr> {
    let (kind, name) = match e {
        Expr::FunctionCall { name, args } if args.is_empty() => (ClockSite::Fn, name.as_str()),
        Expr::Column(c) if c.qualifier.is_none() => (ClockSite::BareIdent, c.name.as_str()),
        _ => return None,
    };
    // ASCII case-insensitive name match. Each entry decides what
    // synthetic literal the call expands to.
    //
    // v7.17.0 Phase 3.P0-29 — `unix_timestamp` (no args) joins this
    // table as MySQL's epoch-seconds equivalent of `now()`. Folded
    // to a BigInt literal here so apply_function never needs a
    // clock dependency.
    enum ClockShape {
        Timestamp,
        Date,
        UnixSeconds,
    }
    let shape = match name.len() {
        3 if kind == ClockSite::Fn && name.eq_ignore_ascii_case("now") => {
            Some(ClockShape::Timestamp)
        }
        12 if name.eq_ignore_ascii_case("current_date") => Some(ClockShape::Date),
        14 if kind == ClockSite::Fn && name.eq_ignore_ascii_case("unix_timestamp") => {
            Some(ClockShape::UnixSeconds)
        }
        17 if name.eq_ignore_ascii_case("current_timestamp") => Some(ClockShape::Timestamp),
        _ => None,
    };
    let shape = shape?;
    let payload = match shape {
        ClockShape::Timestamp => now,
        ClockShape::Date => now.div_euclid(86_400_000_000),
        ClockShape::UnixSeconds => now.div_euclid(1_000_000),
    };
    let target = match shape {
        ClockShape::Timestamp => spg_sql::ast::CastTarget::Timestamp,
        ClockShape::Date => spg_sql::ast::CastTarget::Date,
        ClockShape::UnixSeconds => spg_sql::ast::CastTarget::BigInt,
    };
    Some(Expr::Cast {
        expr: alloc::boxed::Box::new(Expr::Literal(spg_sql::ast::Literal::Integer(payload))),
        target,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClockSite {
    Fn,
    BareIdent,
}
