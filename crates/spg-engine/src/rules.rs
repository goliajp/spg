// v7.39 (read01 round 139) — CREATE RULE query-rewrite engine.
//
// Phase 1 scope: unconditional `DO INSTEAD NOTHING` rules on
// INSERT / UPDATE / DELETE. Such a rule turns the whole statement into a
// no-op. Every other rule form (conditional `WHERE`, `DO ALSO <command>`,
// `DO INSTEAD <command>`, multi-command bodies, `ON SELECT`) is rejected at
// CREATE RULE time (see `exec_create_rule`), so nothing is silently swallowed
// and the only rules that ever reach here are unconditional blockers.
//
// A blocked statement is realised by AND-ing a constant `FALSE` predicate into
// the target's WHERE (or, for a VALUES INSERT, by clearing the value tuples).
// Routing the block through the normal execution path keeps RETURNING and the
// wire command tag byte-identical with PostgreSQL — a blocked
// `DELETE ... RETURNING` still emits a RowDescription and `DELETE 0`.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{ColumnName, Expr, Literal, UnOp};
use spg_storage::{ColumnSchema, Row, RuleDef};

use crate::triggers::{self, DeferredEmbeddedStmt};
use crate::{CancelToken, Engine, EngineError};

/// v7.39 (round 141) — rewrite a rule's WHERE predicate so it references base
/// columns instead of NEW/OLD, making it pushable into the target statement's
/// own WHERE. `OLD.col` → the base column `col`; `NEW.col` → the column's SET
/// assignment expression (or the base column, i.e. its OLD value, when the
/// column is not assigned — which is always the case for DELETE). The result
/// evaluates against the pre-image row exactly as the rule's NEW/OLD form would.
fn rewrite_rule_pred_to_base(expr: &mut Expr, assignments: &[(String, Expr)]) {
    if let Expr::Column(c) = expr {
        if let Some(q) = &c.qualifier {
            let lower = q.to_ascii_lowercase();
            if lower == "new" {
                if let Some((_, aexpr)) = assignments
                    .iter()
                    .find(|(col, _)| col.eq_ignore_ascii_case(&c.name))
                {
                    // The SET expression is written over base columns (their OLD
                    // values at evaluation time), so it needs no further rewrite.
                    *expr = aexpr.clone();
                    return;
                }
                *expr = Expr::Column(ColumnName {
                    qualifier: None,
                    name: c.name.clone(),
                });
                return;
            }
            if lower == "old" {
                *expr = Expr::Column(ColumnName {
                    qualifier: None,
                    name: c.name.clone(),
                });
                return;
            }
        }
        return;
    }
    match expr {
        Expr::NamedArg { expr, .. }
        | Expr::Variadic(expr)
        | Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::FieldAccess { base: expr, .. }
        | Expr::Extract { source: expr, .. } => rewrite_rule_pred_to_base(expr, assignments),
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_rule_pred_to_base(lhs, assignments);
            rewrite_rule_pred_to_base(rhs, assignments);
        }
        Expr::Like { expr, pattern, .. } => {
            rewrite_rule_pred_to_base(expr, assignments);
            rewrite_rule_pred_to_base(pattern, assignments);
        }
        Expr::FunctionCall { args, .. } | Expr::Array(args) => {
            for a in args {
                rewrite_rule_pred_to_base(a, assignments);
            }
        }
        Expr::AggregateOrdered { call, order_by, .. } => {
            rewrite_rule_pred_to_base(call, assignments);
            for o in order_by.iter_mut() {
                rewrite_rule_pred_to_base(&mut o.expr, assignments);
            }
        }
        Expr::ArraySubscript { target, index } => {
            rewrite_rule_pred_to_base(target, assignments);
            rewrite_rule_pred_to_base(index, assignments);
        }
        Expr::ArraySlice { target, lo, hi } => {
            rewrite_rule_pred_to_base(target, assignments);
            if let Some(l) = lo {
                rewrite_rule_pred_to_base(l, assignments);
            }
            if let Some(h) = hi {
                rewrite_rule_pred_to_base(h, assignments);
            }
        }
        Expr::AnyAll { expr, array, .. } => {
            rewrite_rule_pred_to_base(expr, assignments);
            rewrite_rule_pred_to_base(array, assignments);
        }
        Expr::InList { expr, list, .. } => {
            rewrite_rule_pred_to_base(expr, assignments);
            for item in list {
                rewrite_rule_pred_to_base(item, assignments);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                rewrite_rule_pred_to_base(o, assignments);
            }
            for (w, t) in branches {
                rewrite_rule_pred_to_base(w, assignments);
                rewrite_rule_pred_to_base(t, assignments);
            }
            if let Some(e) = else_branch {
                rewrite_rule_pred_to_base(e, assignments);
            }
        }
        // Leaves and sub-query variants: no NEW/OLD to rewrite (a rule WHERE with
        // a correlated sub-query referencing NEW/OLD is not supported and would
        // surface as an unresolved-column error, not a silent miss).
        _ => {}
    }
}

