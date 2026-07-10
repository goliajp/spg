//! v7.39 — row-level security enforcement (Phase 1: SELECT `USING`).
//!
//! The catalog side (policies, the ENABLE/FORCE flags, pg_policy/pg_policies)
//! landed in Phase 0. This module turns an RLS-enabled table's policies into a
//! WHERE predicate that the single-table SELECT executor ANDs into its filter,
//! but only for a *policy-subject* session (a non-superuser `SET ROLE`). The
//! default Admin/login session is a superuser and bypasses RLS entirely —
//! byte-identical to a customer on real PG connected as a superuser, so every
//! existing query path is unaffected.
//!
//! Scope (Phase 1): single-table SELECT. A non-superuser query that JOINs an
//! RLS-enabled table fails closed (error) rather than silently under-enforce;
//! cross-table enforcement is a later phase. Subqueries are handled naturally
//! — each inner SELECT re-enters the executor and gets its own RLS pass.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{BinOp, Expr, Literal, SelectStatement};
use spg_storage::{PolicyCmd, TableSchema};

use crate::{Engine, EngineError};

impl Engine {
    /// The RLS `USING` predicate to AND into a single-table SELECT's WHERE, or
    /// `None` when RLS does not apply (superuser session, non-RLS table, or a
    /// derived-table / meta-view primary). Errors (fail-closed) when a
    /// non-superuser query joins tables and any involved base table has RLS
    /// enabled.
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
        Ok(Some(build_select_predicate(
            table.schema(),
            self.current_role(),
        )))
    }

    /// v7.39 (RLS) Phase 1 — fail closed on writes. Write-side enforcement
    /// (INSERT/UPDATE `WITH CHECK`, UPDATE/DELETE `USING`) is Phase 2; until
    /// then a policy-subject (non-superuser) session must not be able to write
    /// an RLS-enabled table unchecked, so such a write errors rather than
    /// bypassing RLS. Superuser sessions and non-RLS tables pass through.
    pub(crate) fn rls_write_guard(&self, table: &str) -> Result<(), EngineError> {
        if self.is_superuser() {
            return Ok(());
        }
        if self
            .active_catalog()
            .get(table)
            .is_some_and(|t| t.schema().row_security)
        {
            return Err(EngineError::Unsupported(
                "row-level security write enforcement (WITH CHECK / USING on \
                 INSERT/UPDATE/DELETE) is not yet implemented; a policy-subject \
                 session cannot write an RLS-enabled table in this build"
                    .into(),
            ));
        }
        Ok(())
    }
}

/// Combine the applicable SELECT policies into one predicate:
/// `(OR of permissive USING) AND (AND of restrictive USING)`, with the
/// session-identity functions folded to the role literal. No applicable
/// permissive policy → default-deny (`false`).
fn build_select_predicate(schema: &TableSchema, role: &str) -> Expr {
    let mut permissive: Vec<Expr> = Vec::new();
    let mut restrictive: Vec<Expr> = Vec::new();
    for p in &schema.policies {
        if !matches!(p.cmd, PolicyCmd::Select | PolicyCmd::All) {
            continue;
        }
        // roles empty = PUBLIC (applies to everyone).
        if !(p.roles.is_empty() || p.roles.iter().any(|r| r.eq_ignore_ascii_case(role))) {
            continue;
        }
        let Some(src) = &p.using_expr else {
            // An ALL policy with only WITH CHECK imposes no read restriction.
            if p.permissive {
                permissive.push(bool_lit(true));
            }
            continue;
        };
        // A stored qual should always re-parse; a corrupt one fails closed.
        let term = match spg_sql::parser::parse_expression(src) {
            Ok(mut e) => {
                fold_session_identity(&mut e, role);
                e
            }
            Err(_) => bool_lit(false),
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

/// Replace the niladic session-identity functions a policy qual may reference
/// (`current_user` / `current_role` / `user` → the effective role;
/// `session_user` → the login) with string literals, so the predicate
/// evaluates correctly in a scan context that carries no session GUCs.
fn fold_session_identity(e: &mut Expr, role: &str) {
    match e {
        Expr::FunctionCall { name, args } if args.is_empty() => {
            let lc = name.to_ascii_lowercase();
            match lc.as_str() {
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
