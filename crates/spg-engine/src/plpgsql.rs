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
        let collected = triggers::execute_do_block_top_level(
            &body,
            dts.as_deref(),
            Some(&resolver_fn),
            Some(&for_query_fn),
            Some(&raise_sink),
        );
        // v7.39 (round 757, F31-B3) — deliver the body's RAISE messages
        // even when it errored afterwards (PG sends the notices raised
        // before the failure, then the error).
        engine_cell.borrow_mut().drain_raise_sink(raise_sink);
        let collected = collected
            .map_err(|e| EngineError::Storage(StorageError::Corrupt(alloc::format!("DO: {e}"))))?;
        // engine_cell goes out of scope here, releasing the &mut self borrow
        // Run each embedded statement against the engine. The
        // statements were already substitute-walked for NEW/OLD/
        // locals (those evaluate to engine literals before they
        // land here) so dispatch is plain execute_stmt_with_cancel.
        for stmt in collected {
            // v7.16.2 — preserve current_tx wrap so an outer
            // BEGIN/COMMIT around a DO block keeps the
            // EmbeddedSql writes inside that same tx slot.
            self.execute_stmt_with_cancel(stmt, CancelToken::none())?;
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
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
        self.resolve_plpgsql_stmts_subqueries(&mut block.statements, cancel)
    }

    fn resolve_plpgsql_stmts_subqueries(
        &self,
        stmts: &mut [spg_sql::ast::PlPgSqlStmt],
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        use spg_sql::ast::PlPgSqlStmt;
        for stmt in stmts {
            match stmt {
                PlPgSqlStmt::Assign { value, .. } => {
                    self.resolve_expr_subqueries(value, cancel)?;
                }
                PlPgSqlStmt::Return(spg_sql::ast::ReturnTarget::Expr(e)) => {
                    self.resolve_expr_subqueries(e, cancel)?;
                }
                PlPgSqlStmt::Return(_) => {}
                // v7.39 (read01 round 66) — the set-building statements.
                PlPgSqlStmt::ReturnNext(e) => {
                    self.resolve_expr_subqueries(e, cancel)?;
                }
                PlPgSqlStmt::ReturnQuery(_) => {}
                PlPgSqlStmt::ReturnQueryExecute { sql } => {
                    self.resolve_expr_subqueries(sql, cancel)?;
                }
                PlPgSqlStmt::If {
                    branches,
                    else_branch,
                } => {
                    for (cond, body) in branches.iter_mut() {
                        self.resolve_expr_subqueries(cond, cancel)?;
                        self.resolve_plpgsql_stmts_subqueries(body, cancel)?;
                    }
                    self.resolve_plpgsql_stmts_subqueries(else_branch, cancel)?;
                }
                PlPgSqlStmt::Raise { args, .. } => {
                    for a in args {
                        self.resolve_expr_subqueries(a, cancel)?;
                    }
                }
                PlPgSqlStmt::Assert { condition, message } => {
                    self.resolve_expr_subqueries(condition, cancel)?;
                    if let Some(m) = message {
                        self.resolve_expr_subqueries(m, cancel)?;
                    }
                }
                PlPgSqlStmt::While { condition, body } => {
                    self.resolve_expr_subqueries(condition, cancel)?;
                    self.resolve_plpgsql_stmts_subqueries(body, cancel)?;
                }
                PlPgSqlStmt::ForRange {
                    start, end, body, ..
                } => {
                    self.resolve_expr_subqueries(start, cancel)?;
                    self.resolve_expr_subqueries(end, cancel)?;
                    self.resolve_plpgsql_stmts_subqueries(body, cancel)?;
                }
                PlPgSqlStmt::Loop { body } => {
                    self.resolve_plpgsql_stmts_subqueries(body, cancel)?;
                }
                PlPgSqlStmt::Exit { when } => {
                    if let Some(cond) = when {
                        self.resolve_expr_subqueries(cond, cancel)?;
                    }
                }
                PlPgSqlStmt::Continue { when } => {
                    if let Some(cond) = when {
                        self.resolve_expr_subqueries(cond, cancel)?;
                    }
                }
                PlPgSqlStmt::ExecuteDynamic { sql } => {
                    self.resolve_expr_subqueries(sql, cancel)?;
                }
                PlPgSqlStmt::ForQuery { query, body, .. } => {
                    self.resolve_select_subqueries(query, cancel)?;
                    self.resolve_plpgsql_stmts_subqueries(body, cancel)?;
                }
                PlPgSqlStmt::ForExecute { sql_expr, body, .. } => {
                    self.resolve_expr_subqueries(sql_expr, cancel)?;
                    self.resolve_plpgsql_stmts_subqueries(body, cancel)?;
                }
                PlPgSqlStmt::EmbeddedSql(_) => {
                    // Embedded SQL goes back through execute_stmt
                    // _with_cancel which runs the SELECT-side
                    // resolver itself; nothing to do here.
                }
                PlPgSqlStmt::SelectInto { body, .. } => {
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
