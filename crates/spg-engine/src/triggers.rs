//! v7.12.4 — PL/pgSQL row-level trigger executor.
//!
//! The catalogued [`spg_storage::FunctionDef`] carries the trigger
//! function's source body as raw text (between the original
//! `$$ ... $$`). Each time a trigger fires we re-parse the body
//! via `spg_sql::parse_function_body` and walk the resulting
//! [`spg_sql::ast::PlPgSqlBlock`] against a NEW / OLD row context.
//!
//! v7.12.4 surface (the minimum that lets a mailrs-shape
//! `update_search_vector` trigger run end-to-end):
//!
//!   * `NEW.col := <expr>;`     — mutate a NEW cell. BEFORE only.
//!   * `RETURN NEW;`            — pass the (possibly-mutated) row
//!                                back to the row writer.
//!   * `RETURN OLD;`            — return the pre-change row.
//!   * `RETURN NULL;` / `RETURN;` — skip the write (BEFORE) or
//!                                no-op the notification (AFTER).
//!   * sub-expression eval recurses through the regular
//!     [`crate::eval::eval_expr`] so anything the SELECT executor
//!     can compute is fair game inside a trigger body.
//!
//! Out of scope for v7.12.4 (land in v7.12.5+):
//!
//!   * `DECLARE`'d local variables
//!   * `IF / ELSIF / ELSE / END IF;` control flow
//!   * Embedded SQL statements (`UPDATE … WHERE …`, `SELECT … INTO var`)
//!   * `RAISE NOTICE / RAISE EXCEPTION`
//!   * Loop constructs

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use spg_sql::ast::{AssignTarget, Expr, PlPgSqlDeclare, PlPgSqlStmt, RaiseLevel, ReturnTarget};
use spg_storage::{ColumnSchema, FunctionDef, Row, StorageError, TriggerDef, Value};

use crate::eval::{self, EvalContext, EvalError};
use crate::{CancelToken, Engine, EngineError, MAX_TRIGGER_RECURSION};

/// v7.12.7 — embedded SQL statement collected during a trigger
/// fire, queued for execution after the firing DML completes.
/// NEW / OLD / DECLARE-local references inside the statement's
/// Expr tree have already been substituted with literals; the
/// engine just feeds it to `execute_stmt_with_cancel`.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredEmbeddedStmt {
    /// Trigger function the embedded SQL came from. Used to
    /// label recursion errors precisely.
    pub function: String,
    /// Substituted statement, ready to execute.
    pub stmt: spg_sql::ast::Statement,
}

/// What the trigger function returned. Drives the row-write path
/// the trigger fired from.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum TriggerOutcome {
    /// `RETURN NEW;` (or `RETURN OLD;`) — write this row.
    /// For BEFORE triggers, the row may differ from the input
    /// (e.g. `NEW.search_vector := …` rewrote a cell). For AFTER
    /// triggers, the value is currently ignored — but we still
    /// surface it for symmetric callers / future v7.12.5 use.
    Row(Row<'static>),
    /// `RETURN NULL;` or trigger fell off the end. For a BEFORE
    /// trigger, the row writer must skip the affected row. For
    /// an AFTER trigger, no-op.
    Skip,
}

/// Result type the trigger executor exposes. Wraps `EvalError`
/// at the eval-of-expressions layer and adds trigger-specific
/// failure modes (`OLD.col := …`, unsupported PL/pgSQL feature,
/// body that fails to re-parse, …).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum TriggerError {
    /// Body source stored in the catalog can't be re-parsed.
    /// Usually means the function was created against a newer
    /// PL/pgSQL surface than the running engine knows about.
    UnparseableBody { function: String, detail: String },
    /// Trigger function uses a v7.12.5+ language feature
    /// (DECLARE, IF, embedded SQL, RAISE, …). The error names
    /// the construct so the operator can plan around it until
    /// the feature lands.
    UnsupportedConstruct { function: String, detail: String },
    /// `OLD.col := <expr>` inside the body. PG itself rejects
    /// this; we surface a clear message rather than silently
    /// dropping the assignment.
    OldIsReadOnly { function: String, column: String },
    /// `NEW.col := <expr>` in an AFTER trigger — same rationale
    /// as OLD: PG enforces "NEW is read-only after the row has
    /// been written" and we mirror.
    NewReadOnlyInAfterTrigger { function: String, column: String },
    /// `NEW.col := <expr>` against a non-existent column.
    /// Usually a schema-drift bug.
    UnknownColumn {
        function: String,
        column: String,
        table: String,
    },
    /// Sub-expression eval inside the trigger body failed. The
    /// wrapped [`EvalError`] explains the underlying cause
    /// (`ColumnNotFound`, `TypeMismatch`, …).
    EvalFailed { function: String, cause: EvalError },
    /// v7.12.6 — `RAISE EXCEPTION '<message>' [, args]*` in the
    /// trigger body. The interpreter formats the args into the
    /// message via PG-style `%` substitution and surfaces the
    /// resolved text up to the caller.
    /// 9.0.0 — `sqlstate` carries what the RAISE named, if it named
    /// one: a literal `SQLSTATE '…'`, a condition name already resolved
    /// to its code, or `USING ERRCODE`.
    RaiseException {
        function: String,
        message: String,
        sqlstate: Option<String>,
    },
    /// 8.0.3 — an embedded SQL statement failed while the block ran. Carries
    /// the SQLSTATE the wire would send, so an `EXCEPTION WHEN
    /// unique_violation` handler can match it, and the client-facing
    /// message `SQLERRM` reads.
    Sql {
        function: String,
        /// 9.0.0 — owned, so a `RAISE … SQLSTATE '<code>'` inside a
        /// handler can carry the code the block wrote.
        sqlstate: alloc::borrow::Cow<'static, str>,
        message: String,
    },
}

impl fmt::Display for TriggerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnparseableBody { function, detail } => {
                write!(
                    f,
                    "trigger function {function:?} body did not parse: {detail}"
                )
            }
            Self::UnsupportedConstruct { function, detail } => {
                write!(
                    f,
                    "trigger function {function:?} uses an unsupported PL/pgSQL construct: {detail}"
                )
            }
            Self::OldIsReadOnly { function, column } => {
                write!(
                    f,
                    "trigger function {function:?}: cannot assign to OLD.{column} (OLD is read-only — PG rule)"
                )
            }
            Self::NewReadOnlyInAfterTrigger { function, column } => {
                write!(
                    f,
                    "trigger function {function:?}: cannot assign to NEW.{column} inside an AFTER trigger \
                     (NEW is read-only post-write — use BEFORE triggers for mutation, or an embedded UPDATE statement \
                      in v7.12.5+)"
                )
            }
            Self::UnknownColumn {
                function,
                column,
                table,
            } => {
                write!(
                    f,
                    "trigger function {function:?}: target column {column:?} not in table {table:?} schema"
                )
            }
            Self::EvalFailed { function, cause } => {
                write!(
                    f,
                    "trigger function {function:?}: expression eval failed: {cause}"
                )
            }
            Self::Sql { message, .. } => f.write_str(message),
            Self::RaiseException {
                function, message, ..
            } => {
                write!(
                    f,
                    "trigger function {function:?}: RAISE EXCEPTION {message:?}"
                )
            }
        }
    }
}

/// v7.39 (read01 round 82) — the firing trigger's identity, for the TG_* magic
/// variables. `op` is `INSERT` / `UPDATE` / `DELETE`; `level` is `ROW` (SPG
/// fires row-level triggers only). `TG_WHEN` derives from `is_after`.
#[derive(Debug)]
pub struct TgMeta<'a> {
    pub op: &'a str,
    pub name: &'a str,
    pub level: &'a str,
}

/// Fire a single row-level trigger.
///
/// `is_after` is true for AFTER triggers; the executor enforces
/// "NEW is read-only" by rejecting NEW.col assignments in that
/// case. AFTER trigger return values are ignored by callers; the
/// returned [`TriggerOutcome`] just carries the (possibly
/// untouched) NEW row for symmetry.
#[allow(clippy::too_many_arguments)] // the table_name / columns / params /
// ts-config trio are independent; folding
// them into a struct just shuffles the
// boilerplate to the call sites without
// material gain.
pub fn fire_row_trigger(
    function: &FunctionDef,
    new_row: Option<Row<'static>>,
    old_row: Option<&Row<'static>>,
    table_name: &str,
    columns: &[ColumnSchema],
    params: &[Value<'static>],
    default_text_search_config: Option<&str>,
    is_after: bool,
    // v7.39 (read01 round 82) — the firing trigger's identity, for the TG_*
    // magic variables (`TG_OP`, `TG_NAME`, `TG_WHEN`, `TG_LEVEL`,
    // `TG_TABLE_NAME`, `TG_NARGS`). PG exposes these to every trigger function;
    // SPG bound none, so any function that read `TG_OP` died on
    // "column tg_op does not exist" — most audit / dispatch triggers do.
    tg: &TgMeta<'_>,
    // v7.39 (round 757, F31-B3) — see [`NoticeSink`].
    notice_sink: Option<&NoticeSink>,
) -> Result<(TriggerOutcome, Vec<DeferredEmbeddedStmt>), TriggerError> {
    if !function.language.eq_ignore_ascii_case("plpgsql") {
        return Err(TriggerError::UnsupportedConstruct {
            function: function.name.clone(),
            detail: format!(
                "v7.12.4 only invokes LANGUAGE plpgsql trigger functions; \
                 {:?} declares LANGUAGE {}",
                function.name, function.language
            ),
        });
    }
    let block = spg_sql::parse_function_body(&function.body).map_err(|e| {
        TriggerError::UnparseableBody {
            function: function.name.clone(),
            detail: format!("{e}"),
        }
    })?;
    // v7.12.6 — initialise local variable scope from the DECLARE
    // block. Each init expr (if any) evaluates against the
    // so-far-bound scope + the NEW/OLD context, so later DECLAREs
    // can reference earlier ones.
    let mut locals: BTreeMap<String, Value<'static>> = BTreeMap::new();
    // v7.39 (read01 round 82) — the TG_* magic variables, bound before the
    // DECLARE block so an initialiser may reference them. PG names them
    // case-insensitively; the interpreter lowercases identifiers, so lowercase
    // keys are what a `TG_OP` reference resolves to.
    locals.insert(
        "tg_op".into(),
        Value::text::<alloc::string::String>(tg.op.into()),
    );
    locals.insert(
        "tg_when".into(),
        Value::text::<alloc::string::String>(if is_after { "AFTER" } else { "BEFORE" }.into()),
    );
    locals.insert(
        "tg_level".into(),
        Value::text::<alloc::string::String>(tg.level.into()),
    );
    locals.insert(
        "tg_name".into(),
        Value::text::<alloc::string::String>(tg.name.into()),
    );
    locals.insert(
        "tg_table_name".into(),
        Value::text::<alloc::string::String>(table_name.into()),
    );
    locals.insert(
        "tg_table_schema".into(),
        Value::text::<alloc::string::String>("public".into()),
    );
    locals.insert(
        "tg_relname".into(),
        Value::text::<alloc::string::String>(table_name.into()),
    );
    locals.insert("tg_nargs".into(), Value::Int(0));
    init_locals_from_declarations(
        &block.declarations,
        &mut locals,
        new_row.as_ref(),
        old_row,
        columns,
        table_name,
        params,
        default_text_search_config,
        &function.name,
        None,
    )?;
    let mut current_new = new_row;
    let ctx = BodyCtx {
        function: &function.name,
        table_name,
        columns,
        params,
        default_text_search_config,
        is_after,
        select_into_resolver: None,
        notice_sink,
        for_query_resolver: None,
        // A trigger function is not set-returning.
        set_sink: None,
        write_resolver: None,
        savepoint: None,
    };
    let mut deferred: Vec<DeferredEmbeddedStmt> = Vec::new();
    let outcome = match execute_stmts(
        &block.statements,
        &mut current_new,
        old_row,
        &mut locals,
        &ctx,
        &mut deferred,
    )? {
        BodyOutcome::Return(target) => resolve_return(target, current_new, old_row),
        // Body fell off without an explicit RETURN. PL/pgSQL
        // default is `RETURN NULL`; we mirror — the BEFORE
        // trigger then skips the row.
        BodyOutcome::FellThrough | BodyOutcome::Break(_) | BodyOutcome::Continue(_) => {
            TriggerOutcome::Skip
        }
    };
    Ok((outcome, deferred))
}

/// v7.12.6 — body-walk return signal. `Return(target)` short-
/// circuits the caller; `FellThrough` means the statement list
/// completed without a RETURN, equivalent to PL/pgSQL's implicit
/// `RETURN NULL`.
enum BodyOutcome {
    Return(ReturnTarget),
    FellThrough,
    /// v7.37.20 (20.2) — `EXIT [WHEN <cond>];` bubbled up through
    /// the current loop body's execute_stmts. WHILE / FOR / bare
    /// LOOP catch this at their iteration point and break; any
    /// non-loop caller treats it as a benign no-op.
    ///
    /// 9.0.0 — carries the label of `EXIT <label>`, which names the
    /// loop OR the enclosing block to leave. `None` is the unlabelled
    /// form, which the innermost loop catches.
    Break(Option<String>),
    /// v7.37.20 (20.2) — `CONTINUE [WHEN <cond>];` bubbled up
    /// through the current loop body. WHILE / FOR / bare LOOP
    /// catch this and jump to the next iteration.
    ///
    /// 9.0.0 — carries the label of `CONTINUE <label>`.
    Continue(Option<String>),
}

/// v7.39 (round 757, F31-B3) — where `RAISE NOTICE / WARNING / INFO`
/// deliver their rendered messages. The caller drains it into the
/// session's pending notices, and pgwire ships one NoticeResponse per
/// entry; `None` (the SELECT-path scalar-function caller, which holds
/// the engine immutably) drops them — ledgered as the B3 residual.
pub type NoticeSink = core::cell::RefCell<Vec<(crate::NoticeSeverity, String)>>;

/// Shared parameters every body-stmt evaluation needs. Bundled so
/// the recursive `execute_stmts` doesn't have to thread eight
/// individual `&str` / `&[…]` args around.
struct BodyCtx<'a> {
    function: &'a str,
    table_name: &'a str,
    columns: &'a [ColumnSchema],
    params: &'a [Value<'static>],
    default_text_search_config: Option<&'a str>,
    is_after: bool,
    /// v7.16.2 — synchronous SELECT … INTO resolver. Provided
    /// by `Engine::exec_do_block` so the walker can run a
    /// SELECT against the engine right when SelectInto is
    /// reached (so subsequent IF reads of the local see the
    /// fresh value). `None` for trigger paths where SelectInto
    /// isn't yet supported.
    select_into_resolver: Option<&'a SelectIntoResolver<'a>>,
    /// v7.39 (round 757, F31-B3) — see [`NoticeSink`].
    notice_sink: Option<&'a NoticeSink>,
    /// v7.37.20 (20.5) — synchronous SELECT-to-rows resolver used
    /// by `FOR <var> IN <SELECT> LOOP`. Provided by the DO block
    /// executor; runs the SELECT once and returns every row.
    for_query_resolver: Option<&'a ForQueryResolver<'a>>,
    /// v7.39 (read01 round 66) — where `RETURN NEXT` / `RETURN QUERY` append
    /// their rows. `None` outside a SETOF function, which makes either statement
    /// an error there — as in PG.
    set_sink: Option<&'a core::cell::RefCell<Vec<Vec<Value<'static>>>>>,
    /// 8.0.3 — runs an embedded write against the engine as the walker
    /// reaches it. Present only for a DO block; a trigger fires inside a
    /// row-write borrow of the catalog and still defers.
    write_resolver: Option<&'a WriteResolver<'a>>,
    /// 9.0.0 — the savepoint hook a protected block uses, so a NESTED
    /// block that carries an EXCEPTION clause rolls its writes back the
    /// way the outermost one already did. `None` on the trigger and
    /// scalar-function paths, which have no savepoint of their own.
    savepoint: Option<&'a dyn Fn(BlockSavepoint)>,
}

