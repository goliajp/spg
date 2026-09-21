//! PL/pgSQL `DO` block execution. The top-level DO executor walks
//! a parsed PlPgSqlBlock, pre-resolves the subqueries embedded in its
//! expression slots, then drives the block through the shared
//! `triggers` interpreter. Split out of `lib.rs` (cut 22).

use alloc::string::String;

use spg_storage::{StorageError, Value};

use crate::{CancelToken, Engine, EngineError, QueryResult, eval, triggers};

impl Engine {
    /// v7.16.2 — top-level DO block executor. Walks the
    /// PlPgSqlBlock via [`triggers::execute_do_block_top_level`],
    /// then runs each collected EmbeddedSql statement through
    /// the engine's regular execute path (NOT deferred — DO is
    /// outside any row-write borrow). Errors from any step
    /// abort the block and propagate verbatim.
    pub(crate) fn exec_do_block(
        &mut self,
        body: spg_sql::ast::PlPgSqlBlock,
    ) -> Result<QueryResult, EngineError> {
        // v7.16.2 — pre-resolve every subquery the body's
        // expressions reach. `eval::eval_expr` errors on
        // unresolved Exists/ScalarSubquery/InSubquery; the
        // top-level SELECT path runs `resolve_select_subqueries`
        // for the caller — for plpgsql we have to do the
        // equivalent before the body walker runs. Catches the
        // mailrs idiom `IF EXISTS (SELECT 1 FROM
        // information_schema.columns WHERE …) THEN …`.
        let mut body = body;
        self.resolve_plpgsql_block_subqueries(&mut body, CancelToken::none())?;
        let dts = self
            .session_param("default_text_search_config")
            .map(String::from);
        // v7.16.2 — SELECT … INTO resolver. The walker calls
        // this synchronously when it hits a SelectInto stmt
        // so the IF / locals scope sees the result before the
        // next statement. Body walks for trigger paths (no
        // resolver) error loudly on SelectInto.
        // SAFETY: the closure shares this engine borrow with
        // the walker, but the walker only borrows for the
        // duration of `execute_do_block_top_level` and doesn't
        // reach back into the engine through any other path —
        // so the recursive `&mut` is sound. We use a `RefCell`
        // for interior mutability since the closure is
        // Fn-shaped.
        let engine_cell = core::cell::RefCell::new(&mut *self);
        let resolver_fn =
            |stmt: &spg_sql::ast::Statement| -> Result<Value<'static>, triggers::TriggerError> {
                let mut eng = engine_cell.borrow_mut();
                let r = eng
                    .execute_stmt_with_cancel(stmt.clone(), CancelToken::none())
                    .map_err(|e| triggers::TriggerError::EvalFailed {
                        function: "DO".into(),
                        cause: eval::EvalError::TypeMismatch {
                            detail: alloc::format!("SELECT … INTO failed: {e}"),
                        },
                    })?;
                match r {
                    QueryResult::Rows { rows, .. } => match rows.into_iter().next() {
                        Some(row) => Ok(row.values.into_iter().next().unwrap_or(Value::Null)),
                        None => Ok(Value::Null),
                    },
                    _ => Err(triggers::TriggerError::EvalFailed {
                        function: "DO".into(),
                        cause: eval::EvalError::TypeMismatch {
                            detail: "SELECT … INTO body must be a SELECT".into(),
                        },
                    }),
                }
            };
        // v7.37.20 (20.5) — FOR IN SELECT resolver: run the SELECT
        // once, return every row's values.
        let for_query_fn = |stmt: &spg_sql::ast::Statement| -> Result<
            (
                alloc::vec::Vec<alloc::string::String>,
                alloc::vec::Vec<alloc::vec::Vec<Value<'static>>>,
            ),
            triggers::TriggerError,
        > {
            let mut eng = engine_cell.borrow_mut();
            let r = eng
                .execute_stmt_with_cancel(stmt.clone(), CancelToken::none())
                .map_err(|e| triggers::TriggerError::EvalFailed {
                    function: "DO".into(),
                    cause: eval::EvalError::TypeMismatch {
                        detail: alloc::format!("FOR IN SELECT failed: {e}"),
                    },
                })?;
            match r {
                QueryResult::Rows { columns, rows } => Ok((
                    columns.iter().map(|c| c.name.clone()).collect(),
                    rows.into_iter().map(|r| r.values).collect(),
                )),
                _ => Err(triggers::TriggerError::EvalFailed {
                    function: "DO".into(),
                    cause: eval::EvalError::TypeMismatch {
                        detail: "FOR IN body must be a SELECT".into(),
                    },
                }),
            }
        };
        let raise_sink = triggers::NoticeSink::default();
        // 8.0.3 — writes run in place, so an `EXCEPTION` clause can catch
        // what they raise; see `triggers::execute_do_block_live`.
        //
        // The engine error a write failed with is kept beside the walk:
        // an error no handler caught leaves the DO with the SQLSTATE and
        // message the statement itself would have had, and a wait
        // (`LockWouldBlock`) reaches the host as a wait.
        let failed_with: core::cell::RefCell<Option<EngineError>> = core::cell::RefCell::new(None);
        let write_fn = |stmt: &spg_sql::ast::Statement| -> Result<(), triggers::TriggerError> {
            let mut eng = engine_cell.borrow_mut();
            match eng.execute_stmt_with_cancel(stmt.clone(), CancelToken::none()) {
                Ok(_) => Ok(()),
                Err(e) => {
                    let (sqlstate, message) = match &e {
                        EngineError::LockWouldBlock | EngineError::Cancelled => (
                            alloc::borrow::Cow::Borrowed(triggers::INTERNAL_WAIT_SQLSTATE),
                            alloc::format!("{e}"),
                        ),
                        other => {
                            // SQLERRM is the primary message alone, as PG's is.
                            let (code, full) = crate::sqlstate::error_to_wire(other);
                            let (main, _, _) = crate::sqlstate::split_detail_and_hint(&full);
                            let main = crate::sqlstate::without_table_suffix(&code, main);
                            (code, alloc::string::String::from(main))
                        }
                    };
                    *failed_with.borrow_mut() = Some(e);
                    Err(triggers::TriggerError::Sql {
                        function: "DO".into(),
                        sqlstate,
                        message,
                    })
                }
            }
        };
        // The block savepoint. The catalog is persistent, so the snapshot is
        // O(1), and putting it back puts the tables' redo buffers back with
        // it — the model `ROLLBACK TO SAVEPOINT` uses.
        let block_saved: core::cell::RefCell<Option<spg_storage::Catalog>> =
            core::cell::RefCell::new(None);
        let savepoint_fn = |step: triggers::BlockSavepoint| {
            let mut eng = engine_cell.borrow_mut();
            match step {
                triggers::BlockSavepoint::Take => {
                    *block_saved.borrow_mut() = Some(eng.active_catalog().clone());
                }
                triggers::BlockSavepoint::RollBack => {
                    if let Some(c) = block_saved.borrow_mut().take() {
                        *eng.active_catalog_mut() = c;
                    }
                    *failed_with.borrow_mut() = None;
                }
            }
        };
        // 8.0.3 — a DO is ONE statement, and a statement that fails leaves
        // nothing behind. See the CHANGELOG for the measurement: a body whose
        // second INSERT failed kept its first, in memory and not in the WAL.
        let before = engine_cell.borrow().active_catalog().clone();
        // 9.0.0 (A4b) — where the walker leaves the statement it is on,
        // so an error can end with PostgreSQL's `CONTEXT:  PL/pgSQL
        // function inline_code_block line N at <KIND>`.
        let at: core::cell::Cell<(u32, &'static str)> = core::cell::Cell::new((0, ""));
        let outcome = triggers::execute_do_block_live(
            &body,
            dts.as_deref(),
            Some(&resolver_fn),
            Some(&for_query_fn),
            Some(&raise_sink),
            &write_fn,
            &savepoint_fn,
            Some(&at),
        );
        // v7.39 (round 757, F31-B3) — deliver the body's RAISE messages
        // even when it errored afterwards (PG sends the notices raised
        // before the failure, then the error).
        engine_cell.borrow_mut().drain_raise_sink(raise_sink);
        if let Err(e) = outcome {
            *engine_cell.borrow_mut().active_catalog_mut() = before;
            // 9.0.0 (A4b) — PostgreSQL's CONTEXT line, from the
            // statement the walker was on. `inline_code_block` is the
            // name PG gives a DO block's anonymous function.
            let (line, kind) = at.get();
            if line > 0 {
                engine_cell.borrow_mut().set_error_context(alloc::format!(
                    "PL/pgSQL function inline_code_block line {line} at {kind}"
                ));
            }
            if let Some(engine_err) = failed_with.into_inner() {
                return Err(engine_err);
            }
            return Err(match e {
                triggers::TriggerError::RaiseException {
                    message, sqlstate, ..
                } => EngineError::Raised {
                    // 9.0.0 — the block may name its own, through
                    // `SQLSTATE '…'`, a condition name, or
                    // `USING ERRCODE`. P0001 is PG's default for a
                    // `RAISE EXCEPTION` that names none.
                    sqlstate: sqlstate.map_or(alloc::borrow::Cow::Borrowed("P0001"), Into::into),
                    message,
                },
                triggers::TriggerError::Sql {
                    sqlstate, message, ..
                } => EngineError::Raised { sqlstate, message },
                triggers::TriggerError::EvalFailed { cause, .. } => EngineError::Eval(cause),
                other => EngineError::Storage(StorageError::Corrupt(alloc::format!("DO: {other}"))),
            });
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: self.catalog_change_is_committed(),
        })
    }

    /// v7.16.2 — resolve every subquery inside a PlPgSqlBlock's
    /// expression slots so the downstream trigger-flavoured
    /// evaluator (which expects pre-resolved Expr::Literal /
    /// Binary chains) doesn't trip on raw Exists/ScalarSubquery
    /// nodes. Walks IF conditions, Assign values, RAISE args.
    /// EmbeddedSql statements re-enter the engine for execution
    /// later so their subqueries get the normal SELECT-side
    /// resolution.
    fn resolve_plpgsql_block_subqueries(
        &self,
        block: &mut spg_sql::ast::PlPgSqlBlock,
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        for d in &mut block.declarations {
            if let Some(e) = &mut d.default {
                self.resolve_expr_subqueries(e, cancel)?;
            }
        }
        self.resolve_plpgsql_stmts_subqueries(&mut block.statements, cancel)?;
        // 9.0.0 — and the handler bodies, which this walker skipped: a
        // subquery inside `EXCEPTION WHEN others THEN x := (SELECT …)`
        // reached the trigger-flavoured evaluator unresolved.
        for h in &mut block.exception_handlers {
            self.resolve_plpgsql_stmts_subqueries(&mut h.body, cancel)?;
        }
        Ok(())
    }

    fn resolve_plpgsql_stmts_subqueries(
        &self,
        stmts: &mut [spg_sql::ast::PlPgSqlStmt],
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        use spg_sql::ast::PlPgSqlStmtKind;
        for stmt in stmts {
            match &mut stmt.kind {
                PlPgSqlStmtKind::Block(b) => {
                    self.resolve_plpgsql_block_subqueries(b, cancel)?;
                }
                PlPgSqlStmtKind::Assign { value, .. } => {
                    self.resolve_expr_subqueries(value, cancel)?;
                }
                PlPgSqlStmtKind::Return(spg_sql::ast::ReturnTarget::Expr(e)) => {
                    self.resolve_expr_subqueries(e, cancel)?;
                }
                PlPgSqlStmtKind::Return(_) => {}
                // v7.39 (read01 round 66) — the set-building statements.
                PlPgSqlStmtKind::ReturnNext(e) => {
                    self.resolve_expr_subqueries(e, cancel)?;
                }
                PlPgSqlStmtKind::ReturnQuery(_) => {}
                PlPgSqlStmtKind::ReturnQueryExecute { sql } => {
                    self.resolve_expr_subqueries(sql, cancel)?;
                }
                PlPgSqlStmtKind::If {
                    branches,
                    else_branch,
                } => {
                    for (cond, body) in branches.iter_mut() {
                        self.resolve_expr_subqueries(cond, cancel)?;
                        self.resolve_plpgsql_stmts_subqueries(body, cancel)?;
                    }
                    self.resolve_plpgsql_stmts_subqueries(else_branch, cancel)?;
                }
                PlPgSqlStmtKind::Raise { args, .. } => {
                    for a in args {
                        self.resolve_expr_subqueries(a, cancel)?;
                    }
                }
                PlPgSqlStmtKind::Assert { condition, message } => {
                    self.resolve_expr_subqueries(condition, cancel)?;
                    if let Some(m) = message {
                        self.resolve_expr_subqueries(m, cancel)?;
                    }
                }
                PlPgSqlStmtKind::While {
                    condition, body, ..
                } => {
                    self.resolve_expr_subqueries(condition, cancel)?;
                    self.resolve_plpgsql_stmts_subqueries(body, cancel)?;
                }
                PlPgSqlStmtKind::ForRange {
                    start, end, body, ..
                } => {
                    self.resolve_expr_subqueries(start, cancel)?;
                    self.resolve_expr_subqueries(end, cancel)?;
                    self.resolve_plpgsql_stmts_subqueries(body, cancel)?;
                }
                PlPgSqlStmtKind::Loop { body, .. } => {
                    self.resolve_plpgsql_stmts_subqueries(body, cancel)?;
                }
                PlPgSqlStmtKind::Exit { when, label } => {
                    if let Some(cond) = when {
                        self.resolve_expr_subqueries(cond, cancel)?;
                    }
                }
                PlPgSqlStmtKind::Continue { when, label } => {
                    if let Some(cond) = when {
                        self.resolve_expr_subqueries(cond, cancel)?;
                    }
                }
                PlPgSqlStmtKind::ExecuteDynamic { sql } => {
                    self.resolve_expr_subqueries(sql, cancel)?;
                }
                PlPgSqlStmtKind::ForQuery { query, body, .. } => {
                    self.resolve_select_subqueries(query, cancel)?;
                    self.resolve_plpgsql_stmts_subqueries(body, cancel)?;
                }
                PlPgSqlStmtKind::ForExecute { sql_expr, body, .. } => {
                    self.resolve_expr_subqueries(sql_expr, cancel)?;
                    self.resolve_plpgsql_stmts_subqueries(body, cancel)?;
                }
                PlPgSqlStmtKind::EmbeddedSql(_) => {
                    // Embedded SQL goes back through execute_stmt
                    // _with_cancel which runs the SELECT-side
                    // resolver itself; nothing to do here.
                }
                PlPgSqlStmtKind::SelectInto { body, .. } => {
                    // SELECT INTO runs through Engine::execute
                    // when reached, so subquery resolution
                    // happens via the normal SELECT-side path.
                    // Still walk for nested subqueries inside
                    // the SELECT body so eval doesn't trip.
                    self.resolve_select_subqueries(body, cancel)?;
                }
            }
        }
        Ok(())
    }
}
