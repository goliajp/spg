//! v7.39 (round 218) — server-side cursors (`DECLARE / FETCH / MOVE /
//! CLOSE`), the canonical driver path for streaming large result sets
//! (psycopg2 named cursors, JDBC `setFetchSize`). SPG materializes the
//! query at DECLARE (INSENSITIVE semantics — the only behaviour PG
//! actually provides) and serves FETCH / MOVE slices off the stored
//! rows with a PG-position model:
//!
//!   pos ∈ 0..=n+1 — 0 = before first row, i = on row i (1-based),
//!   n+1 = after last row.
//!
//! Live-PG18.4 differential (2026-07-18) pinned the semantics: DECLARE
//! outside a transaction → 25P01; FETCH past the end → 0 rows (no
//! error); a default (no SCROLL keyword) cursor allows backward fetch
//! (PG only rejects it for explicit NO SCROLL, 55000 + HINT); WITH HOLD
//! survives COMMIT; ROLLBACK closes cursors not held by an earlier
//! commit; FETCH/CLOSE on an unknown name → 34000.

use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{CursorDirection, SelectStatement};
use spg_storage::Row;

use crate::{ColumnSchema, EngineError};

/// A cursor that produces its rows on demand instead of at DECLARE.
///
/// Only the scan shape qualifies — one table, an optional WHERE, a
/// projection, and nothing that needs the whole input before the first
/// output row (no ORDER BY, GROUP BY, DISTINCT, join, aggregate,
/// subquery, set operation, or LIMIT). For that shape the resumable
/// state is one hot-tier index, so a FETCH walks forward from where the
/// previous one stopped.
///
/// The statement is kept rather than a prepared plan: rebuilding the
/// projection per batch is per-statement setup, not per-row work, and
/// keeping it avoids a second lifetime-bound plan representation inside
/// the cursor.
#[derive(Debug, Clone)]
pub(crate) struct LazyScan {
    pub stmt: SelectStatement,
    pub table: String,
    pub alias: String,
    /// The snapshot DECLARE ran under, reused by every batch.
    ///
    /// Taking a fresh one per batch would make the cursor sensitive to
    /// commits that land between two FETCHes: outside RR/SER,
    /// `current_snapshot` is per-statement, so a row another connection
    /// committed after DECLARE would appear mid-drain. PG pins the
    /// cursor's snapshot at DECLARE even in READ COMMITTED, and the
    /// eager path gets this for free by reading everything up front.
    pub snapshot: spg_storage::snapshot::Snapshot,
    /// The last row the cursor consumed, and the slot it sat in.
    ///
    /// The id is the authority and the slot is a hint: `vacuum` rebuilds
    /// the row vector when it reclaims tombstones, so a bare index would
    /// point somewhere else afterwards and the drain would skip or
    /// repeat rows. `None` before the first batch.
    pub last_rowid: Option<spg_storage::row_header::RowId>,
    pub slot_hint: usize,
    /// The scan reached the end of the table; `rows` is now the whole
    /// result and the cursor behaves exactly like an eager one.
    pub done: bool,
}

/// One open cursor: its rows and position.
///
/// `rows` is the whole result for an eagerly materialized cursor, and
/// the prefix produced so far for a [`LazyScan`] one. Backward motion
/// therefore never needs to produce anything: a client can only move
/// back over rows it has already moved forward through.
#[derive(Debug, Clone)]
pub(crate) struct OpenCursor {
    pub columns: Vec<ColumnSchema>,
    pub rows: Vec<Row<'static>>,
    /// PG position model: 0 = before first, `i` = on row i (1-based),
    /// `rows.len() + 1` = after last.
    pub pos: usize,
    /// `Some(false)` = explicit NO SCROLL (backward fetch errors 55000).
    pub scroll: Option<bool>,
    /// Declared WITH HOLD — survives its transaction's COMMIT.
    pub hold: bool,
    /// Set at COMMIT for WITH HOLD cursors: an already-held cursor
    /// survives a LATER transaction's ROLLBACK too.
    pub held: bool,
    /// `Some` while the cursor still produces rows on demand. `None`
    /// for an eagerly materialized cursor, and for a lazy one whose
    /// scan has run out (at which point the two are indistinguishable).
    pub lazy: Option<LazyScan>,
}

