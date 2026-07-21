//! v7.39 (round 305, V23) — row-count expression resolution.
//!
//! `LIMIT` / `OFFSET` accept any expression in PG, not just a constant:
//! `LIMIT (SELECT 4)`, `LIMIT greatest(2,3)`. The parser folds what it
//! can at parse time and hands anything else through as
//! [`LimitExpr::Expr`]. This pass runs once per statement, at the single
//! dispatch choke point, and evaluates those down to a literal row count
//! before any executor looks at one.
//!
//! Why it must run there and not lazily: every row-count consumer reads
//! `limit_literal() -> Option<u32>` and takes `None` as "no limit". An
//! expression that survived to execution would therefore not fail — it
//! would quietly return the whole table. Two things guard that:
//!
//!   * the expression walk ([`Expr::for_each_subquery_mut`]) is
//!     compile-time exhaustive, so no nesting shape can be missed;
//!   * `LimitExpr::as_literal` carries a debug assertion, so anything
//!     this pass fails to reach fails loudly in every test build.
//!
//! Semantics measured against PG 18.4 (round 305): NULL means "no
//! limit"; a numeric rounds half away from zero (`LIMIT 2.5` keeps 3);
//! a string coerces by content; a boolean is a type error; a negative
//! count is refused; and a column reference is rejected outright,
//! because the clause is evaluated once, before the query runs.

use alloc::string::String;
use alloc::vec;

use spg_sql::ast::{
    CteBody, Expr, LimitExpr, SelectItem, SelectStatement, Statement,
};
use spg_storage::Value;

use crate::eval::EvalError;
use crate::{CancelToken, Engine, EngineError, QueryResult};