/// 8.0.3 — callback a DO block registers so its writes run in place.
/// Running them after the walk put every write outside the block's
/// `EXCEPTION` clause, which could therefore catch nothing a write raised.
pub type WriteResolver<'a> = dyn Fn(&spg_sql::ast::Statement) -> Result<(), TriggerError> + 'a;

/// 8.0.3 — what a protected block asks of its savepoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockSavepoint {
    /// Entering a block that has handlers.
    Take,
    /// A handler matched: undo what the block wrote before the error.
    RollBack,
}

/// v7.16.2 — callback shape the DO-block executor registers
/// on `BodyCtx`. Runs the supplied SELECT statement against
/// the engine, returns the first row's first column.
pub type SelectIntoResolver<'a> =
    dyn Fn(&spg_sql::ast::Statement) -> Result<Value<'static>, TriggerError> + 'a;

/// v7.37.20 (20.5) — callback shape the DO-block executor registers
/// on `BodyCtx` for FOR-IN-SELECT loops. Runs the supplied SELECT
/// statement against the engine and returns every row's values.
/// v7.39 (read01 round 64) — the COLUMN NAMES ride along now, so the loop can
/// bind a record variable's fields (`rec.v`), not just its first cell.
pub type ForQueryResolver<'a> = dyn Fn(
        &spg_sql::ast::Statement,
    ) -> Result<
        (
            alloc::vec::Vec<String>,
            alloc::vec::Vec<alloc::vec::Vec<Value<'static>>>,
        ),
        TriggerError,
    > + 'a;