/// PG's error when a RETURNING statement is suppressed by a `DO INSTEAD
/// NOTHING` rule — the RETURNING rows can never be produced. `op` is the
/// uppercase command keyword (`INSERT` / `UPDATE` / `DELETE`).
pub(crate) fn rule_returning_error(op: &str, table: &str) -> EngineError {
    EngineError::Unsupported(alloc::format!(
        "cannot perform {op} RETURNING on relation \"{table}\"\n\
         HINT:  You need an unconditional ON {op} DO INSTEAD rule with a RETURNING clause."
    ))
}

impl Engine {
    /// Rules catalogued for `(table, event)`, cloned so the borrow on the
    /// catalog is released before any mutation.
    pub(crate) fn snapshot_rules(&self, table: &str, event: &str) -> Vec<RuleDef> {
        self.active_catalog()
            .rules()
            .iter()
            .filter(|r| r.table == table && r.event.eq_ignore_ascii_case(event))
            .cloned()
            .collect()
    }

    /// True when an unconditional `DO INSTEAD NOTHING` rule suppresses the
    /// whole statement. A conditional (`WHERE`) one only narrows the row set —
    /// see [`Engine::conditional_block_predicate`] — so it must not count here.
    pub(crate) fn rule_blocks_statement(&self, table: &str, event: &str) -> bool {
        self.snapshot_rules(table, event)
            .iter()
            .any(|r| r.instead && r.commands.is_empty() && r.when_condition.is_empty())
    }

    /// v7.39 (round 141) — the combined WHERE predicate that keeps a row in a
    /// DELETE / UPDATE when conditional `DO INSTEAD NOTHING` rules apply. Each
    /// such rule blocks the rows its condition holds for; the row survives the
    /// statement iff every rule's condition is NOT TRUE. Returns `None` when no
    /// conditional-blocking rule exists (the caller leaves WHERE untouched).
    /// `assignments` is the UPDATE's SET list (empty for DELETE), used to rewrite
    /// `NEW.col` references into the post-image expression.
    pub(crate) fn conditional_block_predicate(
        &self,
        table: &str,
        event: &str,
        assignments: &[(String, Expr)],
    ) -> Result<Option<Expr>, EngineError> {
        let mut acc: Option<Expr> = None;
        for r in self.snapshot_rules(table, event) {
            if !(r.instead && r.commands.is_empty() && !r.when_condition.is_empty()) {
                continue;
            }
            let mut cond =
                spg_sql::parser::parse_expression(&r.when_condition).map_err(EngineError::Parse)?;
            rewrite_rule_pred_to_base(&mut cond, assignments);
            // Keep (affect) the row iff the rule condition is NOT TRUE — false or
            // NULL both mean "rule does not apply". COALESCE(NOT(cond), TRUE).
            let keep = Expr::FunctionCall {
                name: String::from("coalesce"),
                args: alloc::vec![
                    Expr::Unary {
                        op: UnOp::Not,
                        expr: Box::new(cond)
                    },
                    Expr::Literal(Literal::Bool(true)),
                ],
            };
            acc = Some(match acc {
                Some(a) => Expr::Binary {
                    lhs: Box::new(a),
                    op: spg_sql::ast::BinOp::And,
                    rhs: Box::new(keep),
                },
                None => keep,
            });
        }
        Ok(acc)
    }

