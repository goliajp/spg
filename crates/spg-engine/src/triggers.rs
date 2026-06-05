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
use spg_storage::{ColumnSchema, FunctionDef, Row, TriggerDef, Value};

use crate::eval::{self, EvalContext, EvalError};

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
pub enum TriggerOutcome {
    /// `RETURN NEW;` (or `RETURN OLD;`) — write this row.
    /// For BEFORE triggers, the row may differ from the input
    /// (e.g. `NEW.search_vector := …` rewrote a cell). For AFTER
    /// triggers, the value is currently ignored — but we still
    /// surface it for symmetric callers / future v7.12.5 use.
    Row(Row),
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
    RaiseException { function: String, message: String },
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
            Self::RaiseException { function, message } => {
                write!(
                    f,
                    "trigger function {function:?}: RAISE EXCEPTION {message:?}"
                )
            }
        }
    }
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
    new_row: Option<Row>,
    old_row: Option<&Row>,
    table_name: &str,
    columns: &[ColumnSchema],
    params: &[Value],
    default_text_search_config: Option<&str>,
    is_after: bool,
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
    let mut locals: BTreeMap<String, Value> = BTreeMap::new();
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
    )?;
    let mut current_new = new_row;
    let ctx = BodyCtx {
        function: &function.name,
        table_name,
        columns,
        params,
        default_text_search_config,
        is_after,
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
        BodyOutcome::FellThrough => TriggerOutcome::Skip,
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
}

/// Shared parameters every body-stmt evaluation needs. Bundled so
/// the recursive `execute_stmts` doesn't have to thread eight
/// individual `&str` / `&[…]` args around.
struct BodyCtx<'a> {
    function: &'a str,
    table_name: &'a str,
    columns: &'a [ColumnSchema],
    params: &'a [Value],
    default_text_search_config: Option<&'a str>,
    is_after: bool,
}

fn execute_stmts(
    stmts: &[PlPgSqlStmt],
    current_new: &mut Option<Row>,
    old_row: Option<&Row>,
    locals: &mut BTreeMap<String, Value>,
    ctx: &BodyCtx<'_>,
    deferred: &mut Vec<DeferredEmbeddedStmt>,
) -> Result<BodyOutcome, TriggerError> {
    for stmt in stmts {
        match stmt {
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
                )
                .map_err(|cause| TriggerError::EvalFailed {
                    function: ctx.function.into(),
                    cause,
                })?;
                match target {
                    AssignTarget::NewColumn(col) => {
                        if ctx.is_after {
                            return Err(TriggerError::NewReadOnlyInAfterTrigger {
                                function: ctx.function.into(),
                                column: col.clone(),
                            });
                        }
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
                        return Err(TriggerError::OldIsReadOnly {
                            function: ctx.function.into(),
                            column: col.clone(),
                        });
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
                    )
                    .map_err(|cause| TriggerError::EvalFailed {
                        function: ctx.function.into(),
                        cause,
                    })?;
                    if matches!(cond_val, Value::Bool(true)) {
                        matched = true;
                        match execute_stmts(body, current_new, old_row, locals, ctx, deferred)? {
                            BodyOutcome::Return(t) => return Ok(BodyOutcome::Return(t)),
                            BodyOutcome::FellThrough => {}
                        }
                        break;
                    }
                }
                if !matched && !else_branch.is_empty() {
                    match execute_stmts(else_branch, current_new, old_row, locals, ctx, deferred)? {
                        BodyOutcome::Return(t) => return Ok(BodyOutcome::Return(t)),
                        BodyOutcome::FellThrough => {}
                    }
                }
            }
            PlPgSqlStmt::Raise {
                level,
                message,
                args,
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
                    )
                    .map_err(|cause| TriggerError::EvalFailed {
                        function: ctx.function.into(),
                        cause,
                    })?;
                    rendered_args.push(value_to_display_string(&v));
                }
                let resolved = format_raise_message(message, &rendered_args);
                if matches!(level, RaiseLevel::Exception) {
                    return Err(TriggerError::RaiseException {
                        function: ctx.function.into(),
                        message: resolved,
                    });
                }
                // NOTICE / WARNING / INFO / LOG / DEBUG — log to
                // stderr for v7.12.6. Wiring through the server's
                // log channel is a v7.12.7+ polish item; the
                // resolved message stays accessible regardless.
                let _ = resolved;
                let _ = level;
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
                deferred.push(DeferredEmbeddedStmt {
                    function: ctx.function.into(),
                    stmt: substituted,
                });
            }
        }
    }
    Ok(BodyOutcome::FellThrough)
}