/// 9.0.0 — run a nested block: its own variable scope, and its own
/// EXCEPTION clause.
///
/// PostgreSQL's scoping, measured: an inner `DECLARE x` SHADOWS the outer
/// `x` for the length of the block and the outer value is back afterwards
/// (`inner 2` / `outer 1`), while an assignment to a variable the inner
/// block did NOT declare reaches the outer one and survives the block
/// (`after 9`). So the names the block declares are saved on entry and
/// put back on exit, and nothing else is touched.
fn execute_nested_block(
    block: &spg_sql::ast::PlPgSqlBlock,
    current_new: &mut Option<Row<'static>>,
    old_row: Option<&Row<'static>>,
    locals: &mut BTreeMap<String, Value<'static>>,
    ctx: &BodyCtx<'_>,
    deferred: &mut Vec<DeferredEmbeddedStmt>,
) -> Result<BodyOutcome, TriggerError> {
    let shadowed: Vec<(String, Option<Value<'static>>)> = block
        .declarations
        .iter()
        .map(|d| (d.name.clone(), locals.get(&d.name).cloned()))
        .collect();
    let result = run_nested_block_body(block, current_new, old_row, locals, ctx, deferred);
    for (name, prior) in shadowed {
        match prior {
            Some(v) => {
                locals.insert(name, v);
            }
            None => {
                locals.remove(&name);
            }
        }
    }
    result
}

fn run_nested_block_body(
    block: &spg_sql::ast::PlPgSqlBlock,
    current_new: &mut Option<Row<'static>>,
    old_row: Option<&Row<'static>>,
    locals: &mut BTreeMap<String, Value<'static>>,
    ctx: &BodyCtx<'_>,
    deferred: &mut Vec<DeferredEmbeddedStmt>,
) -> Result<BodyOutcome, TriggerError> {
    init_locals_from_declarations(
        &block.declarations,
        locals,
        current_new.as_ref(),
        old_row,
        ctx.columns,
        ctx.table_name,
        ctx.params,
        ctx.default_text_search_config,
        ctx.function,
        ctx.select_into_resolver,
    )?;
    let protected = !block.exception_handlers.is_empty();
    // Where this block's own embedded writes start, so a handler can
    // drop exactly them and leave the ones the enclosing block queued.
    let deferred_mark = deferred.len();
    if protected && let Some(sp) = ctx.savepoint {
        sp(BlockSavepoint::Take);
    }
    let body = execute_stmts(
        &block.statements,
        current_new,
        old_row,
        locals,
        ctx,
        deferred,
    );
    if let Ok(outcome) = body {
        // 9.0.0 — `EXIT <label>` naming THIS block leaves it, which is
        // the one jump a block answers: measured on PostgreSQL 18.6,
        // `<<ob>> BEGIN BEGIN EXIT ob; END; RAISE NOTICE 'x'; END`
        // prints nothing.
        if let BodyOutcome::Break(Some(t)) = &outcome
            && block
                .label
                .as_deref()
                .is_some_and(|l| l.eq_ignore_ascii_case(t))
        {
            return Ok(BodyOutcome::FellThrough);
        }
        return Ok(outcome);
    }
    let Err(err) = body else {
        unreachable!("the Ok arm returned above");
    };
    let (sqlstate, message) = error_state(&err);
    let Some(handler) = block
        .exception_handlers
        .iter()
        .find(|h| h.conditions.iter().any(|c| condition_matches(c, &sqlstate)))
    else {
        return Err(err);
    };
    if let Some(sp) = ctx.savepoint {
        sp(BlockSavepoint::RollBack);
    }
    deferred.truncate(deferred_mark);
    locals.insert("sqlerrm".into(), Value::text(message));
    locals.insert(
        "sqlstate".into(),
        Value::text(alloc::string::String::from(sqlstate)),
    );
    execute_stmts(&handler.body, current_new, old_row, locals, ctx, deferred)
}

/// 9.0.0 — does a loop carrying `label` answer this `EXIT` / `CONTINUE`?
/// The unlabelled form is answered by the innermost loop; a labelled one
/// only by the loop that carries that name, and travels outward until it
/// finds it.
fn jump_is_mine(loop_label: Option<&str>, target: Option<&str>) -> bool {
    match target {
        None => true,
        Some(t) => loop_label.is_some_and(|l| l.eq_ignore_ascii_case(t)),
    }
}

fn execute_stmts(
    stmts: &[PlPgSqlStmt],
    current_new: &mut Option<Row<'static>>,
    old_row: Option<&Row<'static>>,
    locals: &mut BTreeMap<String, Value<'static>>,
    ctx: &BodyCtx<'_>,
    deferred: &mut Vec<DeferredEmbeddedStmt>,
) -> Result<BodyOutcome, TriggerError> {
    for stmt in stmts {
        match stmt {
            // 9.0.0 — a nested `[<<label>>] [DECLARE …] BEGIN … END`.
            PlPgSqlStmt::Block(inner) => {
                let outcome =
                    execute_nested_block(inner, current_new, old_row, locals, ctx, deferred)?;
                if !matches!(outcome, BodyOutcome::FellThrough) {
                    return Ok(outcome);
                }
            }
            PlPgSqlStmt::Assign { target, value } => {
                let evaluated = eval_with_new_old_and_locals(
                    value,
                    current_new.as_ref(),
                    old_row,
                    locals,
                    ctx.columns,
                    ctx.table_name,
                    ctx.params,
                    ctx.default_text_search_config,
                    ctx.select_into_resolver,
                )
                .map_err(|cause| TriggerError::EvalFailed {
                    function: ctx.function.into(),
                    cause,
                })?;
                match target {
                    AssignTarget::NewColumn(col) => {
                        // v7.39 (round 767, F31-D4) — PG treats NEW as a
                        // plain plpgsql record variable: assigning to it
                        // inside an AFTER trigger is ACCEPTED and simply
                        // has no effect on the row (measured — the old
                        // hard refusal broke PG-valid triggers reused
                        // across BEFORE/AFTER). The local copy mutates
                        // (later reads in the body see it); the AFTER
                        // caller discards the outcome as before.
                        let pos = ctx
                            .columns
                            .iter()
                            .position(|c| c.name.eq_ignore_ascii_case(col))
                            .ok_or_else(|| TriggerError::UnknownColumn {
                                function: ctx.function.into(),
                                column: col.clone(),
                                table: alloc::string::ToString::to_string(&ctx.table_name),
                            })?;
                        let row = current_new.as_mut().ok_or_else(|| {
                            TriggerError::UnsupportedConstruct {
                                function: ctx.function.into(),
                                detail: format!(
                                    "NEW.{col} := … requires a NEW row context \
                                     (BEFORE INSERT / UPDATE only — not available on DELETE)"
                                ),
                            }
                        })?;
                        row.values[pos] = evaluated;
                    }
                    AssignTarget::OldColumn(col) => {
                        // v7.39 (round 767, F31-D4) — PG accepts an
                        // assignment to OLD too (same record-variable
                        // rule; measured: AFTER UPDATE body running
                        // `OLD.id := 5` succeeds and the table keeps
                        // the real update). SPG has no owned OLD copy
                        // on this path, so the write is accepted and
                        // discarded — a later read of OLD.<col> in the
                        // SAME body sees the original value, a niche
                        // divergence ledgered in the F31 audit.
                        let _ = col;
                        let _ = evaluated;
                    }
                    AssignTarget::Local(name) => {
                        // v7.12.6 — write into the DECLARE scope.
                        // Loose-typing: we don't enforce the
                        // declared type at runtime (PG's INTO
                        // coerces; v7.12.6 just stores the
                        // evaluated Value as-is). Type coercion
                        // tightens in a later release.
                        locals.insert(name.clone(), evaluated);
                    }
                }
            }
            PlPgSqlStmt::Return(target) => {
                return Ok(BodyOutcome::Return(target.clone()));
            }
            // v7.39 (read01 round 66) — `RETURN NEXT <expr>`: append one row and
            // KEEP GOING. It is not a return.
            PlPgSqlStmt::ReturnNext(e) => {
                let sink = ctx
                    .set_sink
                    .ok_or_else(|| TriggerError::UnsupportedConstruct {
                        function: ctx.function.into(),
                        // PG's wording.
                        detail: alloc::string::String::from(
                            "cannot use RETURN NEXT in a non-SETOF function",
                        ),
                    })?;
                let v = eval_with_new_old_and_locals(
                    e,
                    current_new.as_ref(),
                    old_row,
                    locals,
                    ctx.columns,
                    ctx.table_name,
                    ctx.params,
                    ctx.default_text_search_config,
                    ctx.select_into_resolver,
                )
                .map_err(|cause| TriggerError::EvalFailed {
                    function: ctx.function.into(),
                    cause,
                })?;
                sink.borrow_mut().push(alloc::vec![v]);
            }
            // `RETURN QUERY <select>`: append every row it yields, and keep
            // going. This used to desugar to a side-effect SELECT whose rows
            // were DISCARDED — the whole answer, thrown away.
            PlPgSqlStmt::ReturnQuery(query) => {
                let sink = ctx
                    .set_sink
                    .ok_or_else(|| TriggerError::UnsupportedConstruct {
                        function: ctx.function.into(),
                        detail: alloc::string::String::from(
                            "cannot use RETURN QUERY in a non-SETOF function",
                        ),
                    })?;
                let resolver =
                    ctx.for_query_resolver
                        .ok_or_else(|| TriggerError::UnsupportedConstruct {
                            function: ctx.function.into(),
                            detail: alloc::string::String::from(
                                "RETURN QUERY needs a query runner (this context has none)",
                            ),
                        })?;
                let mut stmt = spg_sql::ast::Statement::Select((**query).clone());
                substitute_trigger_context_in_statement(
                    &mut stmt,
                    current_new.as_ref(),
                    old_row,
                    locals,
                    ctx.columns,
                )
                .map_err(|cause| TriggerError::EvalFailed {
                    function: ctx.function.into(),
                    cause,
                })?;
                let (_cols, rows) = resolver(&stmt)?;
                sink.borrow_mut().extend(rows);
            }
            PlPgSqlStmt::If {
                branches,
                else_branch,
            } => {
                let mut matched = false;
                for (cond_expr, body) in branches {
                    let cond_val = eval_with_new_old_and_locals(
                        cond_expr,
                        current_new.as_ref(),
                        old_row,
                        locals,
                        ctx.columns,
                        ctx.table_name,
                        ctx.params,
                        ctx.default_text_search_config,
                        ctx.select_into_resolver,
                    )
                    .map_err(|cause| TriggerError::EvalFailed {
                        function: ctx.function.into(),
                        cause,
                    })?;
                    if matches!(cond_val, Value::Bool(true)) {
                        matched = true;
                        match execute_stmts(body, current_new, old_row, locals, ctx, deferred)? {
                            BodyOutcome::FellThrough => {}
                            early => return Ok(early),
                        }
                        break;
                    }
                }
                if !matched && !else_branch.is_empty() {
                    match execute_stmts(else_branch, current_new, old_row, locals, ctx, deferred)? {
                        BodyOutcome::FellThrough => {}
                        early => return Ok(early),
                    }
                }
            }
            PlPgSqlStmt::Raise {
                level,
                message,
                args,
                errcode,
                detail,
                hint,
            } => {
                // Resolve every %-format placeholder by evaluating
                // each arg expression and rendering its Value.
                let mut rendered_args: Vec<String> = Vec::with_capacity(args.len());
                for a in args {
                    let v = eval_with_new_old_and_locals(
                        a,
                        current_new.as_ref(),
                        old_row,
                        locals,
                        ctx.columns,
                        ctx.table_name,
                        ctx.params,
                        ctx.default_text_search_config,
                        ctx.select_into_resolver,
                    )
                    .map_err(|cause| TriggerError::EvalFailed {
                        function: ctx.function.into(),
                        cause,
                    })?;
                    rendered_args.push(value_to_display_string(&v));
                }
                let mut resolved = format_raise_message(message, &rendered_args);
                // 9.0.0 — `USING DETAIL = …` / `HINT = …` ride in the
                // message, which is the shape the wire already splits
                // into the `D` and `H` fields.
                let mut eval_opt = |e: &Option<Expr>| -> Result<Option<String>, TriggerError> {
                    let Some(e) = e else { return Ok(None) };
                    let v = eval_with_new_old_and_locals(
                        e,
                        current_new.as_ref(),
                        old_row,
                        locals,
                        ctx.columns,
                        ctx.table_name,
                        ctx.params,
                        ctx.default_text_search_config,
                        ctx.select_into_resolver,
                    )
                    .map_err(|cause| TriggerError::EvalFailed {
                        function: ctx.function.into(),
                        cause,
                    })?;
                    Ok(Some(value_to_display_string(&v)))
                };
                if let Some(d) = eval_opt(detail)? {
                    resolved.push_str(" DETAIL: ");
                    resolved.push_str(&d);
                }
                if let Some(h) = eval_opt(hint)? {
                    resolved.push_str("\nHINT:  ");
                    resolved.push_str(&h);
                }
                if matches!(level, RaiseLevel::Exception) {
                    // 9.0.0 — the code the statement named, if it named
                    // one. A condition name resolves through the same
                    // table `EXCEPTION WHEN <name>` reads.
                    let named: Option<alloc::string::String> = match errcode {
                        None => None,
                        Some(spg_sql::ast::RaiseErrcode::State(code)) => {
                            Some(alloc::string::String::from(code))
                        }
                        Some(spg_sql::ast::RaiseErrcode::Condition(name)) => {
                            Some(alloc::string::String::from(name))
                        }
                        // `USING ERRCODE = <expr>` is evaluated, and its
                        // TEXT classified below the same way a written
                        // name is. Measured on PG 18.6.
                        Some(spg_sql::ast::RaiseErrcode::Value(e)) => eval_opt(&Some(e.clone()))?,
                    };
                    let sqlstate = match named {
                        None => None,
                        Some(text) => {
                            // Five characters of digits and upper-case
                            // letters is a code; anything else is a
                            // condition name.
                            if text.len() == 5
                                && text
                                    .bytes()
                                    .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
                            {
                                Some(text)
                            } else {
                                match condition_code(&text.to_ascii_lowercase()) {
                                    Some(code) => Some(alloc::string::String::from(code)),
                                    None => {
                                        return Err(TriggerError::Sql {
                                            function: ctx.function.into(),
                                            sqlstate: alloc::borrow::Cow::Borrowed("42704"),
                                            message: alloc::format!(
                                                "unrecognized exception condition \"{text}\""
                                            ),
                                        });
                                    }
                                }
                            }
                        }
                    };
                    return Err(TriggerError::RaiseException {
                        sqlstate,
                        function: ctx.function.into(),
                        message: resolved,
                    });
                }
                // v7.39 (round 757, F31-B3) — NOTICE / WARNING /
                // INFO reach the client (the round-753 audit found
                // them silently discarded here since v7.12.6); LOG
                // and DEBUG are server-log levels PG does not send
                // at the default client_min_messages.
                let severity = match level {
                    RaiseLevel::Notice => Some(crate::NoticeSeverity::Notice),
                    RaiseLevel::Warning => Some(crate::NoticeSeverity::Warning),
                    RaiseLevel::Info => Some(crate::NoticeSeverity::Info),
                    _ => None,
                };
                if let (Some(sev), Some(sink)) = (severity, ctx.notice_sink) {
                    sink.borrow_mut().push((sev, resolved));
                }
            }
            PlPgSqlStmt::SelectInto { var, body } => {
                // v7.16.2 — execute via the engine callback the
                // caller (Engine::exec_do_block) registered on
                // ctx, assign the result to the local. Trigger
                // path (no callback) errors loudly: SELECT INTO
                // doesn't fit in a row-write loop.
                let mut substituted = spg_sql::ast::Statement::Select((**body).clone());
                substitute_trigger_context_in_statement(
                    &mut substituted,
                    current_new.as_ref(),
                    old_row,
                    locals,
                    ctx.columns,
                )
                .map_err(|cause| TriggerError::EvalFailed {
                    function: ctx.function.into(),
                    cause,
                })?;
                let resolver =
                    ctx.select_into_resolver.ok_or_else(|| TriggerError::UnsupportedConstruct {
                        function: ctx.function.into(),
                        detail: alloc::format!(
                            "SELECT … INTO {var}: only supported inside DO blocks (not trigger bodies) in v7.16.2"
                        ),
                    })?;
                let value = resolver(&substituted)?;
                // v7.37.20 (20.15) — the PL/pgSQL FOUND special
                // variable is auto-set after each SQL-executing
                // statement. For SELECT INTO: `true` when the query
                // returned a row (value != Null), `false` otherwise.
                // The variable name is spelled lower-case per PG
                // convention; SPG's local map is case-preserving,
                // so callers reading `found` see this update.
                let found_after_select_into = !matches!(value, spg_storage::Value::Null);
                locals.insert(
                    "found".into(),
                    spg_storage::Value::Bool(found_after_select_into),
                );
                locals.insert(var.clone(), value);
            }
            PlPgSqlStmt::ForRange {
                var,
                start,
                end,
                reverse,
                body,
                label,
            } => {
                // v7.37.20 (20.4) — FOR <var> IN [REVERSE] <s>..<e> LOOP.
                const FOR_RANGE_BUDGET: i64 = 1_000_000;
                let s_v = eval_with_new_old_and_locals(
                    start,
                    current_new.as_ref(),
                    old_row,
                    locals,
                    ctx.columns,
                    ctx.table_name,
                    ctx.params,
                    ctx.default_text_search_config,
                    ctx.select_into_resolver,
                )
                .map_err(|cause| TriggerError::EvalFailed {
                    function: ctx.function.into(),
                    cause,
                })?;
                let e_v = eval_with_new_old_and_locals(
                    end,
                    current_new.as_ref(),
                    old_row,
                    locals,
                    ctx.columns,
                    ctx.table_name,
                    ctx.params,
                    ctx.default_text_search_config,
                    ctx.select_into_resolver,
                )
                .map_err(|cause| TriggerError::EvalFailed {
                    function: ctx.function.into(),
                    cause,
                })?;
                let to_i64 = |v: &spg_storage::Value<'static>| -> Result<i64, TriggerError> {
                    match v {
                        spg_storage::Value::Int(n) => Ok(i64::from(*n)),
                        spg_storage::Value::BigInt(n) => Ok(*n),
                        spg_storage::Value::SmallInt(n) => Ok(i64::from(*n)),
                        other => Err(TriggerError::UnsupportedConstruct {
                            function: ctx.function.into(),
                            detail: alloc::format!(
                                "FOR <var> IN start..end: bounds must be integer, got {}",
                                crate::conversions::pg_type_name_for_error_opt(other.data_type())
                            ),
                        }),
                    }
                };
                let s = to_i64(&s_v)?;
                let e = to_i64(&e_v)?;
                // PG's `FOR i IN REVERSE 5..1` iterates 5, 4, 3, 2, 1 —
                // the first bound is the start, the second is the end,
                // step is -1.
                let (lo, hi, step): (i64, i64, i64) = if *reverse { (s, e, -1) } else { (s, e, 1) };
                let mut i = lo;
                let mut iter: i64 = 0;
                loop {
                    if iter >= FOR_RANGE_BUDGET {
                        return Err(TriggerError::RaiseException {
                            sqlstate: None,
                            function: ctx.function.into(),
                            message: alloc::format!(
                                "FOR loop iteration budget {FOR_RANGE_BUDGET} reached"
                            ),
                        });
                    }
                    let cont = if *reverse { i >= hi } else { i <= hi };
                    if !cont {
                        break;
                    }
                    locals.insert(var.clone(), spg_storage::Value::BigInt(i));
                    match execute_stmts(body, current_new, old_row, locals, ctx, deferred)? {
                        BodyOutcome::FellThrough => {}
                        BodyOutcome::Continue(t)
                            if jump_is_mine(label.as_deref(), t.as_deref()) => {}
                        BodyOutcome::Break(t) if jump_is_mine(label.as_deref(), t.as_deref()) => {
                            break;
                        }
                        early => return Ok(early),
                    }
                    i = i.saturating_add(step);
                    iter += 1;
                }
            }
            PlPgSqlStmt::Loop { body, label } => {
                // v7.37.20 (20.2) — bare LOOP: iterate body until an
                // EXIT bubbles up, or the budget is exhausted.
                const LOOP_BUDGET: u64 = 1_000_000;
                let mut iter: u64 = 0;
                loop {
                    if iter >= LOOP_BUDGET {
                        return Err(TriggerError::RaiseException {
                            sqlstate: None,
                            function: ctx.function.into(),
                            message: alloc::format!("LOOP iteration budget {LOOP_BUDGET} reached"),
                        });
                    }
                    match execute_stmts(body, current_new, old_row, locals, ctx, deferred)? {
                        BodyOutcome::FellThrough => {}
                        BodyOutcome::Continue(t)
                            if jump_is_mine(label.as_deref(), t.as_deref()) => {}
                        BodyOutcome::Break(t) if jump_is_mine(label.as_deref(), t.as_deref()) => {
                            break;
                        }
                        early => return Ok(early),
                    }
                    iter += 1;
                }
            }
            PlPgSqlStmt::Exit { when, label } => {
                // v7.37.20 (20.2) — EXIT [WHEN <cond>]. Unconditional
                // exit or conditional (only breaks when truthy).
                let should_break = match when {
                    None => true,
                    Some(cond) => {
                        let v = eval_with_new_old_and_locals(
                            cond,
                            current_new.as_ref(),
                            old_row,
                            locals,
                            ctx.columns,
                            ctx.table_name,
                            ctx.params,
                            ctx.default_text_search_config,
                            ctx.select_into_resolver,
                        )
                        .map_err(|cause| TriggerError::EvalFailed {
                            function: ctx.function.into(),
                            cause,
                        })?;
                        matches!(v, spg_storage::Value::Bool(true))
                    }
                };
                if should_break {
                    return Ok(BodyOutcome::Break(label.clone()));
                }
            }
            PlPgSqlStmt::ForExecute {
                var,
                sql_expr,
                body,
                label,
            } => {
                // v7.37.20 (20.6) — FOR <var> IN EXECUTE <expr> LOOP.
                // Evaluate the expression at runtime to obtain a SQL
                // string, parse it, run through the for_query_resolver,
                // iterate rows same way ForQuery does.
                let resolver =
                    ctx.for_query_resolver
                        .ok_or_else(|| TriggerError::UnsupportedConstruct {
                            function: ctx.function.into(),
                            detail: alloc::format!(
                                "FOR <var> IN EXECUTE <expr> LOOP: only supported inside DO blocks"
                            ),
                        })?;
                let v = eval_with_new_old_and_locals(
                    sql_expr,
                    current_new.as_ref(),
                    old_row,
                    locals,
                    ctx.columns,
                    ctx.table_name,
                    ctx.params,
                    ctx.default_text_search_config,
                    ctx.select_into_resolver,
                )
                .map_err(|cause| TriggerError::EvalFailed {
                    function: ctx.function.into(),
                    cause,
                })?;
                let sql_text = match v {
                    spg_storage::Value::Text(s) => s.into_owned(),
                    other => {
                        return Err(TriggerError::UnsupportedConstruct {
                            function: ctx.function.into(),
                            detail: alloc::format!(
                                "FOR IN EXECUTE: expression must evaluate to TEXT, got {}",
                                crate::conversions::pg_type_name_for_error_opt(other.data_type())
                            ),
                        });
                    }
                };
                let stmt = spg_sql::parser::parse_statement(&sql_text).map_err(|e| {
                    TriggerError::UnparseableBody {
                        function: ctx.function.into(),
                        detail: alloc::format!(
                            "FOR IN EXECUTE {sql_text:?}: parse failed: {}",
                            e.message
                        ),
                    }
                })?;
                let (col_names, rows) = resolver(&stmt)?;
                for row_values in rows {
                    // v7.39 (read01 round 64) — bind the whole ROW: `rec` still
                    // carries the first cell (what a scalar loop variable
                    // means), and each column also lands as `rec.<col>` so a
                    // record variable's fields resolve.
                    for (i, cname) in col_names.iter().enumerate() {
                        locals.insert(
                            alloc::format!(
                                "{}.{}",
                                var.to_ascii_lowercase(),
                                cname.to_ascii_lowercase()
                            ),
                            row_values
                                .get(i)
                                .cloned()
                                .unwrap_or(spg_storage::Value::Null),
                        );
                    }
                    let first_cell = row_values
                        .into_iter()
                        .next()
                        .unwrap_or(spg_storage::Value::Null);
                    locals.insert(var.clone(), first_cell);
                    match execute_stmts(body, current_new, old_row, locals, ctx, deferred)? {
                        BodyOutcome::FellThrough => {}
                        BodyOutcome::Continue(t)
                            if jump_is_mine(label.as_deref(), t.as_deref()) => {}
                        BodyOutcome::Break(t) if jump_is_mine(label.as_deref(), t.as_deref()) => {
                            break;
                        }
                        early => return Ok(early),
                    }
                }
            }
            PlPgSqlStmt::ForQuery {
                var,
                query,
                body,
                label,
            } => {
                // v7.37.20 (20.5) — FOR <var> IN <SELECT> LOOP.
                // Runs the SELECT once via the DO block's registered
                // resolver, iterates rows, binds the first cell of
                // each row to `var` as a scalar Value. Full record
                // binding (var carrying all columns) queues with
                // v7.40 record type infrastructure.
                let resolver = ctx.for_query_resolver.ok_or_else(|| {
                    TriggerError::UnsupportedConstruct {
                        function: ctx.function.into(),
                        detail: alloc::format!(
                            "FOR <var> IN <SELECT> LOOP: only supported inside DO blocks in v7.37.20 (trigger paths queue with v7.40)"
                        ),
                    }
                })?;
                let mut stmt = spg_sql::ast::Statement::Select((**query).clone());
                substitute_trigger_context_in_statement(
                    &mut stmt,
                    current_new.as_ref(),
                    old_row,
                    locals,
                    ctx.columns,
                )
                .map_err(|cause| TriggerError::EvalFailed {
                    function: ctx.function.into(),
                    cause,
                })?;
                let (col_names, rows) = resolver(&stmt)?;
                for row_values in rows {
                    // Same record binding as FOR … IN <SELECT>.
                    for (i, cname) in col_names.iter().enumerate() {
                        locals.insert(
                            alloc::format!(
                                "{}.{}",
                                var.to_ascii_lowercase(),
                                cname.to_ascii_lowercase()
                            ),
                            row_values
                                .get(i)
                                .cloned()
                                .unwrap_or(spg_storage::Value::Null),
                        );
                    }
                    let first_cell = row_values
                        .into_iter()
                        .next()
                        .unwrap_or(spg_storage::Value::Null);
                    locals.insert(var.clone(), first_cell);
                    match execute_stmts(body, current_new, old_row, locals, ctx, deferred)? {
                        BodyOutcome::FellThrough => {}
                        BodyOutcome::Continue(t)
                            if jump_is_mine(label.as_deref(), t.as_deref()) => {}
                        BodyOutcome::Break(t) if jump_is_mine(label.as_deref(), t.as_deref()) => {
                            break;
                        }
                        early => return Ok(early),
                    }
                }
            }
            // v7.39 (read01 round 68) — `RETURN QUERY EXECUTE <sql>`: evaluate
            // the expression to a SQL string, run it through the same query
            // runner the static form uses, and append the rows to the set. It
            // used to run and DISCARD them.
            PlPgSqlStmt::ReturnQueryExecute { sql } => {
                let sink = ctx
                    .set_sink
                    .ok_or_else(|| TriggerError::UnsupportedConstruct {
                        function: ctx.function.into(),
                        detail: alloc::string::String::from(
                            "cannot use RETURN QUERY in a non-SETOF function",
                        ),
                    })?;
                let resolver =
                    ctx.for_query_resolver
                        .ok_or_else(|| TriggerError::UnsupportedConstruct {
                            function: ctx.function.into(),
                            detail: alloc::string::String::from(
                                "RETURN QUERY EXECUTE needs a query runner (this context has none)",
                            ),
                        })?;
                let sql_val = eval_with_new_old_and_locals(
                    sql,
                    current_new.as_ref(),
                    old_row,
                    locals,
                    ctx.columns,
                    ctx.table_name,
                    ctx.params,
                    ctx.default_text_search_config,
                    ctx.select_into_resolver,
                )
                .map_err(|cause| TriggerError::EvalFailed {
                    function: ctx.function.into(),
                    cause,
                })?;
                let Value::Text(text) = &sql_val else {
                    return Err(TriggerError::UnsupportedConstruct {
                        function: ctx.function.into(),
                        detail: alloc::format!(
                            "RETURN QUERY EXECUTE needs a text SQL string, got {}",
                            crate::conversions::pg_type_name_for_error_opt(sql_val.data_type())
                        ),
                    });
                };
                let stmt = spg_sql::parser::parse_statement(text.as_ref()).map_err(|e| {
                    TriggerError::UnparseableBody {
                        function: ctx.function.into(),
                        detail: alloc::format!("RETURN QUERY EXECUTE: {e}"),
                    }
                })?;
                let (_cols, rows) = resolver(&stmt)?;
                sink.borrow_mut().extend(rows);
            }
            PlPgSqlStmt::ExecuteDynamic { sql } => {
                // v7.37.20 (20.13) — EXECUTE <string_expr>. Evaluate
                // the expression at runtime to obtain a SQL string,
                // parse it, and queue for post-body execution the
                // same way EmbeddedSql does. USING <params> for
                // placeholder binding queues with v7.40 PL/pgSQL
                // epic.
                let v = eval_with_new_old_and_locals(
                    sql,
                    current_new.as_ref(),
                    old_row,
                    locals,
                    ctx.columns,
                    ctx.table_name,
                    ctx.params,
                    ctx.default_text_search_config,
                    ctx.select_into_resolver,
                )
                .map_err(|cause| TriggerError::EvalFailed {
                    function: ctx.function.into(),
                    cause,
                })?;
                let sql_text = match v {
                    spg_storage::Value::Text(s) => s.into_owned(),
                    other => {
                        return Err(TriggerError::UnsupportedConstruct {
                            function: ctx.function.into(),
                            detail: alloc::format!(
                                "EXECUTE <expr>: expression must evaluate to TEXT, got {}",
                                crate::conversions::pg_type_name_for_error_opt(other.data_type())
                            ),
                        });
                    }
                };
                let parsed = spg_sql::parser::parse_statement(&sql_text).map_err(|e| {
                    TriggerError::UnparseableBody {
                        function: ctx.function.into(),
                        detail: alloc::format!("EXECUTE {sql_text:?}: parse failed: {}", e.message),
                    }
                })?;
                if let Some(write) = ctx.write_resolver {
                    // 8.0.3 — dynamic SQL runs in place too, inside the
                    // block's EXCEPTION clause, like every embedded statement.
                    write(&parsed)?;
                } else {
                    deferred.push(DeferredEmbeddedStmt {
                        function: ctx.function.into(),
                        stmt: parsed,
                    });
                }
            }
            PlPgSqlStmt::Continue { when, label } => {
                // v7.37.20 (20.2) — CONTINUE [WHEN <cond>]. Same shape
                // as EXIT but signals BodyOutcome::Continue.
                let should_continue = match when {
                    None => true,
                    Some(cond) => {
                        let v = eval_with_new_old_and_locals(
                            cond,
                            current_new.as_ref(),
                            old_row,
                            locals,
                            ctx.columns,
                            ctx.table_name,
                            ctx.params,
                            ctx.default_text_search_config,
                            ctx.select_into_resolver,
                        )
                        .map_err(|cause| TriggerError::EvalFailed {
                            function: ctx.function.into(),
                            cause,
                        })?;
                        matches!(v, spg_storage::Value::Bool(true))
                    }
                };
                if should_continue {
                    return Ok(BodyOutcome::Continue(label.clone()));
                }
            }
            PlPgSqlStmt::While {
                condition,
                body,
                label,
            } => {
                // v7.37.20 (20.3) — WHILE <cond> LOOP iteration.
                // Iteration count bounded by a generous budget so a
                // mis-spelled condition can't lock the engine. The
                // budget matches the v7.12.6 trigger-recursion cap
                // shape (~1M iterations).
                const WHILE_LOOP_BUDGET: u64 = 1_000_000;
                let mut iter: u64 = 0;
                loop {
                    if iter >= WHILE_LOOP_BUDGET {
                        return Err(TriggerError::RaiseException {
                            sqlstate: None,
                            function: ctx.function.into(),
                            message: alloc::format!(
                                "WHILE loop iteration budget {WHILE_LOOP_BUDGET} reached — likely runaway condition"
                            ),
                        });
                    }
                    let v = eval_with_new_old_and_locals(
                        condition,
                        current_new.as_ref(),
                        old_row,
                        locals,
                        ctx.columns,
                        ctx.table_name,
                        ctx.params,
                        ctx.default_text_search_config,
                        ctx.select_into_resolver,
                    )
                    .map_err(|cause| TriggerError::EvalFailed {
                        function: ctx.function.into(),
                        cause,
                    })?;
                    if !matches!(v, spg_storage::Value::Bool(true)) {
                        break;
                    }
                    // Re-enter the trigger body interpreter on `body`.
                    // Recursive call shares the same `ctx`,
                    // `current_new`, `old_row`, `locals`, `deferred`
                    // so any Assign / RAISE / EmbeddedSql side effect
                    // inside the loop propagates back the same way
                    // the IF / ELSE arms do.
                    match execute_stmts(body, current_new, old_row, locals, ctx, deferred)? {
                        BodyOutcome::FellThrough => {}
                        BodyOutcome::Continue(t)
                            if jump_is_mine(label.as_deref(), t.as_deref()) => {}
                        BodyOutcome::Break(t) if jump_is_mine(label.as_deref(), t.as_deref()) => {
                            break;
                        }
                        early => return Ok(early),
                    }
                    iter += 1;
                }
            }
            PlPgSqlStmt::Assert { condition, message } => {
                // v7.37.20 (20.14) — ASSERT <cond> [, <msg>]. If
                // the condition evaluates to a falsy Value (NULL or
                // BOOL(false)), raise the same EngineError shape as
                // RAISE EXCEPTION. Otherwise no-op.
                let v = eval_with_new_old_and_locals(
                    condition,
                    current_new.as_ref(),
                    old_row,
                    locals,
                    ctx.columns,
                    ctx.table_name,
                    ctx.params,
                    ctx.default_text_search_config,
                    ctx.select_into_resolver,
                )
                .map_err(|cause| TriggerError::EvalFailed {
                    function: ctx.function.into(),
                    cause,
                })?;
                let cond_holds = matches!(v, spg_storage::Value::Bool(true));
                if !cond_holds {
                    let msg_text = if let Some(m) = message {
                        let mv = eval_with_new_old_and_locals(
                            m,
                            current_new.as_ref(),
                            old_row,
                            locals,
                            ctx.columns,
                            ctx.table_name,
                            ctx.params,
                            ctx.default_text_search_config,
                            ctx.select_into_resolver,
                        )
                        .map_err(|cause| TriggerError::EvalFailed {
                            function: ctx.function.into(),
                            cause,
                        })?;
                        value_to_display_string(&mv)
                    } else {
                        alloc::string::String::from("assertion failed")
                    };
                    // 8.0.3 — P0004 ASSERT_FAILURE, not P0001: PG gives a
                    // failed ASSERT its own code, and deliberately keeps
                    // `EXCEPTION WHEN OTHERS` from catching it — an assert
                    // is a statement about the program, not a runtime
                    // condition to recover from.
                    return Err(TriggerError::Sql {
                        function: ctx.function.into(),
                        sqlstate: alloc::borrow::Cow::Borrowed("P0004"),
                        message: msg_text,
                    });
                }
            }
            PlPgSqlStmt::EmbeddedSql(boxed_stmt) => {
                // v7.12.7 — substitute NEW/OLD/locals into every
                // Expr field of the statement, then queue for
                // post-DML execution. The trigger interpreter
                // doesn't call back into Engine::execute directly
                // (that would deadlock the row-write mut borrow);
                // the engine drains `deferred` after the firing
                // INSERT/UPDATE/DELETE completes its main work.
                let mut substituted = (**boxed_stmt).clone();
                substitute_trigger_context_in_statement(
                    &mut substituted,
                    current_new.as_ref(),
                    old_row,
                    locals,
                    ctx.columns,
                )
                .map_err(|cause| TriggerError::EvalFailed {
                    function: ctx.function.into(),
                    cause,
                })?;
                if let Some(write) = ctx.write_resolver {
                    write(&substituted)?;
                } else {
                    deferred.push(DeferredEmbeddedStmt {
                        function: ctx.function.into(),
                        stmt: substituted,
                    });
                }
            }
        }
    }
    Ok(BodyOutcome::FellThrough)
}