impl Engine {
    /// Resolve every non-constant `LIMIT` / `OFFSET` in `stmt`.
    ///
    /// The statement-level dispatch enumerates the kinds that can carry
    /// a SELECT. Anything reached from there is walked exhaustively; a
    /// statement kind missing from this list would leave its row-count
    /// expression unresolved, which the `as_literal` debug assertion
    /// turns into a loud test failure rather than a wider result set.
    pub(crate) fn resolve_limit_exprs_in_statement(
        &mut self,
        stmt: &mut Statement,
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        match stmt {
            Statement::Select(s) => self.resolve_limits_in_select(s, cancel)?,
            Statement::Insert(ins) => {
                for row in &mut ins.rows {
                    for e in row.iter_mut() {
                        self.resolve_limits_in_expr(e, cancel)?;
                    }
                }
                if let Some(sel) = &mut ins.select_source {
                    self.resolve_limits_in_select(sel, cancel)?;
                }
            }
            Statement::Update(u) => {
                for (_, e) in &mut u.assignments {
                    self.resolve_limits_in_expr(e, cancel)?;
                }
                if let Some(w) = &mut u.where_ {
                    self.resolve_limits_in_expr(w, cancel)?;
                }
            }
            Statement::Delete(d) => {
                if let Some(w) = &mut d.where_ {
                    self.resolve_limits_in_expr(w, cancel)?;
                }
            }
            Statement::Merge(m) => {
                if let Some(sel) = &mut m.source_select {
                    self.resolve_limits_in_select(sel, cancel)?;
                }
            }
            Statement::Explain(e) => {
                self.resolve_limit_exprs_in_statement(&mut e.inner, cancel)?;
            }
            Statement::DeclareCursor { query, .. } => {
                self.resolve_limit_exprs_in_statement(query, cancel)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Every nested statement first, then this select's own row counts —
    /// so a `LIMIT (SELECT … LIMIT (SELECT 1))` has its inner clause
    /// resolved before the outer one is evaluated.
    fn resolve_limits_in_select(
        &mut self,
        s: &mut SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        for cte in &mut s.ctes {
            match &mut cte.body {
                CteBody::Select(s2) => self.resolve_limits_in_select(s2, cancel)?,
                CteBody::Insert(_)
                | CteBody::Update(_)
                | CteBody::Delete(_)
                | CteBody::Merge(_) => {}
            }
        }
        for item in &mut s.items {
            if let SelectItem::Expr { expr, .. } = item {
                self.resolve_limits_in_expr(expr, cancel)?;
            }
        }
        if let Some(from) = &mut s.from {
            // Derived tables — plain and LATERAL alike — ride the
            // `lateral_subquery` channel, so this reaches `FROM (SELECT
            // … LIMIT (SELECT 2)) s` as well as the correlated form.
            if let Some(sub) = &mut from.primary.lateral_subquery {
                self.resolve_limits_in_select(sub, cancel)?;
            }
            for j in &mut from.joins {
                if let Some(sub) = &mut j.table.lateral_subquery {
                    self.resolve_limits_in_select(sub, cancel)?;
                }
                if let Some(on) = &mut j.on {
                    self.resolve_limits_in_expr(on, cancel)?;
                }
            }
        }
        if let Some(w) = &mut s.where_ {
            self.resolve_limits_in_expr(w, cancel)?;
        }
        if let Some(gs) = &mut s.group_by {
            for g in gs.iter_mut() {
                self.resolve_limits_in_expr(g, cancel)?;
            }
        }
        if let Some(h) = &mut s.having {
            self.resolve_limits_in_expr(h, cancel)?;
        }
        for o in &mut s.order_by {
            self.resolve_limits_in_expr(&mut o.expr, cancel)?;
        }
        for (_, peer) in &mut s.unions {
            self.resolve_limits_in_select(peer, cancel)?;
        }
        self.resolve_slot(&mut s.limit, "LIMIT", cancel)?;
        self.resolve_slot(&mut s.offset, "OFFSET", cancel)
    }

    /// Reach any SELECT nested inside an expression. The walk itself is
    /// compile-time exhaustive over `Expr`.
    fn resolve_limits_in_expr(
        &mut self,
        e: &mut Expr,
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        e.for_each_subquery_mut(&mut |sel| self.resolve_limits_in_select(sel, cancel))
    }

    fn resolve_slot(
        &mut self,
        slot: &mut Option<LimitExpr>,
        label: &str,
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        // Every other shape goes straight back where it came from: an
        // empty slot means "no limit", so leaving a `Literal` behind
        // would widen the result to the whole table.
        let mut e = match slot.take() {
            Some(LimitExpr::Expr(e)) => e,
            other => {
                *slot = other;
                return Ok(());
            }
        };
        // The row-count expression can itself hold a subquery with its
        // own non-constant clause; settle those first.
        self.resolve_limits_in_expr(&mut e, cancel)?;
        *slot = self.eval_row_count(&e, label, cancel)?.map(LimitExpr::Literal);
        Ok(())
    }

    /// Evaluate one row-count expression. `None` is PG's "no limit",
    /// which is what a NULL result means.
    fn eval_row_count(
        &mut self,
        e: &Expr,
        label: &str,
        cancel: CancelToken<'_>,
    ) -> Result<Option<u32>, EngineError> {
        // Run it as a one-column, no-FROM SELECT so the whole existing
        // evaluator applies — scalar subqueries, functions, casts, and
        // the cardinality check on a subquery that returns two rows.
        let probe = SelectStatement {
            items: vec![SelectItem::Expr {
                expr: e.clone(),
                alias: None,
            }],
            ..Default::default()
        };
        let value = match self.exec_select_cancel(&probe, cancel) {
            Ok(QueryResult::Rows { rows, .. }) => match rows.as_slice() {
                [r0] => r0.values.first().cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            },
            Ok(_) => Value::Null,
            // A bare column is the one shape PG names specifically: the
            // clause is evaluated once, before the scan, so there is no
            // row for a column to come from.
            Err(EngineError::Eval(EvalError::ColumnNotFound { .. })) => {
                return Err(EngineError::Unsupported(alloc::format!(
                    "argument of {label} must not contain variables"
                )));
            }
            Err(other) => return Err(other),
        };
        let count = match row_count_of(&value, label)? {
            Some(n) => n,
            None => return Ok(None),
        };
        if count < 0 {
            return Err(EngineError::Unsupported(alloc::format!(
                "{label} must not be negative"
            )));
        }
        u32::try_from(count).map(Some).map_err(|_| {
            EngineError::Unsupported(alloc::format!("{label} value too large: {count}"))
        })
    }
}

/// PG coerces the row count to bigint. `None` = NULL = no limit.
/// Mirrors the rules the parser's constant folder applies, including
/// the wording, so a folded `LIMIT 2.5` and an evaluated
/// `LIMIT (SELECT 2.5)` answer the same way.
fn row_count_of(v: &Value<'_>, label: &str) -> Result<Option<i128>, EngineError> {
    let n = match v {
        Value::Null => return Ok(None),
        Value::SmallInt(x) => i128::from(*x),
        Value::Int(x) => i128::from(*x),
        Value::BigInt(x) => i128::from(*x),
        // Half away from zero — PG's numeric→bigint cast, which is what
        // makes `LIMIT 2.5` keep three rows and `LIMIT 2.4` keep two.
        Value::Real(x) => round_half_away(f64::from(*x)),
        Value::Float(x) => round_half_away(*x),
        Value::Numeric { .. } | Value::NumericBig { .. } => {
            let text = crate::eval::value_to_text(v);
            round_decimal_text(&text)
        }
        // PG coerces a string by its CONTENT and fails on the value.
        Value::Text(t) => {
            let t = t.trim();
            match t.parse::<i64>() {
                Ok(n) => i128::from(n),
                Err(_) => {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "invalid input syntax for type bigint: \"{t}\""
                    )));
                }
            }
        }
        other => {
            return Err(EngineError::Unsupported(alloc::format!(
                "argument of {label} must be type bigint, not type {}",
                pg_type_name(other)
            )));
        }
    };
    Ok(Some(n))
}

fn round_half_away(x: f64) -> i128 {
    // `f64::round` is already half-away-from-zero.
    let r = x.round();
    if r.is_finite() { r as i128 } else { 0 }
}

/// Round a decimal rendered as text half away from zero, without going
/// through a float (the value may hold more digits than f64 can carry).
fn round_decimal_text(text: &str) -> i128 {
    let (int_part, frac) = text.split_once('.').unwrap_or((text, ""));
    let base: i128 = int_part.parse().unwrap_or(0);
    let round_up = frac.as_bytes().first().is_some_and(|b| *b >= b'5');
    if !round_up {
        return base;
    }
    if text.starts_with('-') {
        base - 1
    } else {
        base + 1
    }
}

fn pg_type_name(v: &Value<'_>) -> String {
    match v {
        Value::Bool(_) => String::from("boolean"),
        Value::Date(_) => String::from("date"),
        Value::Timestamp(_) => String::from("timestamp without time zone"),
        _ => String::from("record"),
    }
}