/// Rows a FETCH returns plus the count a MOVE reports. FETCH streams
/// `rows`; MOVE reports `rows.len()` in its command tag and discards them.
pub(crate) struct FetchSlice {
    pub rows: Vec<Row<'static>>,
}

impl OpenCursor {
    /// Would this direction move backward from `pos`? (What NO SCROLL
    /// rejects.) PG rejects PRIOR / BACKWARD / negative counts / any
    /// ABSOLUTE-RELATIVE landing before the current position.
    fn is_backward(&self, d: CursorDirection) -> bool {
        let n = self.rows.len();
        match d {
            CursorDirection::Prior | CursorDirection::BackwardAll => true,
            CursorDirection::Backward(k) => k > 0,
            CursorDirection::Count(k) => k < 0,
            CursorDirection::First => self.pos > 1,
            CursorDirection::Last => self.pos > n + 1,
            CursorDirection::Absolute(k) => {
                let target = absolute_pos(k, n);
                target < self.pos
            }
            CursorDirection::Relative(k) => k < 0,
            CursorDirection::Next | CursorDirection::All => false,
        }
    }

    /// Execute a FETCH (or MOVE — identical motion, caller discards rows).
    /// Returns the fetched rows in emission order; `self.pos` advances the
    /// way PG's cursor does.
    pub fn fetch(&mut self, d: CursorDirection, name: &str) -> Result<FetchSlice, EngineError> {
        if self.scroll == Some(false) && self.is_backward(d) {
            // PG's main message carries no cursor name; the HINT rides a
            // separate diag field (pgwire splits on "\nHINT:  ").
            let _ = name;
            return Err(EngineError::Unsupported(String::from(
                "cursor can only scan forward\nHINT:  Declare it with SCROLL option to enable backward scan.",
            )));
        }
        let n = self.rows.len();
        let mut out: Vec<Row<'static>> = Vec::new();
        match d {
            CursorDirection::Next => self.step_forward(1, &mut out),
            CursorDirection::Count(k) => {
                if k >= 0 {
                    self.step_forward(k as usize, &mut out);
                } else {
                    self.step_backward(k.unsigned_abs() as usize, &mut out);
                }
            }
            CursorDirection::All => self.step_forward(usize::MAX, &mut out),
            CursorDirection::Prior => self.step_backward(1, &mut out),
            CursorDirection::Backward(k) => {
                if k >= 0 {
                    self.step_backward(k as usize, &mut out);
                } else {
                    self.step_forward(k.unsigned_abs() as usize, &mut out);
                }
            }
            CursorDirection::BackwardAll => self.step_backward(usize::MAX, &mut out),
            CursorDirection::First => self.land_on(1, &mut out),
            CursorDirection::Last => self.land_on(n, &mut out),
            CursorDirection::Absolute(k) => {
                let target = absolute_pos(k, n);
                self.land_on_clamped(target, &mut out);
            }
            CursorDirection::Relative(k) => {
                let target = self.pos.saturating_add_signed(k as isize);
                self.land_on_clamped(target, &mut out);
            }
        }
        Ok(FetchSlice { rows: out })
    }

    /// Move forward up to `k` rows, emitting each; position lands on the
    /// last emitted row, or after-last when the set is exhausted.
    fn step_forward(&mut self, k: usize, out: &mut Vec<Row<'static>>) {
        let n = self.rows.len();
        let mut taken = 0usize;
        while taken < k && self.pos <= n {
            if self.pos == n {
                // On the last row already — the next step exits the set.
                self.pos = n + 1;
                break;
            }
            self.pos += 1;
            out.push(self.rows[self.pos - 1].clone());
            taken += 1;
        }
    }