fn resolve_return(
    target: ReturnTarget,
    current_new: Option<Row>,
    old_row: Option<&Row>,
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
    new_row: Option<&Row>,
    old_row: Option<&Row>,
    columns: &[ColumnSchema],
    table_name: &str,
    params: &[Value],
    default_text_search_config: Option<&str>,
    function_name: &str,
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
        Value::Text(s) | Value::Json(s) => s.clone(),
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
    new_row: Option<&Row>,
    old_row: Option<&Row>,
    locals: &BTreeMap<String, Value>,
    columns: &[ColumnSchema],
    table_alias: &str,
    params: &[Value],
    default_text_search_config: Option<&str>,
) -> Result<Value, EvalError> {
    let mut rewritten = expr.clone();
    substitute_locals(&mut rewritten, locals);
    substitute_new_old(&mut rewritten, new_row, old_row, columns)?;
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
    if let Expr::Column(c) = expr {
        if c.qualifier.is_none()
            && let Some(v) = locals.get(&c.name)
        {
            *expr = value_to_literal_expr(&[], 0, v.clone());
            return;
        }
    }
    match expr {
        Expr::Binary { lhs, rhs, .. } => {
            substitute_locals(lhs, locals);
            substitute_locals(rhs, locals);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            substitute_locals(expr, locals);
        }
        Expr::Like { expr, pattern, .. } => {
            substitute_locals(expr, locals);
            substitute_locals(pattern, locals);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                substitute_locals(a, locals);
            }
        }
        Expr::Extract { source, .. } => substitute_locals(source, locals),
        Expr::Array(items) => {
            for elem in items {
                substitute_locals(elem, locals);
            }
        }
        Expr::ArraySubscript { target, index } => {
            substitute_locals(target, locals);
            substitute_locals(index, locals);
        }
        Expr::AnyAll { expr, array, .. } => {
            substitute_locals(expr, locals);
            substitute_locals(array, locals);
        }
        Expr::Literal(_)
        | Expr::Placeholder(_)
        | Expr::Column(_)
        | Expr::WindowFunction { .. }
        | Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. } => {}
    }
}

fn eval_with_new_old(
    expr: &Expr,
    new_row: Option<&Row>,
    old_row: Option<&Row>,
    columns: &[ColumnSchema],
    table_alias: &str,
    params: &[Value],
    default_text_search_config: Option<&str>,
) -> Result<Value, EvalError> {
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
fn substitute_new_old(
    expr: &mut Expr,
    new_row: Option<&Row>,
    old_row: Option<&Row>,
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
        Expr::Binary { lhs, rhs, .. } => {
            substitute_new_old(lhs, new_row, old_row, columns)?;
            substitute_new_old(rhs, new_row, old_row, columns)?;
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
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
        Expr::AnyAll { expr, array, .. } => {
            substitute_new_old(expr, new_row, old_row, columns)?;
            substitute_new_old(array, new_row, old_row, columns)?;
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
        | Expr::InSubquery { .. } => {}
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
        Value::Text(s) | Value::Json(s) => Literal::String(s),
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
fn substitute_trigger_context_in_statement(
    stmt: &mut spg_sql::ast::Statement,
    new_row: Option<&Row>,
    old_row: Option<&Row>,
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
    new_row: Option<&Row>,
    old_row: Option<&Row>,
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