/// 8.0.3 — the SQLSTATE and the `SQLERRM` text of an error a PL/pgSQL
/// block raised, from the same classification the wire uses.
pub(crate) fn error_state(err: &TriggerError) -> (alloc::borrow::Cow<'static, str>, String) {
    match err {
        // 9.0.0 — a RAISE may name its own code.
        TriggerError::RaiseException {
            message, sqlstate, ..
        } => (
            sqlstate
                .clone()
                .map_or(alloc::borrow::Cow::Borrowed("P0001"), Into::into),
            message.clone(),
        ),
        TriggerError::Sql {
            sqlstate, message, ..
        } => (sqlstate.clone(), message.clone()),
        TriggerError::EvalFailed { cause, .. } => {
            crate::sqlstate::error_to_wire(&crate::EngineError::Eval(cause.clone()))
        }
        other => crate::sqlstate::error_to_wire(&crate::EngineError::Unsupported(alloc::format!(
            "{other}"
        ))),
    }
}

/// 8.0.3 — PG refuses a block naming a condition it does not know, before
/// any of the block runs: `unrecognized exception condition "foo"`, 42704.
/// SPG used to accept any word and match it against the RAISE message by
/// substring, so a misspelt handler silently never fired.
pub(crate) fn check_exception_conditions(
    function: &str,
    block: &spg_sql::ast::PlPgSqlBlock,
) -> Result<(), TriggerError> {
    for h in &block.exception_handlers {
        for c in &h.conditions {
            let known = c.eq_ignore_ascii_case("others")
                || c.starts_with("sqlstate:")
                || condition_code(c).is_some();
            if !known {
                return Err(TriggerError::Sql {
                    function: function.into(),
                    sqlstate: alloc::borrow::Cow::Borrowed("42704"),
                    message: alloc::format!("unrecognized exception condition \"{c}\""),
                });
            }
        }
        // 9.0.0 — a nested block inside a handler body carries handlers
        // of its own.
        check_nested_exception_conditions(function, &h.body)?;
    }
    check_nested_exception_conditions(function, &block.statements)
}

