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

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use spg_sql::ast::{AssignTarget, Expr, PlPgSqlStmt, ReturnTarget};
use spg_storage::{ColumnSchema, FunctionDef, Row, TriggerDef, Value};

use crate::eval::{self, EvalContext, EvalError};

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
) -> Result<TriggerOutcome, TriggerError> {
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
    let mut current_new = new_row;
    for stmt in &block.statements {
        match stmt {
            PlPgSqlStmt::Assign { target, value } => match target {
                AssignTarget::NewColumn(col) => {
                    if is_after {
                        return Err(TriggerError::NewReadOnlyInAfterTrigger {
                            function: function.name.clone(),
                            column: col.clone(),
                        });
                    }
                    let pos = columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(col))
                        .ok_or_else(|| TriggerError::UnknownColumn {
                            function: function.name.clone(),
                            column: col.clone(),
                            table: alloc::string::ToString::to_string(&table_name),
                        })?;
                    let evaluated = eval_with_new_old(
                        value,
                        current_new.as_ref(),
                        old_row,
                        columns,
                        table_name,
                        params,
                        default_text_search_config,
                    )
                    .map_err(|cause| TriggerError::EvalFailed {
                        function: function.name.clone(),
                        cause,
                    })?;
                    // current_new is guaranteed Some here for the
                    // BEFORE INSERT/UPDATE shape (the only ones
                    // that pass a NEW row in). Surface a clear
                    // error rather than panic if a caller passes
                    // None inappropriately.
                    let row =
                        current_new
                            .as_mut()
                            .ok_or_else(|| TriggerError::UnsupportedConstruct {
                                function: function.name.clone(),
                                detail: format!(
                                    "NEW.{col} := … requires a NEW row context \
                                 (BEFORE INSERT / UPDATE only — not available on DELETE)"
                                ),
                            })?;
                    row.values[pos] = evaluated;
                }
                AssignTarget::OldColumn(col) => {
                    return Err(TriggerError::OldIsReadOnly {
                        function: function.name.clone(),
                        column: col.clone(),
                    });
                }
                AssignTarget::Local(name) => {
                    return Err(TriggerError::UnsupportedConstruct {
                        function: function.name.clone(),
                        detail: format!(
                            "local variable {name:?} (`DECLARE` blocks land in v7.12.5)"
                        ),
                    });
                }
            },
            PlPgSqlStmt::Return(target) => {
                return Ok(match target {
                    ReturnTarget::New => {
                        current_new.map_or(TriggerOutcome::Skip, TriggerOutcome::Row)
                    }
                    ReturnTarget::Old => old_row
                        .cloned()
                        .map_or(TriggerOutcome::Skip, TriggerOutcome::Row),
                    ReturnTarget::Null => TriggerOutcome::Skip,
                    ReturnTarget::Expr(_) => {
                        return Err(TriggerError::UnsupportedConstruct {
                            function: function.name.clone(),
                            detail: String::from(
                                "RETURN <expr> in a trigger function — only RETURN NEW / OLD / NULL is valid \
                                 (scalar UDF return values land with the v7.12.5 scalar function surface)",
                            ),
                        });
                    }
                });
            }
        }
    }
    // Body fell off without an explicit RETURN. PL/pgSQL default
    // is `RETURN NULL`; we mirror — the BEFORE trigger then
    // skips the row.
    Ok(TriggerOutcome::Skip)
}

/// Evaluate a sub-expression against the NEW / OLD row context.
/// Pre-walks the AST replacing every `NEW.col` / `OLD.col`
/// reference with a literal of the actual value, then dispatches
/// to the regular [`eval::eval_expr`]. Pre-walk strategy mirrors
/// the existing [`substitute_in_expr`] used by correlated
/// subqueries.
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