    /// v7.39 (round 141) — the conditional `DO INSTEAD NOTHING` rules for
    /// `(table, event)`: INSTEAD rules with a WHERE and no command body. Used to
    /// filter INSERT value tuples per row.
    pub(crate) fn conditional_instead_nothing_rules(
        &self,
        table: &str,
        event: &str,
    ) -> Vec<RuleDef> {
        self.snapshot_rules(table, event)
            .into_iter()
            .filter(|r| r.instead && r.commands.is_empty() && !r.when_condition.is_empty())
            .collect()
    }

    /// v7.39 (round 142) — the `DO INSTEAD <command>` rules for `(table, event)`:
    /// INSTEAD rules that carry a command body. Always unconditional — the
    /// conditional form is refused at CREATE RULE time.
    pub(crate) fn instead_command_rules(&self, table: &str, event: &str) -> Vec<RuleDef> {
        self.snapshot_rules(table, event)
            .into_iter()
            .filter(|r| r.instead && !r.commands.is_empty())
            .collect()
    }

    /// v7.39 (round 140) — the `DO ALSO <command>` rules for `(table, event)`:
    /// non-INSTEAD rules that carry a command body.
    pub(crate) fn also_rules(&self, table: &str, event: &str) -> Vec<RuleDef> {
        self.snapshot_rules(table, event)
            .into_iter()
            .filter(|r| !r.instead && !r.commands.is_empty())
            .collect()
    }

    /// v7.39 (round 140) — build the extra statements a set of `DO ALSO` rules
    /// contributes, given the affected rows' `(NEW, OLD)` images. For each row,
    /// each rule whose `WHERE` predicate holds over that row emits its command
    /// bodies with NEW/OLD substituted to literals (PG evaluates a rule's
    /// command per affected row; for VALUES/point DML that is byte-identical to
    /// its set-based query rewrite).
    pub(crate) fn build_also_rule_stmts(
        rules: &[RuleDef],
        columns: &[ColumnSchema],
        rows: &[(Option<Row<'static>>, Option<Row<'static>>)],
    ) -> Result<Vec<DeferredEmbeddedStmt>, EngineError> {
        let no_locals: BTreeMap<alloc::string::String, spg_storage::Value> = BTreeMap::new();
        let mut out = Vec::new();
        for (new_row, old_row) in rows {
            for rule in rules {
                if !triggers::trigger_when_holds(
                    &rule.when_condition,
                    new_row.as_ref(),
                    old_row.as_ref(),
                    columns,
                )? {
                    continue;
                }
                for cmd_text in &rule.commands {
                    let mut stmt =
                        spg_sql::parser::parse_statement(cmd_text).map_err(EngineError::Parse)?;
                    triggers::substitute_trigger_context_in_statement(
                        &mut stmt,
                        new_row.as_ref(),
                        old_row.as_ref(),
                        &no_locals,
                        columns,
                    )
                    .map_err(EngineError::Eval)?;
                    out.push(DeferredEmbeddedStmt {
                        function: rule.name.clone(),
                        stmt,
                    });
                }
            }
        }
        Ok(out)
    }

    /// v7.39 (round 140) — materialise + run the `DO ALSO` statements for the
    /// affected rows, reusing the trigger cascade's recursion guard so a
    /// rule → command → rule cycle surfaces as an error, not a stack blow-up.
    pub(crate) fn run_also_rules(
        &mut self,
        rules: &[RuleDef],
        columns: &[ColumnSchema],
        rows: &[(Option<Row<'static>>, Option<Row<'static>>)],
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        let stmts = Self::build_also_rule_stmts(rules, columns, rows)?;
        self.execute_deferred_trigger_stmts(stmts, cancel)
    }
}