/// 9.0.0 — the same check over every nested block a statement list holds.
fn check_nested_exception_conditions(
    function: &str,
    stmts: &[PlPgSqlStmt],
) -> Result<(), TriggerError> {
    for s in stmts {
        if let PlPgSqlStmt::Block(b) = s {
            check_exception_conditions(function, b)?;
        }
    }
    Ok(())
}

/// 8.0.3 — the code an embedded write reports when it has to WAIT for
/// another transaction. Not a PostgreSQL error at all but the engine's
/// signal to the host to retry the statement, so no handler — `OTHERS`
/// included — may swallow it; swallowing it would turn a wait into a
/// write that silently did not happen.
pub(crate) const INTERNAL_WAIT_SQLSTATE: &str = "SPGWT";

/// 8.0.3 — does an `EXCEPTION WHEN <condition>` arm catch `sqlstate`?
///
/// The names are PostgreSQL's documented condition names (its manual's
/// error-code appendix), each standing for a code; a name whose code ends
/// in `000` names the whole class, so `integrity_constraint_violation`
/// catches a `23505`. `SQLSTATE 'xxxxx'` names one code directly. `OTHERS`
/// catches everything except a cancel and a failed `ASSERT`, which PG
/// deliberately lets escape.
///
/// This used to match a condition against the RAISE message by substring,
/// and only for a RAISE — so no error a statement or an expression raised
/// could be caught at all, `OTHERS` included.
pub(crate) fn condition_matches(condition: &str, sqlstate: &str) -> bool {
    if let Some(code) = condition.strip_prefix("sqlstate:") {
        return code.eq_ignore_ascii_case(sqlstate);
    }
    if sqlstate == INTERNAL_WAIT_SQLSTATE {
        return false;
    }
    if condition.eq_ignore_ascii_case("others") {
        return sqlstate != "57014" && sqlstate != "P0004";
    }
    let Some(code) = condition_code(condition) else {
        return false;
    };
    if let Some(class) = code.strip_suffix("000") {
        return sqlstate.starts_with(class);
    }
    code == sqlstate
}

fn condition_code(name: &str) -> Option<&'static str> {
    // Every condition name PostgreSQL 18.6 accepts, with the code it
    // stands for — 247 of them. Not transcribed: each pair was
    // MEASURED, by `RAISE <name>` inside a block whose handler reports
    // `SQLSTATE`. A partial list would refuse a name PG accepts, which is
    // why the unrecognized-name check below needs the whole of it.
    const NAMES: &[(&str, &str)] = &[
        ("active_sql_transaction", "25001"),
        ("admin_shutdown", "57P01"),
        ("ambiguous_alias", "42P09"),
        ("ambiguous_column", "42702"),
        ("ambiguous_function", "42725"),
        ("ambiguous_parameter", "42P08"),
        ("array_subscript_error", "2202E"),
        ("assert_failure", "P0004"),
        ("bad_copy_file_format", "22P04"),
        ("branch_transaction_already_active", "25002"),
        ("cannot_coerce", "42846"),
        ("cannot_connect_now", "57P03"),
        ("cant_change_runtime_param", "55P02"),
        ("cardinality_violation", "21000"),
        ("case_not_found", "20000"),
        ("character_not_in_repertoire", "22021"),
        ("check_violation", "23514"),
        ("collation_mismatch", "42P21"),
        ("config_file_error", "F0000"),
        ("configuration_limit_exceeded", "53400"),
        ("connection_does_not_exist", "08003"),
        ("connection_exception", "08000"),
        ("connection_failure", "08006"),
        ("containing_sql_not_permitted", "38001"),
        ("crash_shutdown", "57P02"),
        ("data_corrupted", "XX001"),
        ("data_exception", "22000"),
        ("database_dropped", "57P04"),
        ("datatype_mismatch", "42804"),
        ("datetime_field_overflow", "22008"),
        ("deadlock_detected", "40P01"),
        ("dependent_objects_still_exist", "2BP01"),
        ("dependent_privilege_descriptors_still_exist", "2B000"),
        ("diagnostics_exception", "0Z000"),
        ("disk_full", "53100"),
        ("division_by_zero", "22012"),
        ("duplicate_alias", "42712"),
        ("duplicate_column", "42701"),
        ("duplicate_cursor", "42P03"),
        ("duplicate_database", "42P04"),
        ("duplicate_file", "58P02"),
        ("duplicate_function", "42723"),
        ("duplicate_json_object_key_value", "22030"),
        ("duplicate_object", "42710"),
        ("duplicate_prepared_statement", "42P05"),
        ("duplicate_schema", "42P06"),
        ("duplicate_table", "42P07"),
        ("error_in_assignment", "22005"),
        ("escape_character_conflict", "2200B"),
        ("event_trigger_protocol_violated", "39P03"),
        ("exclusion_violation", "23P01"),
        ("external_routine_exception", "38000"),
        ("external_routine_invocation_exception", "39000"),
        ("fdw_column_name_not_found", "HV005"),
        ("fdw_dynamic_parameter_value_needed", "HV002"),
        ("fdw_error", "HV000"),
        ("fdw_function_sequence_error", "HV010"),
        ("fdw_inconsistent_descriptor_information", "HV021"),
        ("fdw_invalid_attribute_value", "HV024"),
        ("fdw_invalid_column_name", "HV007"),
        ("fdw_invalid_column_number", "HV008"),
        ("fdw_invalid_data_type", "HV004"),
        ("fdw_invalid_data_type_descriptors", "HV006"),
        ("fdw_invalid_descriptor_field_identifier", "HV091"),
        ("fdw_invalid_handle", "HV00B"),
        ("fdw_invalid_option_index", "HV00C"),
        ("fdw_invalid_option_name", "HV00D"),
        ("fdw_invalid_string_format", "HV00A"),
        ("fdw_invalid_string_length_or_buffer_length", "HV090"),
        ("fdw_invalid_use_of_null_pointer", "HV009"),
        ("fdw_no_schemas", "HV00P"),
        ("fdw_option_name_not_found", "HV00J"),
        ("fdw_out_of_memory", "HV001"),
        ("fdw_reply_handle", "HV00K"),
        ("fdw_schema_not_found", "HV00Q"),
        ("fdw_table_not_found", "HV00R"),
        ("fdw_too_many_handles", "HV014"),
        ("fdw_unable_to_create_execution", "HV00L"),
        ("fdw_unable_to_create_reply", "HV00M"),
        ("fdw_unable_to_establish_connection", "HV00N"),
        ("feature_not_supported", "0A000"),
        ("file_name_too_long", "58P03"),
        ("floating_point_exception", "22P01"),
        ("foreign_key_violation", "23503"),
        ("function_executed_no_return_statement", "2F005"),
        ("generated_always", "428C9"),
        ("grouping_error", "42803"),
        ("held_cursor_requires_same_isolation_level", "25008"),
        ("idle_in_transaction_session_timeout", "25P03"),
        ("idle_session_timeout", "57P05"),
        ("in_failed_sql_transaction", "25P02"),
        ("inappropriate_access_mode_for_branch_transaction", "25003"),
        (
            "inappropriate_isolation_level_for_branch_transaction",
            "25004",
        ),
        ("indeterminate_collation", "42P22"),
        ("indeterminate_datatype", "42P18"),
        ("index_corrupted", "XX002"),
        ("indicator_overflow", "22022"),
        ("insufficient_privilege", "42501"),
        ("insufficient_resources", "53000"),
        ("integrity_constraint_violation", "23000"),
        ("internal_error", "XX000"),
        ("interval_field_overflow", "22015"),
        ("invalid_argument_for_logarithm", "2201E"),
        ("invalid_argument_for_nth_value_function", "22016"),
        ("invalid_argument_for_ntile_function", "22014"),
        ("invalid_argument_for_power_function", "2201F"),
        ("invalid_argument_for_sql_json_datetime_function", "22031"),
        ("invalid_argument_for_width_bucket_function", "2201G"),
        ("invalid_argument_for_xquery", "10608"),
        ("invalid_authorization_specification", "28000"),
        ("invalid_binary_representation", "22P03"),
        ("invalid_catalog_name", "3D000"),
        ("invalid_character_value_for_cast", "22018"),
        ("invalid_column_definition", "42611"),
        ("invalid_column_reference", "42P10"),
        ("invalid_cursor_definition", "42P11"),
        ("invalid_cursor_name", "34000"),
        ("invalid_cursor_state", "24000"),
        ("invalid_database_definition", "42P12"),
        ("invalid_datetime_format", "22007"),
        ("invalid_escape_character", "22019"),
        ("invalid_escape_octet", "2200D"),
        ("invalid_escape_sequence", "22025"),
        ("invalid_foreign_key", "42830"),
        ("invalid_function_definition", "42P13"),
        ("invalid_grant_operation", "0LP01"),
        ("invalid_grantor", "0L000"),
        ("invalid_indicator_parameter_value", "22010"),
        ("invalid_json_text", "22032"),
        ("invalid_locator_specification", "0F001"),
        ("invalid_name", "42602"),
        ("invalid_object_definition", "42P17"),
        ("invalid_parameter_value", "22023"),
        ("invalid_password", "28P01"),
        ("invalid_preceding_or_following_size", "22013"),
        ("invalid_prepared_statement_definition", "42P14"),
        ("invalid_recursion", "42P19"),
        ("invalid_regular_expression", "2201B"),
        ("invalid_role_specification", "0P000"),
        ("invalid_row_count_in_limit_clause", "2201W"),
        ("invalid_row_count_in_result_offset_clause", "2201X"),
        ("invalid_savepoint_specification", "3B001"),
        ("invalid_schema_definition", "42P15"),
        ("invalid_schema_name", "3F000"),
        ("invalid_sql_json_subscript", "22033"),
        ("invalid_sql_statement_name", "26000"),
        ("invalid_sqlstate_returned", "39001"),
        ("invalid_table_definition", "42P16"),
        ("invalid_tablesample_argument", "2202H"),
        ("invalid_tablesample_repeat", "2202G"),
        ("invalid_text_representation", "22P02"),
        ("invalid_time_zone_displacement_value", "22009"),
        ("invalid_transaction_initiation", "0B000"),
        ("invalid_transaction_state", "25000"),
        ("invalid_transaction_termination", "2D000"),
        ("invalid_use_of_escape_character", "2200C"),
        ("invalid_xml_comment", "2200S"),
        ("invalid_xml_content", "2200N"),
        ("invalid_xml_document", "2200M"),
        ("invalid_xml_processing_instruction", "2200T"),
        ("io_error", "58030"),
        ("locator_exception", "0F000"),
        ("lock_file_exists", "F0001"),
        ("lock_not_available", "55P03"),
        ("modifying_sql_data_not_permitted", "2F002"),
        ("more_than_one_sql_json_item", "22034"),
        ("most_specific_type_mismatch", "2200G"),
        ("name_too_long", "42622"),
        ("no_active_sql_transaction", "25P01"),
        ("no_active_sql_transaction_for_branch_transaction", "25005"),
        ("no_data_found", "P0002"),
        ("no_sql_json_item", "22035"),
        ("non_numeric_sql_json_item", "22036"),
        ("non_unique_keys_in_a_json_object", "22037"),
        ("nonstandard_use_of_escape_character", "22P06"),
        ("not_an_xml_document", "2200L"),
        ("not_null_violation", "23502"),
        ("null_value_no_indicator_parameter", "22002"),
        ("null_value_not_allowed", "22004"),
        ("numeric_value_out_of_range", "22003"),
        ("object_in_use", "55006"),
        ("object_not_in_prerequisite_state", "55000"),
        ("operator_intervention", "57000"),
        ("out_of_memory", "53200"),
        ("plpgsql_error", "P0000"),
        ("program_limit_exceeded", "54000"),
        ("prohibited_sql_statement_attempted", "2F003"),
        ("protocol_violation", "08P01"),
        ("query_canceled", "57014"),
        ("raise_exception", "P0001"),
        ("read_only_sql_transaction", "25006"),
        ("reading_sql_data_not_permitted", "2F004"),
        ("reserved_name", "42939"),
        ("restrict_violation", "23001"),
        ("savepoint_exception", "3B000"),
        ("schema_and_data_statement_mixing_not_supported", "25007"),
        ("sequence_generator_limit_exceeded", "2200H"),
        ("serialization_failure", "40001"),
        ("singleton_sql_json_item_required", "22038"),
        ("sql_json_array_not_found", "22039"),
        ("sql_json_item_cannot_be_cast_to_target_type", "2203G"),
        ("sql_json_member_not_found", "2203A"),
        ("sql_json_number_not_found", "2203B"),
        ("sql_json_object_not_found", "2203C"),
        ("sql_json_scalar_required", "2203F"),
        ("sql_routine_exception", "2F000"),
        ("sql_statement_not_yet_complete", "03000"),
        ("sqlclient_unable_to_establish_sqlconnection", "08001"),
        ("sqlserver_rejected_establishment_of_sqlconnection", "08004"),
        ("srf_protocol_violated", "39P02"),
        (
            "stacked_diagnostics_accessed_without_active_handler",
            "0Z002",
        ),
        ("statement_completion_unknown", "40003"),
        ("statement_too_complex", "54001"),
        ("string_data_length_mismatch", "22026"),
        ("string_data_right_truncation", "22001"),
        ("substring_error", "22011"),
        ("syntax_error", "42601"),
        ("syntax_error_or_access_rule_violation", "42000"),
        ("system_error", "58000"),
        ("too_many_arguments", "54023"),
        ("too_many_columns", "54011"),
        ("too_many_connections", "53300"),
        ("too_many_json_array_elements", "2203D"),
        ("too_many_json_object_members", "2203E"),
        ("too_many_rows", "P0003"),
        ("transaction_integrity_constraint_violation", "40002"),
        ("transaction_resolution_unknown", "08007"),
        ("transaction_rollback", "40000"),
        ("transaction_timeout", "25P04"),
        ("trigger_protocol_violated", "39P01"),
        ("triggered_action_exception", "09000"),
        ("triggered_data_change_violation", "27000"),
        ("trim_error", "22027"),
        ("undefined_column", "42703"),
        ("undefined_file", "58P01"),
        ("undefined_function", "42883"),
        ("undefined_object", "42704"),
        ("undefined_parameter", "42P02"),
        ("undefined_table", "42P01"),
        ("unique_violation", "23505"),
        ("unsafe_new_enum_value_usage", "55P04"),
        ("unterminated_c_string", "22024"),
        ("untranslatable_character", "22P05"),
        ("windowing_error", "42P20"),
        ("with_check_option_violation", "44000"),
        ("wrong_object_type", "42809"),
        ("zero_length_character_string", "2200F"),
    ];
    NAMES
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, c)| *c)
}

