//! v7.39 — row-level security enforcement (Phase 1 SELECT `USING`; Phase 2
//! write side: INSERT/UPDATE `WITH CHECK`, UPDATE/DELETE `USING`).
//!
//! The catalog side (policies, the ENABLE/FORCE flags, pg_policy/pg_policies)
//! landed in Phase 0. Enforcement only applies to a *policy-subject* session (a
//! non-superuser `SET ROLE`); the default Admin/login session is a superuser
//! and bypasses RLS entirely — byte-identical to a customer on real PG
//! connected as a superuser, so every existing path is unaffected.
//!
//! Scope: single-target-table statements. A non-superuser SELECT that JOINs an
//! RLS-enabled table fails closed (cross-table enforcement is a later phase);
//! subqueries are covered by recursion.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{BinOp, Expr, Literal, SelectStatement};
use spg_storage::{ColumnSchema, PolicyCmd, Row, TableSchema, Value};

use crate::eval;
use crate::{Engine, EngineError};

/// Which qual of a policy an enforcement pass reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum QualKind {
    /// The `USING` visibility qual (SELECT / UPDATE / DELETE).
    Using,
    /// The `WITH CHECK` new-row qual (INSERT / UPDATE), falling back to `USING`
    /// when a policy has no explicit `WITH CHECK`.
    WithCheck,
}

impl Engine {
    /// v7.39 (RLS) Phase 1 — the SELECT `USING` predicate to AND into a
    /// single-table SELECT's WHERE, or `None` when RLS does not apply. Errors
    /// (fail-closed) when a non-superuser query joins an RLS-enabled table.
    pub(crate) fn rls_select_predicate(
        &self,
        stmt: &SelectStatement,
    ) -> Result<Option<Expr>, EngineError> {
        if self.is_superuser() {
            return Ok(None);
        }
        let Some(from) = &stmt.from else {
            return Ok(None);
        };
        let cat = self.active_catalog();
        if !from.joins.is_empty() {
            let rls_join = cat
                .get(&from.primary.name)
                .is_some_and(|t| t.schema().row_security)
                || from.joins.iter().any(|j| {
                    cat.get(&j.table.name)
                        .is_some_and(|t| t.schema().row_security)
                });
            if rls_join {
                return Err(EngineError::Unsupported(
                    "row-level security is enforced only on single-table queries in this build; \
                     a join references an RLS-enabled table"
                        .into(),
                ));
            }
            return Ok(None);
        }
        if from.primary.lateral_subquery.is_some() {
            return Ok(None);
        }
        let Some(table) = cat.get(&from.primary.name) else {
            return Ok(None);
        };
        if !table.schema().row_security {
            return Ok(None);
        }
        Ok(Some(build_policy_predicate(
            table.schema(),
            self.current_role(),
            PolicyCmd::Select,
            QualKind::Using,
        )))
    }

    /// v7.39 (RLS) Phase 2 — the `USING` visibility predicate to AND into an
    /// UPDATE / DELETE WHERE (a hidden row is silently skipped, `UPDATE 0`).
    /// `None` when RLS does not apply.
    pub(crate) fn rls_write_using_predicate(&self, table: &str, cmd: PolicyCmd) -> Option<Expr> {
        if self.is_superuser() {
            return None;
        }
        let t = self.active_catalog().get(table)?;
        if !t.schema().row_security {
            return None;
        }
        Some(build_policy_predicate(
            t.schema(),
            self.current_role(),
            cmd,
            QualKind::Using,
        ))
    }

