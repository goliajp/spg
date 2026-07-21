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

use spg_sql::ast::CursorDirection;
use spg_storage::Row;

use crate::{ColumnSchema, EngineError};

/// One open cursor: the materialized result set + position.
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
        let result = self.execute_stmt_with_cancel(query, crate::CancelToken::none())?;
        let crate::QueryResult::Rows { columns, rows } = result else {
            return Err(EngineError::Unsupported(String::from(
                "DECLARE CURSOR requires a query that returns rows",
            )));
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
        let cur = self.cursors.get_mut(name).ok_or_else(|| no_such_cursor(name))?;
        let slice = cur.fetch(direction, name)?;
        Ok(crate::QueryResult::Rows {
            columns: cur.columns.clone(),
            rows: slice.rows,
        })
    }

    /// Execute `MOVE <direction> FROM <name>` — same motion as FETCH, rows
    /// discarded; `affected` carries the moved count for the `MOVE n` tag.
    pub(crate) fn exec_move_cursor(
        &mut self,
        name: &str,
        direction: CursorDirection,
    ) -> Result<crate::QueryResult, EngineError> {
        let cur = self.cursors.get_mut(name).ok_or_else(|| no_such_cursor(name))?;
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