/// v7.16.2 — execute a DO block's PlPgSqlBlock at top level.
/// Different from `fire_row_trigger` in three ways:
///   1. No NEW/OLD row context — DO blocks aren't row-scoped.
///   2. EmbeddedSql statements collected into the returned vec
///      so the caller (`Engine::exec_do_block`) can dispatch
///      them via `Engine::execute_in_with_cancel` IMMEDIATELY,
///      not defer. Triggers defer because they fire inside a
///      row-write `&mut Catalog` borrow; DO has no such borrow.
///   3. Embedded condition Expr (e.g. `IF EXISTS (SELECT ...)`)
///      evaluation happens inline against the engine's
///      current state — the caller resolves the subquery
///      result before walking the body. We do that by
///      collecting the IF / Assign / RAISE statements and
///      letting the caller-side evaluator decide; v7.16.2's
///      simple path lets `eval_with_new_old_and_locals` do
///      it inline, falling back to the embedded sub-engine
///      for SELECT subqueries via the regular eval path.
///
/// Returns the deferred SQL list in execution order. Errors
/// from the walk propagate verbatim (parse / eval / engine).
pub fn execute_do_block_top_level<'a>(
    block: &spg_sql::ast::PlPgSqlBlock,
    default_text_search_config: Option<&'a str>,
    select_into_resolver: Option<&'a SelectIntoResolver<'a>>,
    for_query_resolver: Option<&'a ForQueryResolver<'a>>,
    notice_sink: Option<&'a NoticeSink>,
) -> Result<Vec<spg_sql::ast::Statement>, TriggerError> {
    do_block(
        block,
        default_text_search_config,
        select_into_resolver,
        for_query_resolver,
        notice_sink,
        None,
        None,
    )
}

/// 8.0.3 — a DO block whose writes run in place, inside a savepoint when
/// the block has an `EXCEPTION` clause. What the engine uses; the public
/// function above keeps its signature and its deferring behaviour.
pub(crate) fn execute_do_block_live<'a>(
    block: &spg_sql::ast::PlPgSqlBlock,
    default_text_search_config: Option<&'a str>,
    select_into_resolver: Option<&'a SelectIntoResolver<'a>>,
    for_query_resolver: Option<&'a ForQueryResolver<'a>>,
    notice_sink: Option<&'a NoticeSink>,
    write_resolver: &'a WriteResolver<'a>,
    savepoint: &'a dyn Fn(BlockSavepoint),
) -> Result<(), TriggerError> {
    do_block(
        block,
        default_text_search_config,
        select_into_resolver,
        for_query_resolver,
        notice_sink,
        Some(write_resolver),
        Some(savepoint),
    )
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
fn do_block<'a>(
    block: &spg_sql::ast::PlPgSqlBlock,
    default_text_search_config: Option<&'a str>,
    select_into_resolver: Option<&'a SelectIntoResolver<'a>>,
    for_query_resolver: Option<&'a ForQueryResolver<'a>>,
    notice_sink: Option<&'a NoticeSink>,
    write_resolver: Option<&'a WriteResolver<'a>>,
    savepoint: Option<&'a dyn Fn(BlockSavepoint)>,
) -> Result<Vec<spg_sql::ast::Statement>, TriggerError> {
    // A DO block returns nothing, so RETURN NEXT / RETURN QUERY have nowhere to
    // go — PG rejects them there too.
    check_exception_conditions("DO", block)?;
    let set_sink: Option<&core::cell::RefCell<Vec<Vec<Value<'static>>>>> = None;
    let mut locals: BTreeMap<String, Value<'static>> = BTreeMap::new();
    let empty_cols: &[ColumnSchema] = &[];
    init_locals_from_declarations(
        &block.declarations,
        &mut locals,
        None,
        None,
        empty_cols,
        "",
        &[],
        default_text_search_config,
        "DO",
        select_into_resolver,
    )?;
    let ctx = BodyCtx {
        function: "DO",
        table_name: "",
        columns: empty_cols,
        params: &[],
        default_text_search_config,
        is_after: false,
        select_into_resolver,
        notice_sink,
        for_query_resolver,
        set_sink,
        write_resolver,
        savepoint,
    };
    let mut current_new: Option<Row> = None;
    let mut deferred: Vec<DeferredEmbeddedStmt> = Vec::new();
    let protected = !block.exception_handlers.is_empty();
    if protected && let Some(sp) = savepoint {
        sp(BlockSavepoint::Take);
    }
    // RETURN inside a DO is a no-op by PG semantics: the block's outer
    // scope has no return contract.
    let body_result = execute_stmts(
        &block.statements,
        &mut current_new,
        None,
        &mut locals,
        &ctx,
        &mut deferred,
    );
    if let Err(err) = body_result {
        // 8.0.3 — any error the block raised, matched by SQLSTATE. PG
        // rolls the block back to where it began before the handler runs,
        // and keeps the variables as they stood at the error.
        let (sqlstate, message) = error_state(&err);
        let handler = block
            .exception_handlers
            .iter()
            .find(|h| h.conditions.iter().any(|c| condition_matches(c, &sqlstate)));
        let Some(handler) = handler else {
            return Err(err);
        };
        if let Some(sp) = savepoint {
            sp(BlockSavepoint::RollBack);
        }
        deferred.clear();
        locals.insert("sqlerrm".into(), Value::text(message));
        locals.insert(
            "sqlstate".into(),
            Value::text(alloc::string::String::from(sqlstate)),
        );
        // A handler that itself raises propagates as the new error.
        execute_stmts(
            &handler.body,
            &mut current_new,
            None,
            &mut locals,
            &ctx,
            &mut deferred,
        )?;
    }
    Ok(deferred.into_iter().map(|d| d.stmt).collect())
}

/// v7.39 (read01 round 64) — run a plpgsql body as a SCALAR function: the same
/// interpreter the DO block and the triggers use, with no NEW / OLD, the
/// arguments pre-bound as locals, and `RETURN <expr>` actually EVALUATED (the
/// trigger path discards it — `resolve_return`'s own comment said "the scalar
/// UDF surface in a later release handles RETURN <expr> properly").
///
/// `Ok(None)` means the body fell out of the bottom without returning, which PG
/// reports as an error for a non-void function; the caller phrases it.
///
/// A body that WRITES (an embedded INSERT / UPDATE / DELETE) cannot run here:
/// the call arrives through expression evaluation, which holds the engine
/// immutably. Those `deferred` statements are refused rather than dropped —
/// silently discarding a write would be the worst possible answer.
pub fn call_plpgsql_scalar<'a>(
    function: &str,
    block: &spg_sql::ast::PlPgSqlBlock,
    args: BTreeMap<String, Value<'static>>,
    default_text_search_config: Option<&'a str>,
    select_into_resolver: Option<&'a SelectIntoResolver<'a>>,
    for_query_resolver: Option<&'a ForQueryResolver<'a>>,
    // v7.39 (read01 round 66) — where `RETURN NEXT` / `RETURN QUERY` append.
    // `Some` when the function is SETOF; the caller reads the rows out of it.
    set_sink: Option<&'a core::cell::RefCell<Vec<Vec<Value<'static>>>>>,
    // v7.39 (round 757, F31-B3) — see [`NoticeSink`]. The SELECT-path
    // caller passes `None` (immutable engine borrow; B3 residual).
    notice_sink: Option<&'a NoticeSink>,
) -> Result<Option<Value<'static>>, TriggerError> {
    check_exception_conditions(function, block)?;
    let mut locals: BTreeMap<String, Value<'static>> = args;
    let empty_cols: &[ColumnSchema] = &[];
    // The DECLARE block runs AFTER the arguments are bound, so an initialiser
    // may reference them (`DECLARE y int := x * 2;`).
    init_locals_from_declarations(
        &block.declarations,
        &mut locals,
        None,
        None,
        empty_cols,
        "",
        &[],
        default_text_search_config,
        function,
        select_into_resolver,
    )?;
    let ctx = BodyCtx {
        function,
        table_name: "",
        columns: empty_cols,
        params: &[],
        default_text_search_config,
        is_after: false,
        select_into_resolver,
        notice_sink,
        for_query_resolver,
        set_sink,
        write_resolver: None,
        savepoint: None,
    };
    let mut current_new: Option<Row> = None;
    let mut deferred: Vec<DeferredEmbeddedStmt> = Vec::new();
    let mut outcome = execute_stmts(
        &block.statements,
        &mut current_new,
        None,
        &mut locals,
        &ctx,
        &mut deferred,
    );
    // An EXCEPTION handler catches a RAISE, exactly as in a DO block.
    if let Err(err) = outcome {
        let mut handled = None;
        if !block.exception_handlers.is_empty() {
            let (sqlstate, message) = error_state(&err);
            for handler in &block.exception_handlers {
                let matches = handler
                    .conditions
                    .iter()
                    .any(|c| condition_matches(c, &sqlstate));
                if matches {
                    locals.insert("sqlerrm".into(), Value::text(message.clone()));
                    locals.insert(
                        "sqlstate".into(),
                        Value::text(alloc::string::String::from(sqlstate)),
                    );
                    handled = Some(execute_stmts(
                        &handler.body,
                        &mut current_new,
                        None,
                        &mut locals,
                        &ctx,
                        &mut deferred,
                    )?);
                    break;
                }
            }
        }
        match handled {
            Some(o) => outcome = Ok(o),
            None => return Err(err),
        }
    }
    if !deferred.is_empty() {
        return Err(TriggerError::UnsupportedConstruct {
            function: function.into(),
            detail: alloc::string::String::from(
                "a plpgsql function body that writes (INSERT / UPDATE / DELETE) \
                 cannot be called from an expression",
            ),
        });
    }
    match outcome.expect("error paths returned above") {
        BodyOutcome::Return(ReturnTarget::Expr(e)) => {
            let v = eval_with_new_old_and_locals(
                &e,
                None,
                None,
                &locals,
                empty_cols,
                "",
                &[],
                default_text_search_config,
                ctx.select_into_resolver,
            )
            .map_err(|cause| TriggerError::EvalFailed {
                function: function.into(),
                cause,
            })?;
            Ok(Some(v))
        }
        BodyOutcome::Return(ReturnTarget::Null) => Ok(Some(Value::Null)),
        BodyOutcome::Return(_) => Err(TriggerError::UnsupportedConstruct {
            function: function.into(),
            detail: alloc::string::String::from("RETURN NEW / OLD is only meaningful in a trigger"),
        }),
        _ => Ok(None),
    }
}

