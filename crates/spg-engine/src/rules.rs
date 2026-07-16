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

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use spg_storage::{ColumnSchema, Row, RuleDef};

use crate::triggers::{self, DeferredEmbeddedStmt};
use crate::{CancelToken, Engine, EngineError};

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
    /// whole statement. Conditional-INSTEAD and `DO INSTEAD <command>` forms are
    /// rejected at CREATE RULE time, so a stored `instead` rule with an empty
    /// command body is always an unconditional blocker.
    pub(crate) fn rule_blocks_statement(&self, table: &str, event: &str) -> bool {
        self.snapshot_rules(table, event)
            .iter()
            .any(|r| r.instead && r.commands.is_empty())
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
                    let mut stmt = spg_sql::parser::parse_statement(cmd_text)
                        .map_err(EngineError::Parse)?;
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