    /// Move backward up to `k` rows (emitting in reverse order); position
    /// lands on the last emitted row, or before-first when exhausted.
    fn step_backward(&mut self, k: usize, out: &mut Vec<Row<'static>>) {
        let mut taken = 0usize;
        while taken < k && self.pos >= 1 {
            if self.pos == 1 {
                self.pos = 0;
                break;
            }
            self.pos -= 1;
            out.push(self.rows[self.pos - 1].clone());
            taken += 1;
        }
    }

    /// Land exactly on 1-based row `i` (emitting it), when it exists.
    fn land_on(&mut self, i: usize, out: &mut Vec<Row<'static>>) {
        let n = self.rows.len();
        if i >= 1 && i <= n {
            self.pos = i;
            out.push(self.rows[i - 1].clone());
        } else if i == 0 {
            self.pos = 0;
        } else {
            self.pos = n + 1;
        }
    }

    /// ABSOLUTE / RELATIVE: land on `target` (already clamped to
    /// 0..=n+1 by the caller math), emitting the row when on one.
    fn land_on_clamped(&mut self, target: usize, out: &mut Vec<Row<'static>>) {
        let n = self.rows.len();
        let t = target.min(n + 1);
        self.pos = t;
        if t >= 1 && t <= n {
            out.push(self.rows[t - 1].clone());
        }
    }
}

/// PG `ABSOLUTE k` position: k > 0 counts from the start (k = 1 is the
/// first row); k < 0 counts from the end (-1 is the last row); k = 0 is
/// before the first row. Out-of-range clamps to before-first / after-last.
fn absolute_pos(k: i64, n: usize) -> usize {
    if k > 0 {
        (k as usize).min(n + 1)
    } else if k < 0 {
        let from_end = k.unsigned_abs() as usize; // -1 → last (pos n)
        if from_end > n { 0 } else { n + 1 - from_end }
    } else {
        0
    }
}

/// PG's 34000 undefined_cursor message.
pub(crate) fn no_such_cursor(name: &str) -> EngineError {
    EngineError::Unsupported(alloc::format!("cursor \"{name}\" does not exist"))
}

/// The command-tag word FETCH/MOVE report (`FETCH 3` / `MOVE 1`).
pub(crate) fn moved_count(slice: &FetchSlice) -> usize {
    slice.rows.len()
}

impl crate::Engine {
    /// COMMIT boundary: non-HOLD cursors close; WITH HOLD ones become held
    /// (they now survive later ROLLBACKs too).
    pub(crate) fn cursors_on_commit(&mut self) {
        self.cursors.retain(|_, c| c.hold);
        for c in self.cursors.values_mut() {
            c.held = true;
        }
    }

    /// ROLLBACK boundary: everything created in the aborted transaction
    /// closes — only cursors already held by an earlier COMMIT survive.
    pub(crate) fn cursors_on_rollback(&mut self) {
        self.cursors.retain(|_, c| c.held);
    }