fn resolve_return(
    target: ReturnTarget,
    current_new: Option<Row<'static>>,
    old_row: Option<&Row<'static>>,
) -> TriggerOutcome {
    match target {
        ReturnTarget::New => current_new.map_or(TriggerOutcome::Skip, TriggerOutcome::Row),
        ReturnTarget::Old => old_row
            .cloned()
            .map_or(TriggerOutcome::Skip, TriggerOutcome::Row),
        ReturnTarget::Null => TriggerOutcome::Skip,
        // The scalar UDF surface in a later release handles
        // RETURN <expr> properly; for now we fall through to Skip.
        ReturnTarget::Expr(_) => TriggerOutcome::Skip,
    }
}

#[allow(clippy::too_many_arguments)]
fn init_locals_from_declarations(
    decls: &[PlPgSqlDeclare],
    locals: &mut BTreeMap<String, Value>,
    new_row: Option<&Row<'static>>,
    old_row: Option<&Row<'static>>,
    columns: &[ColumnSchema],
    table_name: &str,
    params: &[Value<'static>],
    default_text_search_config: Option<&str>,
    function_name: &str,
    subquery_resolver: Option<&SelectIntoResolver<'_>>,
) -> Result<(), TriggerError> {
    for d in decls {
        let v = if let Some(init) = &d.default {
            eval_with_new_old_and_locals(
                init,
                new_row,
                old_row,
                locals,
                columns,
                table_name,
                params,
                default_text_search_config,
                subquery_resolver,
            )
            .map_err(|cause| TriggerError::EvalFailed {
                function: function_name.into(),
                cause,
            })?
        } else {
            Value::Null
        };
        locals.insert(d.name.clone(), v);
    }
    Ok(())
}

/// v7.12.6 — PG `%` format expansion for RAISE. Sequential
/// positional substitution; `%%` produces a literal `%`.
fn format_raise_message(fmt: &str, args: &[String]) -> String {
    let mut out = String::with_capacity(fmt.len());
    let mut iter = args.iter();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.peek() {
                Some('%') => {
                    out.push('%');
                    chars.next();
                }
                _ => {
                    if let Some(a) = iter.next() {
                        out.push_str(a);
                    } else {
                        // Unconsumed placeholder — PG emits an
                        // error here; we mirror by leaving the
                        // bare `%` so the message stays readable.
                        out.push('%');
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// v7.12.6 — Display rendering for a [`Value`] inside a RAISE
/// message arg. Booleans / ints / floats render naturally;
/// strings render unquoted; other types fall back to Debug.
fn value_to_display_string(v: &Value) -> String {
    use alloc::string::ToString;
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Float(x) => x.to_string(),
        Value::Text(s) | Value::Json(s) => s.to_string(),
        other => format!("{other:?}"),
    }
}

/// Evaluate a sub-expression against the NEW / OLD row context.
/// Pre-walks the AST replacing every `NEW.col` / `OLD.col`
/// reference with a literal of the actual value, then dispatches
/// to the regular [`eval::eval_expr`]. Pre-walk strategy mirrors
/// the existing [`substitute_in_expr`] used by correlated
/// subqueries.
/// v7.12.6 — same as [`eval_with_new_old`] but also substitutes
/// qualifier-less `Column(<name>)` references whose name matches
/// a `DECLARE`'d local variable. Locals shadow table-column refs
/// (PG semantics — though a careful trigger function avoids the
/// collision via naming convention).
#[allow(clippy::too_many_arguments)]
fn eval_with_new_old_and_locals(
    expr: &Expr,
    new_row: Option<&Row<'static>>,
    old_row: Option<&Row<'static>>,
    locals: &BTreeMap<String, Value>,
    columns: &[ColumnSchema],
    table_alias: &str,
    params: &[Value<'static>],
    default_text_search_config: Option<&str>,
    subquery_resolver: Option<&SelectIntoResolver<'_>>,
) -> Result<Value<'static>, EvalError> {
    let mut rewritten = expr.clone();
    substitute_locals(&mut rewritten, locals);
    substitute_new_old(&mut rewritten, new_row, old_row, columns)?;
    // v7.39 (round 335, V61) — a scalar subquery inside a plpgsql
    // expression is RUN here, before the row evaluator sees it. The
    // evaluator cannot execute one — it answered "subquery reached row
    // eval — engine resolver bug", an internal message, for
    // `RETURN (SELECT …)`, `n := (SELECT …)` and any expression
    // containing one. `SELECT … INTO` worked only because it had a
    // resolver of its own; this gives expressions the same one.
    if let Some(resolver) = subquery_resolver {
        let mut failure: Option<EvalError> = None;
        substitute_locals_visiting(&mut rewritten, locals, &mut |node| {
            if failure.is_some() {
                return;
            }
            let Expr::ScalarSubquery(sel) = node else {
                return;
            };
            let mut stmt = spg_sql::ast::Statement::Select((**sel).clone());
            if let Err(e) = substitute_trigger_context_in_statement(
                &mut stmt, new_row, old_row, locals, columns,
            ) {
                failure = Some(e);
                return;
            }
            match resolver(&stmt) {
                Ok(v) => *node = value_to_literal_expr(&[], 0, v),
                Err(e) => {
                    failure = Some(EvalError::TypeMismatch {
                        detail: alloc::format!("{e}"),
                    });
                }
            }
        });
        if let Some(e) = failure {
            return Err(e);
        }
    }
    let ctx = EvalContext::new(columns, Some(table_alias))
        .with_params(params)
        .with_default_text_search_config(default_text_search_config);
    let empty = Row::new(Vec::new());
    eval::eval_expr(&rewritten, &empty, &ctx)
}

/// v7.12.6 — in-place substitute every qualifier-less
/// `Column(<name>)` whose name is in `locals` with that local's
/// current Value as a literal. Runs before [`substitute_new_old`]
/// so NEW.col / OLD.col references (which have a qualifier) take
/// the NEW/OLD path normally.
fn substitute_locals(expr: &mut Expr, locals: &BTreeMap<String, Value>) {
    substitute_locals_visiting(expr, locals, &mut |_| {});
}

/// v7.39 (round 335, V61) — the same full-tree walk, calling `visit` on
/// every node. It exists so a scalar subquery can be found and REPLACED
/// wherever it sits, reusing the one walker that already knows every
/// expression shape rather than growing a second one beside it.
fn substitute_locals_visiting(
    expr: &mut Expr,
    locals: &BTreeMap<String, Value>,
    visit: &mut dyn FnMut(&mut Expr),
) {
    visit(expr);
    if let Expr::Column(c) = expr {
        if c.qualifier.is_none()
            && let Some(v) = locals.get(&c.name)
        {
            *expr = value_to_literal_expr(&[], 0, v.clone());
            return;
        }
        // v7.39 (read01 round 64) — a RECORD variable's field: `rec.v` inside a
        // `FOR rec IN SELECT … LOOP`. The loop binds each row's columns as
        // `rec.<col>` locals, so the qualified reference resolves here.
        if let Some(q) = &c.qualifier {
            let key = alloc::format!("{}.{}", q.to_ascii_lowercase(), c.name.to_ascii_lowercase());
            if let Some(v) = locals.get(&key) {
                *expr = value_to_literal_expr(&[], 0, v.clone());
                return;
            }
        }
    }
    match expr {
        Expr::Collate { expr, .. } | Expr::NamedArg { expr, .. } => {
            substitute_locals_visiting(expr, locals, visit)
        }
        Expr::Variadic(expr) => substitute_locals_visiting(expr, locals, visit),
        Expr::AggregateOrdered { call, order_by, .. } => {
            substitute_locals_visiting(call, locals, visit);
            for o in order_by.iter_mut() {
                substitute_locals_visiting(&mut o.expr, locals, visit);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            substitute_locals_visiting(lhs, locals, visit);
            substitute_locals_visiting(rhs, locals, visit);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
            substitute_locals_visiting(expr, locals, visit);
        }
        Expr::Like { expr, pattern, .. } => {
            substitute_locals_visiting(expr, locals, visit);
            substitute_locals_visiting(pattern, locals, visit);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                substitute_locals_visiting(a, locals, visit);
            }
        }
        Expr::Extract { source, .. } => substitute_locals_visiting(source, locals, visit),
        Expr::Array(items) => {
            for elem in items {
                substitute_locals_visiting(elem, locals, visit);
            }
        }
        Expr::ArraySubscript { target, index } => {
            substitute_locals_visiting(target, locals, visit);
            substitute_locals_visiting(index, locals, visit);
        }
        Expr::ArraySlice { target, lo, hi } => {
            substitute_locals_visiting(target, locals, visit);
            if let Some(l) = lo {
                substitute_locals_visiting(l, locals, visit);
            }
            if let Some(h) = hi {
                substitute_locals_visiting(h, locals, visit);
            }
        }
        Expr::AnyAll { expr, array, .. } => {
            substitute_locals_visiting(expr, locals, visit);
            substitute_locals_visiting(array, locals, visit);
        }
        Expr::InList { expr, list, .. } => {
            substitute_locals_visiting(expr, locals, visit);
            for item in list {
                substitute_locals_visiting(item, locals, visit);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                substitute_locals_visiting(o, locals, visit);
            }
            for (w, t) in branches {
                substitute_locals_visiting(w, locals, visit);
                substitute_locals_visiting(t, locals, visit);
            }
            if let Some(e) = else_branch {
                substitute_locals_visiting(e, locals, visit);
            }
        }
        Expr::Literal(_)
        | Expr::Placeholder(_)
        | Expr::Column(_)
        | Expr::WindowFunction { .. }
        | Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. } => {}
    }
}

fn eval_with_new_old(
    expr: &Expr,
    new_row: Option<&Row<'static>>,
    old_row: Option<&Row<'static>>,
    columns: &[ColumnSchema],
    table_alias: &str,
    params: &[Value<'static>],
    default_text_search_config: Option<&str>,
) -> Result<Value<'static>, EvalError> {
    let mut rewritten = expr.clone();
    substitute_new_old(&mut rewritten, new_row, old_row, columns)?;
    let ctx = EvalContext::new(columns, Some(table_alias))
        .with_params(params)
        .with_default_text_search_config(default_text_search_config);
    // Empty row — the substitution above eliminated every column
    // reference that depended on NEW / OLD; any remaining column
    // reference is a bug (would surface as ColumnNotFound).
    let empty = Row::new(Vec::new());
    eval::eval_expr(&rewritten, &empty, &ctx)
}

/// In-place walk: replace every `Column{qualifier=NEW|OLD,name=c}`
/// reference with the corresponding row value, materialised as
/// an `Expr::Literal`. Recurses through every Expr variant so
/// `to_tsvector('english', NEW.subject || ' ' || NEW.sender)`
/// substitutes cleanly even though the references nest inside
/// function calls + binary operators.
/// v7.39 (round 138) — does a row trigger's `WHEN ( condition )` hold for the
/// NEW / OLD row? Empty text = no condition (always fires). After NEW/OLD are
/// substituted to literals the predicate is constant, so a minimal eval context
/// suffices — this is a free fn callable from the borrow-constrained INSERT row
/// loop. Only a definite TRUE fires (NULL / FALSE skip), matching PG.
pub(crate) fn trigger_when_holds(
    when_text: &str,
    new_row: Option<&Row<'static>>,
    old_row: Option<&Row<'static>>,
    columns: &[ColumnSchema],
) -> Result<bool, EngineError> {
    if when_text.is_empty() {
        return Ok(true);
    }
    let mut expr = spg_sql::parser::parse_expression(when_text)
        .map_err(|e| EngineError::Unsupported(alloc::format!("trigger WHEN: {e}")))?;
    substitute_new_old(&mut expr, new_row, old_row, columns).map_err(EngineError::Eval)?;
    let ctx = crate::eval::EvalContext::new(&[], None);
    let empty = Row::new(alloc::vec::Vec::new());
    let v = crate::eval::eval_expr(&expr, &empty, &ctx).map_err(EngineError::Eval)?;
    Ok(matches!(v, Value::Bool(true)))
}

pub(crate) fn substitute_new_old(
    expr: &mut Expr,
    new_row: Option<&Row<'static>>,
    old_row: Option<&Row<'static>>,
    columns: &[ColumnSchema],
) -> Result<(), EvalError> {
    if let Expr::Column(c) = expr {
        if let Some(q) = &c.qualifier {
            let lower = q.to_ascii_lowercase();
            if lower == "new" || lower == "old" {
                let (row, side) = if lower == "new" {
                    (new_row, "NEW")
                } else {
                    (old_row, "OLD")
                };
                let pos = columns
                    .iter()
                    .position(|sc| sc.name.eq_ignore_ascii_case(&c.name))
                    .ok_or_else(|| EvalError::ColumnNotFound {
                        token: spg_sql::ast::SrcToken::NONE,
                        name: format!("{side}.{}", c.name),
                    })?;
                let v = match row {
                    Some(r) => r.values.get(pos).cloned().unwrap_or(Value::Null),
                    None => Value::Null,
                };
                *expr = value_to_literal_expr(columns, pos, v);
                return Ok(());
            }
        }
    }
    match expr {
        Expr::Collate { expr, .. } | Expr::NamedArg { expr, .. } => {
            substitute_new_old(expr, new_row, old_row, columns)?
        }
        Expr::Variadic(expr) => substitute_new_old(expr, new_row, old_row, columns)?,
        Expr::AggregateOrdered { call, order_by, .. } => {
            substitute_new_old(call, new_row, old_row, columns)?;
            for o in order_by.iter_mut() {
                substitute_new_old(&mut o.expr, new_row, old_row, columns)?;
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            substitute_new_old(lhs, new_row, old_row, columns)?;
            substitute_new_old(rhs, new_row, old_row, columns)?;
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
            substitute_new_old(expr, new_row, old_row, columns)?;
        }
        Expr::Like { expr, pattern, .. } => {
            substitute_new_old(expr, new_row, old_row, columns)?;
            substitute_new_old(pattern, new_row, old_row, columns)?;
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                substitute_new_old(a, new_row, old_row, columns)?;
            }
        }
        Expr::Extract { source, .. } => substitute_new_old(source, new_row, old_row, columns)?,
        Expr::Array(items) => {
            for elem in items {
                substitute_new_old(elem, new_row, old_row, columns)?;
            }
        }
        Expr::ArraySubscript { target, index } => {
            substitute_new_old(target, new_row, old_row, columns)?;
            substitute_new_old(index, new_row, old_row, columns)?;
        }
        Expr::ArraySlice { target, lo, hi } => {
            substitute_new_old(target, new_row, old_row, columns)?;
            if let Some(l) = lo {
                substitute_new_old(l, new_row, old_row, columns)?;
            }
            if let Some(h) = hi {
                substitute_new_old(h, new_row, old_row, columns)?;
            }
        }
        Expr::AnyAll { expr, array, .. } => {
            substitute_new_old(expr, new_row, old_row, columns)?;
            substitute_new_old(array, new_row, old_row, columns)?;
        }
        Expr::InList { expr, list, .. } => {
            substitute_new_old(expr, new_row, old_row, columns)?;
            for item in list {
                substitute_new_old(item, new_row, old_row, columns)?;
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                substitute_new_old(o, new_row, old_row, columns)?;
            }
            for (w, t) in branches {
                substitute_new_old(w, new_row, old_row, columns)?;
                substitute_new_old(t, new_row, old_row, columns)?;
            }
            if let Some(e) = else_branch {
                substitute_new_old(e, new_row, old_row, columns)?;
            }
        }
        // Leaves + variants we don't recurse into (sub-queries
        // inside a trigger body would require correlated-query
        // wiring; carved out of v7.12.4).
        Expr::Literal(_)
        | Expr::Placeholder(_)
        | Expr::Column(_)
        | Expr::WindowFunction { .. }
        | Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. } => {}
    }
    Ok(())
}

/// Turn a [`Value`] back into an [`Expr::Literal`]. Necessary
/// because [`substitute_new_old`] inlines NEW/OLD cell values
/// into the expression tree.
fn value_to_literal_expr(_columns: &[ColumnSchema], _pos: usize, v: Value) -> Expr {
    use spg_sql::ast::Literal;
    let lit = match v {
        Value::Null => Literal::Null,
        Value::Bool(b) => Literal::Bool(b),
        Value::SmallInt(n) => Literal::Integer(i64::from(n)),
        Value::Int(n) => Literal::Integer(i64::from(n)),
        Value::BigInt(n) => Literal::Integer(n),
        Value::Float(x) => Literal::Float(x),
        Value::Text(s) | Value::Json(s) => Literal::String(s.into_owned()),
        // Other values (Vector, Date, Timestamp, TsVector, etc.)
        // round-trip through the Display form back into a string
        // literal. v7.12.5 will add typed-literal variants here
        // so the cast layer doesn't need to re-parse from text.
        other => Literal::String(format!("{other:?}")),
    };
    Expr::Literal(lit)
}

/// v7.12.7 — substitute NEW / OLD / DECLARE-local references in
/// every `Expr` field of a [`Statement`]. Used to materialise an
/// embedded SQL statement's NEW.col / OLD.col / local-var refs as
/// literals so the engine can re-execute it without holding the
/// trigger context.
pub(crate) fn substitute_trigger_context_in_statement(
    stmt: &mut spg_sql::ast::Statement,
    new_row: Option<&Row<'static>>,
    old_row: Option<&Row<'static>>,
    locals: &BTreeMap<String, Value>,
    columns: &[ColumnSchema],
) -> Result<(), EvalError> {
    use spg_sql::ast::Statement;
    let mut walk = |e: &mut Expr| -> Result<(), EvalError> {
        substitute_locals(e, locals);
        substitute_new_old(e, new_row, old_row, columns)?;
        Ok(())
    };
    match stmt {
        Statement::Insert(s) => {
            for tuple in &mut s.rows {
                for e in tuple {
                    walk(e)?;
                }
            }
        }
        Statement::Update(s) => {
            for (_col, e) in &mut s.assignments {
                walk(e)?;
            }
            if let Some(w) = &mut s.where_ {
                walk(w)?;
            }
        }
        Statement::Delete(s) => {
            if let Some(w) = &mut s.where_ {
                walk(w)?;
            }
        }
        Statement::Select(s) => {
            substitute_trigger_context_in_select(s, new_row, old_row, locals, columns)?
        }
        // Other statement kinds (DDL, SHOW, etc.) inside a
        // trigger body would only meaningfully reference NEW/OLD
        // in error-message position; v7.12.7 doesn't recursively
        // substitute their Expr fields. Future surfaces (e.g.
        // RAISE ... USING) can add cases here.
        _ => {}
    }
    Ok(())
}

fn substitute_trigger_context_in_select(
    s: &mut spg_sql::ast::SelectStatement,
    new_row: Option<&Row<'static>>,
    old_row: Option<&Row<'static>>,
    locals: &BTreeMap<String, Value>,
    columns: &[ColumnSchema],
) -> Result<(), EvalError> {
    use spg_sql::ast::SelectItem;
    let mut walk = |e: &mut Expr| -> Result<(), EvalError> {
        substitute_locals(e, locals);
        substitute_new_old(e, new_row, old_row, columns)?;
        Ok(())
    };
    for item in &mut s.items {
        if let SelectItem::Expr { expr, .. } = item {
            walk(expr)?;
        }
    }
    if let Some(w) = &mut s.where_ {
        walk(w)?;
    }
    if let Some(group_by) = &mut s.group_by {
        for g in group_by {
            walk(g)?;
        }
    }
    if let Some(h) = &mut s.having {
        walk(h)?;
    }
    for ob in &mut s.order_by {
        walk(&mut ob.expr)?;
    }
    // LIMIT / OFFSET use `LimitExpr` (integer literal or
    // placeholder); they don't carry an `Expr` to substitute
    // into. Leave them alone.
    let _ = &s.limit;
    let _ = &s.offset;
    Ok(())
}

/// v7.12.4 — find the triggers that should fire for a given
/// `(table, event, timing)` tuple. Returns names so the caller
/// can iterate without holding a borrow on the catalog while it
/// mutates rows.
pub fn matching_trigger_names<'a>(
    triggers: &'a [TriggerDef],
    table: &str,
    event: &str,
    timing: &str,
) -> Vec<&'a TriggerDef> {
    triggers
        .iter()
        .filter(|t| {
            t.table == table
                && t.timing.eq_ignore_ascii_case(timing)
                && t.for_each.eq_ignore_ascii_case("row")
                && t.events.iter().any(|e| e.eq_ignore_ascii_case(event))
        })
        .collect()
}

