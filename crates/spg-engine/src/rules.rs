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

use alloc::vec::Vec;

use spg_storage::RuleDef;

use crate::{Engine, EngineError};

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
    /// whole statement. Conditional / `DO ALSO` / `DO INSTEAD <command>` forms
    /// never reach the catalog in Phase 1 (rejected at CREATE RULE time), so a
    /// stored `instead` rule with an empty command body is always a blocker.
    pub(crate) fn rule_blocks_statement(&self, table: &str, event: &str) -> bool {
        self.snapshot_rules(table, event)
            .iter()
            .any(|r| r.instead && r.commands.is_empty())
    }
}
