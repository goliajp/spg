//! 8.0.3 — a unique key another transaction has written and not
//! committed.
//!
//! A transaction writes into its own shadow catalog, and the shadow's
//! writes reach the committed catalog at COMMIT, where they are
//! re-checked for uniqueness. So until COMMIT no other session can see
//! the key, and two sessions that write the same key both succeed at the
//! statement and one of them is refused at COMMIT with 40001:
//!
//! ```text
//!   A: BEGIN; INSERT (k=1)            -- told INSERT 0 1
//!   B: INSERT (k=1)                   -- PG waits for A; SPG inserted
//!   A: COMMIT                         -- PG commits; SPG 40001, A's
//!                                         whole transaction is gone
//! ```
//!
//! PostgreSQL gives the key to the FIRST WRITER: B's uniqueness check
//! sees A's in-progress row and waits for A to end, then fails with
//! 23505 if A committed or proceeds if A rolled back — and `ON CONFLICT
//! DO NOTHING` / `DO UPDATE` wait the same way, then see A's row.
//! Measured on PG 18.6, B waited 1.51 s of the 1.5 s A still held.
//!
//! This asks that question before an INSERT arbitrates: does any OTHER
//! live transaction hold an uncommitted row whose key, under any unique
//! constraint or unique index of the table, is one this statement is
//! about to write? If so the statement returns `LockWouldBlock` and the
//! host retries it with the engine lock released — the same wait a row
//! lock takes — and on the retry the holder has either committed (its
//! row is visible and the ordinary paths answer) or rolled back (the
//! key is free).
//!
//! It was reported against 8.0.0 as a regression and it is not one: the
//! same client-held transaction behaves identically on 7.40.11. What
//! 8.0.0 changed is that `pg_sleep` began to sleep — the report's probe
//! held its transaction with `pg_sleep(2)`, which on 7.40.11 returned in
//! 0.4 ms, so A committed before B arrived.
//!
//! The keys are computed by the same functions the uniqueness check
//! uses (`uniqueness_constraint_fold`, `UniqueIndexKeyer`), so a
//! collated, expression or partial key agrees with the check that will
//! run after the wait.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use spg_storage::Value;

use crate::EngineError;
use crate::locks::LockOutcome;

impl crate::Engine {
    /// Wait (by `LockWouldBlock`) while another live transaction holds
    /// an uncommitted key that `rows` would write into `table`.
    ///
    /// Costs nothing when no other transaction has touched the table,
    /// which is every single-session workload.
    pub(crate) fn wait_for_uncommitted_unique_keys(
        &mut self,
        table: &str,
        rows: &[Vec<Value<'static>>],
    ) -> Result<(), EngineError> {
        let me = self.current_tx;
        let my_version = me.and_then(|tx| self.tx_writer_versions.get(&tx).copied());
        let others_touch = self
            .tx_catalogs
            .iter()
            .any(|(id, st)| Some(*id) != me && st.touched_tables.contains(table));
        if rows.is_empty() || !others_touch {
            if let Some(v) = my_version {
                self.locks.clear_wait(v);
            }
            return Ok(());
        }
        let holders = self.uncommitted_unique_holders(table, rows)?;
        let Some(v) = my_version else {
            // An autocommit statement holds nothing across the retry, so
            // it cannot close a cycle; it waits without an edge.
            return if holders.is_empty() {
                Ok(())
            } else {
                Err(EngineError::LockWouldBlock)
            };
        };
        if holders.is_empty() {
            self.locks.clear_wait(v);
            return Ok(());
        }
        match self.locks.wait_on_versions(v, &holders) {
            LockOutcome::Deadlock { victim } if victim == v => Err(EngineError::LockDeadlock),
            _ => Err(EngineError::LockWouldBlock),
        }
    }

    /// The writer versions of the other transactions whose uncommitted
    /// rows claim a key `rows` would also claim.
    fn uncommitted_unique_holders(
        &self,
        table: &str,
        rows: &[Vec<Value<'static>>],
    ) -> Result<Vec<u64>, EngineError> {
        let Some(t) = self.active_catalog().get(table) else {
            return Ok(Vec::new());
        };
        let schema = t.schema();
        let mysql = self.speaks_mysql;
        let keyers: Vec<crate::constraints::UniqueIndexKeyer<'_>> = t
            .indices()
            .iter()
            .filter(|i| i.is_unique)
            .map(|i| crate::constraints::UniqueIndexKeyer::new(i, schema, mysql))
            .collect::<Result<_, _>>()?;
        let ucs = &schema.uniqueness_constraints;
        if ucs.is_empty() && keyers.is_empty() {
            return Ok(Vec::new());
        }
        // One set of claimed keys per rule: constraints first, then
        // indexes, so a key only matches under the rule that made it.
        let rule_count = ucs.len() + keyers.len();
        let claims_of = |values: &[Value<'static>]| -> Result<Vec<Option<String>>, EngineError> {
            let mut out = Vec::with_capacity(rule_count);
            for uc in ucs {
                out.push(crate::constraints::uniqueness_constraint_claimed_key(
                    uc, schema, mysql, values,
                ));
            }
            for k in &keyers {
                out.push(k.claimed_key(values)?);
            }
            Ok(out)
        };
        let mut mine: Vec<hashbrown::HashSet<String>> =
            (0..rule_count).map(|_| hashbrown::HashSet::new()).collect();
        for r in rows {
            for (rule, key) in claims_of(r)?.into_iter().enumerate() {
                if let Some(k) = key {
                    mine[rule].insert(k);
                }
            }
        }
        let mut holders: Vec<u64> = Vec::new();
        for (id, st) in &self.tx_catalogs {
            if Some(*id) == self.current_tx || !st.touched_tables.contains(table) {
                continue;
            }
            let Some(&v) = self.tx_writer_versions.get(id) else {
                continue;
            };
            let Some(theirs) = st.catalog.get(table) else {
                continue;
            };
            let ws = theirs.extract_tx_writeset(v);
            let gone: alloc::collections::BTreeSet<spg_storage::row_header::RowId> =
                ws.tombstoned.iter().copied().collect();
            'rows: for (rid, row) in &ws.inserted {
                if gone.contains(rid) {
                    continue;
                }
                for (rule, key) in claims_of(&row.values)?.into_iter().enumerate() {
                    if key.is_some_and(|k| mine[rule].contains(&k)) {
                        holders.push(v);
                        break 'rows;
                    }
                }
            }
        }
        Ok(holders)
    }
}