    /// The scan shape a [`LazyScan`] cursor can serve, or `None` when the
    /// query needs its whole input before it can answer the first row.
    ///
    /// Everything excluded here either reorders rows (ORDER BY), folds
    /// them (GROUP BY, aggregates, DISTINCT), reads a second source
    /// (join, set operation, CTE, subquery), or expands one input row
    /// into several (SRF in FROM). A cursor over any of those keeps
    /// today's DECLARE-time materialisation.
    ///
    /// Cold-tier rows are excluded too: the generic scan walks them
    /// after the hot tier through the PK index, so resuming across the
    /// boundary would need a second kind of position. A table that has
    /// spilled to cold segments keeps the eager path.
    fn lazy_scan_shape<'a>(
        &self,
        query: &'a spg_sql::ast::Statement,
        hold: bool,
    ) -> Option<(&'a SelectStatement, String, String)> {
        // WITH HOLD outlives its transaction, and PG materializes those
        // at COMMIT precisely so a held cursor cannot see later changes.
        // Producing rows on demand after that point would show them.
        if hold {
            return None;
        }
        let spg_sql::ast::Statement::Select(s) = query else {
            return None;
        };
        if !s.ctes.is_empty()
            || s.distinct
            || s.group_by.is_some()
            || s.group_by_all
            || s.having.is_some()
            || !s.unions.is_empty()
            || !s.order_by.is_empty()
            || s.limit.is_some()
            || s.offset.is_some()
            || s.limit_with_ties
        {
            return None;
        }
        if crate::aggregate::uses_aggregate(s) || crate::subquery::expr_tree_has_subquery(s) {
            return None;
        }
        let from = s.from.as_ref()?;
        if !from.joins.is_empty() {
            return None;
        }
        let p = &from.primary;
        if p.unnest_expr.is_some()
            || p.generate_series_args.is_some()
            || p.table_fn_call.is_some()
            || p.lateral_subquery.is_some()
            || p.as_of_segment.is_some()
            || p.jsonb_each_text_arg.is_some()
            || p.rows_from.is_some()
            || p.scalar_fn_item
        {
            return None;
        }
        let table = self.active_catalog().get(&p.name)?;
        if table.cold_row_count() > 0 {
            return None;
        }
        let alias = p.alias.as_deref().unwrap_or(p.name.as_str());
        Some((s, p.name.clone(), String::from(alias)))
    }

    /// Produce up to `want` more rows for a lazy cursor, appending them
    /// to `rows` and advancing the scan position. `want` of `None` means
    /// "the rest of the table" (FETCH ALL, and the backward-from-the-end
    /// directions that need to know where the end is).
    ///
    /// An error here surfaces from the FETCH that hit it, which is where
    /// PG raises it too — a cursor over `100/(100 - id)` hands back the
    /// first 99 rows and fails on the batch that reaches row 100.
    fn cursor_fill(
        &self,
        lz: &mut LazyScan,
        want: Option<usize>,
        rows: &mut Vec<Row<'static>>,
    ) -> Result<(), EngineError> {
        if lz.done {
            return Ok(());
        }
        let Some(table) = self.active_catalog().get(&lz.table) else {
            // The table went away under an open cursor. Nothing more to
            // produce; what was already fetched stays fetched.
            lz.done = true;
            return Ok(());
        };
        let cols = table.schema().columns.clone();
        let ctx = self.ev_ctx(&cols, Some(lz.alias.as_str()));
        let projection = crate::select::build_projection(
            &lz.stmt.items,
            &cols,
            lz.alias.as_str(),
            self.backslash_escapes,
            Some(self.active_catalog()),
        )?;
        // Same ceiling the materialising path charges through the
        // executor. Without it a cursor would be the one way to build an
        // unbounded row set with `SPG_MAX_QUERY_BYTES` set — the budget
        // lives in the query executor this path deliberately skips.
        // One budget per batch: a FETCH is a statement, so what it caps
        // is how much a single FETCH may produce, and `FETCH ALL` over a
        // huge table trips it exactly like the bare SELECT would.
        let mut budget = crate::bytebudget::ByteBudget::new(self.max_query_bytes);
        let mut produced = 0usize;
        let mut hit_end = true;
        let start = match lz.last_rowid {
            None => 0,
            Some(last) => table.resume_slot_after(last, lz.slot_hint),
        };
        for (i, row) in table.scan_visible_from(start, &lz.snapshot) {
            if want.is_some_and(|w| produced >= w) {
                hit_end = false;
                break;
            }
            // Advance before the row is evaluated, not after: an error
            // here escapes with rows already appended, and a position
            // that still pointed behind them would hand those rows out
            // twice if the cursor were fetched again. It cannot be
            // today — the error aborts the transaction — but the state
            // it leaves behind should not depend on that.
            if let Some(&rid) = table.rowids().get(i) {
                lz.last_rowid = Some(rid);
                lz.slot_hint = i + 1;
            }
            if let Some(w) = &lz.stmt.where_ {
                let cond = crate::eval::eval_expr(w, row, &ctx).map_err(EngineError::Eval)?;
                if !crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)? {
                    continue;
                }
            }
            let mut values = Vec::with_capacity(projection.len());
            for pi in &projection {
                values
                    .push(crate::eval::eval_expr(&pi.expr, row, &ctx).map_err(EngineError::Eval)?);
            }
            budget.charge(crate::bytebudget::approx_values_bytes(&values))?;
            rows.push(Row::new(values));
            produced += 1;
        }
        if hit_end {
            lz.done = true;
        }
        Ok(())
    }

    /// How many rows of the result `d` needs to be answerable. `None`
    /// means the whole thing; `Some(0)` means the motion stays inside
    /// what has already been produced.
    fn lazy_need(d: CursorDirection, pos: usize) -> Option<usize> {
        match d {
            CursorDirection::Next => Some(pos + 1),
            CursorDirection::Count(k) if k >= 0 => Some(pos.saturating_add(k as usize)),
            CursorDirection::Relative(k) if k >= 0 => Some(pos.saturating_add(k as usize)),
            CursorDirection::Absolute(k) if k >= 0 => Some(k as usize),
            CursorDirection::First => Some(1),
            // Everything else either walks backward through rows already
            // produced, or needs the end of the set to be known.
            CursorDirection::Prior
            | CursorDirection::Backward(_)
            | CursorDirection::BackwardAll
            | CursorDirection::Count(_)
            | CursorDirection::Relative(_)
            | CursorDirection::Absolute(_) => Some(0),
            CursorDirection::All | CursorDirection::Last => None,
        }
    }

    /// Execute `DECLARE <name> … CURSOR … FOR <query>`.
    pub(crate) fn exec_declare_cursor(
        &mut self,
        name: String,
        scroll: Option<bool>,
        hold: bool,
        query: spg_sql::ast::Statement,
    ) -> Result<crate::QueryResult, EngineError> {
        // PG: DECLARE requires a transaction block (25P01).
        //
        // v7.39 (round 321, V54) — THIS connection's block, per slot. The
        // global flag is true whenever ANY connection has one open, so a
        // client in autocommit was allowed to declare a cursor because
        // some other client happened to be inside a transaction (the same
        // global-vs-slot trap rounds 298 / 304 / 316 each fixed
        // elsewhere).
        if !self.current_tx.is_some_and(|tx| self.is_tx_open(tx)) {
            return Err(EngineError::Unsupported(String::from(
                "DECLARE CURSOR can only be used in transaction blocks",
            )));
        }
        if self.cursors.contains_key(&name) {
            return Err(EngineError::Unsupported(alloc::format!(
                "cursor \"{name}\" already exists"
            )));
        }
        // A scan-shaped cursor produces its rows as the client fetches
        // them. DECLARE then costs a projection build instead of the
        // whole result set: a cursor over 300k rows that the client
        // drains 1000 at a time used to cost 144 MB before the first
        // row arrived (measured, round 792).
        let (columns, rows, lazy) =
            if let Some((sel, table, alias)) = self.lazy_scan_shape(&query, hold) {
                let Some(t) = self.active_catalog().get(&table) else {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "relation \"{table}\" does not exist"
                    )));
                };
                let scols = t.schema().columns.clone();
                let projection = crate::select::build_projection(
                    &sel.items,
                    &scols,
                    alias.as_str(),
                    self.backslash_escapes,
                    Some(self.active_catalog()),
                )?;
                let columns: Vec<ColumnSchema> = projection
                    .into_iter()
                    .map(|pi| {
                        let mut c = ColumnSchema::new(pi.output_name, pi.ty, pi.nullable);
                        c.user_enum_type = pi.user_enum_type;
                        c.collation_name = pi.collation_name;
                        c.mysql_fsp = pi.mysql_fsp;
                        c
                    })
                    .collect();
                let lz = LazyScan {
                    stmt: sel.clone(),
                    table,
                    alias,
                    snapshot: self.current_snapshot(),
                    last_rowid: None,
                    slot_hint: 0,
                    done: false,
                };
                (columns, Vec::new(), Some(lz))
            } else {
                let result = self.execute_stmt_with_cancel(query, crate::CancelToken::none())?;
                let crate::QueryResult::Rows { columns, rows } = result else {
                    return Err(EngineError::Unsupported(String::from(
                        "DECLARE CURSOR requires a query that returns rows",
                    )));
                };
                (columns, rows, None)
            };
        self.cursors.insert(
            name,
            OpenCursor {
                columns,
                rows,
                pos: 0,
                scroll,
                hold,
                held: false,
                lazy,
            },
        );
        Ok(crate::QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    /// Execute `FETCH <direction> FROM <name>` — returns the fetched rows
    /// with the cursor's column schema.
    pub(crate) fn exec_fetch_cursor(
        &mut self,
        name: &str,
        direction: CursorDirection,
    ) -> Result<crate::QueryResult, EngineError> {
        self.cursor_produce_for(name, direction)?;
        let cur = self
            .cursors
            .get_mut(name)
            .ok_or_else(|| no_such_cursor(name))?;
        let slice = cur.fetch(direction, name)?;
        Ok(crate::QueryResult::Rows {
            columns: cur.columns.clone(),
            rows: slice.rows,
        })
    }

    /// Top up a lazy cursor so `direction` is answerable, then leave it
    /// in the map for the caller's `fetch`.
    ///
    /// The cursor is taken out of the map for the duration: producing
    /// rows reads the catalog through `&self`, which cannot be borrowed
    /// while a `&mut` into `self.cursors` is live. It goes back
    /// unconditionally — a cursor whose batch raised an error stays open,
    /// as it does in PG.
    fn cursor_produce_for(
        &mut self,
        name: &str,
        direction: CursorDirection,
    ) -> Result<(), EngineError> {
        let Some(mut cur) = self.cursors.remove(name) else {
            return Err(no_such_cursor(name));
        };
        let outcome = match cur.lazy.as_mut() {
            None => Ok(()),
            Some(lz) if lz.done => Ok(()),
            Some(lz) => match Self::lazy_need(direction, cur.pos) {
                Some(0) => Ok(()),
                need => {
                    let want = need.map(|n| n.saturating_sub(cur.rows.len()));
                    match want {
                        Some(0) => Ok(()),
                        _ => self.cursor_fill(lz, want, &mut cur.rows),
                    }
                }
            },
        };
        if cur.lazy.as_ref().is_some_and(|lz| lz.done) {
            cur.lazy = None;
        }
        self.cursors.insert(String::from(name), cur);
        outcome
    }

    /// Execute `MOVE <direction> FROM <name>` — same motion as FETCH, rows
    /// discarded; `affected` carries the moved count for the `MOVE n` tag.
    pub(crate) fn exec_move_cursor(
        &mut self,
        name: &str,
        direction: CursorDirection,
    ) -> Result<crate::QueryResult, EngineError> {
        self.cursor_produce_for(name, direction)?;
        let cur = self
            .cursors
            .get_mut(name)
            .ok_or_else(|| no_such_cursor(name))?;
        let slice = cur.fetch(direction, name)?;
        Ok(crate::QueryResult::CommandOk {
            affected: moved_count(&slice),
            modified_catalog: false,
        })
    }

    /// Execute `CLOSE <name>` / `CLOSE ALL`.
    pub(crate) fn exec_close_cursor(
        &mut self,
        name: Option<&str>,
    ) -> Result<crate::QueryResult, EngineError> {
        match name {
            Some(n) => {
                if self.cursors.remove(n).is_none() {
                    return Err(no_such_cursor(n));
                }
            }
            None => self.cursors.clear(),
        }
        Ok(crate::QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }
}