    /// v7.39 (RLS) Phase 2 — validate every new row against the combined
    /// `WITH CHECK` predicate for INSERT / UPDATE. A row that does not satisfy
    /// it raises PG's "new row violates row-level security policy" error.
    /// No-op for a superuser session or a non-RLS table.
    pub(crate) fn rls_check_new_rows(
        &self,
        table: &str,
        cmd: PolicyCmd,
        columns: &[ColumnSchema],
        rows: &[Vec<Value<'static>>],
    ) -> Result<(), EngineError> {
        if self.is_superuser() {
            return Ok(());
        }
        let Some(t) = self.active_catalog().get(table) else {
            return Ok(());
        };
        if !t.schema().row_security {
            return Ok(());
        }
        let pred =
            build_policy_predicate(t.schema(), self.current_role(), cmd, QualKind::WithCheck);
        let ctx = eval::EvalContext::new(columns, None);
        for values in rows {
            let tmp = Row {
                values: values.clone(),
            };
            let v = eval::eval_expr(&pred, &tmp, &ctx).map_err(EngineError::Eval)?;
            // RLS rejects unless the check is definitely true (false OR NULL
            // both violate — stricter than a CHECK constraint, matching PG).
            if !matches!(v, Value::Bool(true)) {
                return Err(EngineError::Unsupported(alloc::format!(
                    "new row violates row-level security policy for table {table:?}"
                )));
            }
        }
        Ok(())
    }
}

/// Combine the applicable policies for `target_cmd` into one predicate:
/// `(OR of permissive) AND (AND of restrictive)`, reading each policy's `USING`
/// or `WITH CHECK` qual per `kind` (WITH CHECK falls back to USING). Session-
/// identity functions are folded to the role literal. No applicable permissive
/// policy → `false` (default-deny for reads; every new row violates for writes).
fn build_policy_predicate(
    schema: &TableSchema,
    role: &str,
    target_cmd: PolicyCmd,
    kind: QualKind,
) -> Expr {
    let mut permissive: Vec<Expr> = Vec::new();
    let mut restrictive: Vec<Expr> = Vec::new();
    for p in &schema.policies {
        if !(p.cmd == target_cmd || p.cmd == PolicyCmd::All) {
            continue;
        }
        // roles empty = PUBLIC (applies to everyone).
        if !(p.roles.is_empty() || p.roles.iter().any(|r| r.eq_ignore_ascii_case(role))) {
            continue;
        }
        let src = match kind {
            QualKind::Using => p.using_expr.as_ref(),
            QualKind::WithCheck => p.with_check_expr.as_ref().or(p.using_expr.as_ref()),
        };
        let Some(src) = src else {
            // A policy that imposes no qual in this mode places no restriction:
            // a permissive one allows, a restrictive one is a no-op.
            if p.permissive {
                permissive.push(bool_lit(true));
            }
            continue;
        };
        let term = match spg_sql::parser::parse_expression(src) {
            Ok(mut e) => {
                fold_session_identity(&mut e, role);
                e
            }
            Err(_) => bool_lit(false), // corrupt stored qual → fail closed
        };
        if p.permissive {
            permissive.push(term);
        } else {
            restrictive.push(term);
        }
    }
    if permissive.is_empty() {
        return bool_lit(false); // default-deny
    }
    let mut pred = or_fold(permissive);
    for r in restrictive {
        pred = and(pred, r);
    }
    pred
}

/// Replace the niladic session-identity functions a qual may reference
/// (`current_user` / `current_role` / `user` → the effective role;
/// `session_user` → the login) with string literals, so the predicate
/// evaluates correctly in a context that carries no session GUCs.
fn fold_session_identity(e: &mut Expr, role: &str) {
    match e {
        Expr::FunctionCall { name, args } if args.is_empty() => {
            match name.to_ascii_lowercase().as_str() {
                "current_user" | "current_role" | "user" => {
                    *e = Expr::Literal(Literal::String(String::from(role)));
                }
                "session_user" => {
                    *e = Expr::Literal(Literal::String(String::from("admin")));
                }
                _ => {}
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            fold_session_identity(lhs, role);
            fold_session_identity(rhs, role);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => fold_session_identity(expr, role),
        Expr::FunctionCall { args, .. } => {
            for a in args {
                fold_session_identity(a, role);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            fold_session_identity(expr, role);
            fold_session_identity(pattern, role);
        }
        Expr::InList { expr, list, .. } => {
            fold_session_identity(expr, role);
            for it in list {
                fold_session_identity(it, role);
            }
        }
        _ => {}
    }
}

fn bool_lit(b: bool) -> Expr {
    Expr::Literal(Literal::Bool(b))
}

fn and(a: Expr, b: Expr) -> Expr {
    Expr::Binary {
        lhs: Box::new(a),
        op: BinOp::And,
        rhs: Box::new(b),
    }
}

fn or_fold(mut terms: Vec<Expr>) -> Expr {
    let mut acc = terms.remove(0);
    for t in terms {
        acc = Expr::Binary {
            lhs: Box::new(acc),
            op: BinOp::Or,
            rhs: Box::new(t),
        };
    }
    acc
}