impl Engine {
    /// v7.12.4 — snapshot every row-level trigger on `table` that
    /// fires for `event` (`"INSERT"` / `"UPDATE"` / `"DELETE"`) at
    /// the given `timing` (`"BEFORE"` / `"AFTER"`), and clone its
    /// referenced function definition. Returned as a vec of owned
    /// `FunctionDef` so the row-write loop can fire them without
    /// holding a borrow on the catalog (which would conflict with
    /// the table.insert / update_row / delete mutable borrows).
    pub(crate) fn snapshot_row_triggers(
        &self,
        table: &str,
        event: &str,
        timing: &str,
    ) -> Vec<(
        spg_storage::FunctionDef,
        alloc::string::String,
        alloc::string::String,
    )> {
        let cat = self.active_catalog();
        let mut matching: Vec<&spg_storage::TriggerDef> = cat
            .triggers()
            .iter()
            .filter(|t| {
                // v7.16.1 — skip disabled triggers (mailrs
                // round-9 A.2.b — pg_dump --disable-triggers).
                t.enabled
                    && t.table == table
                    && t.timing.eq_ignore_ascii_case(timing)
                    && t.for_each.eq_ignore_ascii_case("row")
                    && t.events.iter().any(|e| e.eq_ignore_ascii_case(event))
            })
            .collect();
        // v7.39 (round 755, F31-B2) — same-event triggers fire in NAME
        // order, PG18-measured (a_trig before z_trig regardless of
        // creation order); the catalog Vec keeps insertion order.
        matching.sort_by(|a, b| a.name.cmp(&b.name));
        matching
            .into_iter()
            // v7.39 (read01 round 62) — functions are keyed by SIGNATURE now. A
            // trigger names its function by NAME, and a trigger function takes
            // no arguments, so there is at most one.
            // v7.39 (read01 round 82) — carry the TRIGGER's name alongside the
            // function, for TG_NAME (which is the trigger name, not the function
            // name).
            .filter_map(|t| {
                cat.functions_named(&t.function)
                    .first()
                    .map(|f| ((*f).clone(), t.when_condition.clone(), t.name.clone()))
            })
            .collect()
    }

    /// v7.13.0 — UPDATE-side snapshot that pairs each trigger's
    /// function with its `UPDATE OF cols` filter (mailrs round-5
    /// G7). Empty filter Vec means "fire unconditionally", matching
    /// the v7.12 behaviour.
    pub(crate) fn snapshot_update_row_triggers(
        &self,
        table: &str,
        timing: &str,
    ) -> Vec<(
        spg_storage::FunctionDef,
        Vec<String>,
        alloc::string::String,
        alloc::string::String,
    )> {
        let cat = self.active_catalog();
        let mut matching: Vec<&spg_storage::TriggerDef> = cat
            .triggers()
            .iter()
            .filter(|t| {
                // v7.16.1 — skip disabled triggers.
                t.enabled
                    && t.table == table
                    && t.timing.eq_ignore_ascii_case(timing)
                    && t.for_each.eq_ignore_ascii_case("row")
                    && t.events.iter().any(|e| e.eq_ignore_ascii_case("UPDATE"))
            })
            .collect();
        // v7.39 (round 755, F31-B2) — NAME order, PG18-measured.
        matching.sort_by(|a, b| a.name.cmp(&b.name));
        matching
            .into_iter()
            // (fd, UPDATE-OF cols, WHEN text, trigger name).
            .filter_map(|t| {
                cat.functions_named(&t.function).first().map(|fd| {
                    (
                        (*fd).clone(),
                        t.update_columns.clone(),
                        t.when_condition.clone(),
                        t.name.clone(),
                    )
                })
            })
            .collect()
    }

    /// v7.12.7 — drain the trigger-emitted embedded SQL queue.
    /// Called by the INSERT / UPDATE / DELETE executors after
    /// their main row-write loop returns. Each statement runs
    /// inside the same cancel scope as the firing DML and bumps
    /// the recursion counter; nested embedded SQL beyond
    /// [`MAX_TRIGGER_RECURSION`] errors with a clear message so
    /// a trigger-graph cycle surfaces as a query failure instead
    /// of stack-blowing the engine.
    pub(crate) fn execute_deferred_trigger_stmts(
        &mut self,
        deferred: Vec<DeferredEmbeddedStmt>,
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        for d in deferred {
            if self.trigger_recursion_depth >= MAX_TRIGGER_RECURSION {
                return Err(EngineError::Storage(StorageError::Corrupt(alloc::format!(
                    "trigger embedded SQL recursion depth {} exceeded (trigger function \
                     {:?} would push past the {} cap — check for trigger cycles)",
                    self.trigger_recursion_depth,
                    d.function,
                    MAX_TRIGGER_RECURSION,
                ))));
            }
            self.trigger_recursion_depth += 1;
            let res = self.execute_stmt_with_cancel(d.stmt, cancel);
            self.trigger_recursion_depth -= 1;
            res?;
        }
        Ok(())
    }
}
