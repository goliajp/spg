//! Sequence-call resolution split out of `lib.rs` (lib.rs split 9 —
//! first cut into the execution-core `impl Engine`). Pre-resolves
//! `nextval` / `currval` / `setval` (and the serial-implicit sequences)
//! inside a statement's expression slots before the row loop runs:
//! `pre_resolve_sequence_calls_in_statement` walks per-statement-kind,
//! `resolve_sequence_calls_in_expr` rewrites each FunctionCall node,
//! `eval_sequence_call` advances the sequence, and
//! `ensure_implicit_sequence` lazily materialises a BIGSERIAL's backing
//! sequence. Whole `impl Engine` methods moved verbatim; the execute
//! path drives `pre_resolve_*`, `dml.rs` drives `resolve_sequence_calls_in_expr`,
//! and `ddl.rs` drives `ensure_implicit_sequence`.

use alloc::string::{String, ToString};

use spg_sql::ast::{Expr, Statement};
use spg_storage::Value;

use crate::{Engine, EngineError, value_to_literal};

impl Engine {
    /// v7.17.0 Phase 1.1 — walk a Statement tree and pre-resolve
    /// any sequence FunctionCall nodes inside its Expr slots.
    /// Delegates per-statement-kind: SELECT projection +
    /// WHERE, INSERT VALUES, UPDATE SET, DELETE WHERE.
    pub(crate) fn pre_resolve_sequence_calls_in_statement(
        &mut self,
        stmt: &mut Statement,
    ) -> Result<(), EngineError> {
        match stmt {
            Statement::Select(s) => self.pre_resolve_sequence_calls_in_select(s),
            Statement::Insert(s) => {
                for tuple in &mut s.rows {
                    for cell in tuple.iter_mut() {
                        self.resolve_sequence_calls_in_expr(cell)?;
                    }
                }
                Ok(())
            }
            Statement::Update(s) => {
                for (_col, expr) in &mut s.assignments {
                    self.resolve_sequence_calls_in_expr(expr)?;
                }
                if let Some(w) = &mut s.where_ {
                    self.resolve_sequence_calls_in_expr(w)?;
                }
                Ok(())
            }
            Statement::Delete(s) => {
                if let Some(w) = &mut s.where_ {
                    self.resolve_sequence_calls_in_expr(w)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn pre_resolve_sequence_calls_in_select(
        &mut self,
        s: &mut spg_sql::ast::SelectStatement,
    ) -> Result<(), EngineError> {
        for item in &mut s.items {
            match item {
                spg_sql::ast::SelectItem::Expr { expr, .. } => {
                    self.resolve_sequence_calls_in_expr(expr)?;
                }
                spg_sql::ast::SelectItem::Wildcard
                | spg_sql::ast::SelectItem::QualifiedWildcard(_) => {}
            }
        }
        if let Some(w) = &mut s.where_ {
            self.resolve_sequence_calls_in_expr(w)?;
        }
        Ok(())
    }

    /// v7.17.0 Phase 1.1 — walk an Expr tree and pre-resolve any
    /// `nextval(name)` / `currval(name)` / `setval(name, value[,
    /// is_called])` FunctionCall nodes by calling the catalog and
    /// replacing the node with the resulting `Expr::Literal`.
    /// Used by INSERT VALUES / UPDATE SET / DEFAULT eval so the
    /// row-eval path sees pre-computed sequence values instead of
    /// needing mutable catalog access mid-eval.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn resolve_sequence_calls_in_expr(
        &mut self,
        expr: &mut Expr,
    ) -> Result<(), EngineError> {
        match expr {
            Expr::Literal(_) | Expr::Column(_) | Expr::Placeholder(_) => Ok(()),
            Expr::FunctionCall { name, args } => {
                // Descend first so nested calls — e.g.
                // setval('seq', currval('other')) — resolve
                // innermost-first.
                for a in args.iter_mut() {
                    self.resolve_sequence_calls_in_expr(a)?;
                }
                let lc = name.to_ascii_lowercase();
                if lc == "nextval" || lc == "currval" || lc == "setval" || lc == "lastval" {
                    let v = self.eval_sequence_call(&lc, args)?;
                    *expr = Expr::Literal(value_to_literal(v));
                } else if lc == "pg_get_serial_sequence" && args.len() == 2 {
                    // v7.29 (round-23a) — resolves to the implicit
                    // sequence name so the pg_dump idiom
                    // `setval(pg_get_serial_sequence('t','c'), n)`
                    // works (the setval arm receives a literal).
                    let lit = |e: &Expr| -> Option<String> {
                        match e {
                            Expr::Literal(spg_sql::ast::Literal::String(v)) => {
                                let t = v.strip_prefix("public.").unwrap_or(v).trim_matches('"');
                                Some(t.to_string())
                            }
                            _ => None,
                        }
                    };
                    if let (Some(t), Some(c)) = (lit(&args[0]), lit(&args[1])) {
                        let table_opt = self.active_catalog().get(&t);
                        // v7.37.17 (17.6 siblings) — if the table isn't
                        // in the active catalog, leave the call alone
                        // so the scalar arm in eval::functions handles
                        // it (returns the synthetic sequence name).
                        // ORMs (SQLAlchemy, Django) call this against
                        // arbitrary tables at introspection time — we
                        // want to answer with a plausible sequence
                        // name rather than NULL.
                        let Some(tb_ref) = table_opt else {
                            return Ok(());
                        };
                        let is_serial = tb_ref
                            .schema()
                            .columns
                            .iter()
                            .any(|col| col.name == c && col.auto_increment);
                        *expr = if is_serial {
                            Expr::Literal(spg_sql::ast::Literal::String(alloc::format!(
                                "public.{t}_{c}_seq"
                            )))
                        } else {
                            Expr::Literal(spg_sql::ast::Literal::Null)
                        };
                    }
                }
                Ok(())
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.resolve_sequence_calls_in_expr(lhs)?;
                self.resolve_sequence_calls_in_expr(rhs)
            }
            Expr::Unary { expr, .. } => self.resolve_sequence_calls_in_expr(expr),
            Expr::Cast { expr, .. } => self.resolve_sequence_calls_in_expr(expr),
            Expr::IsNull { expr, .. } => self.resolve_sequence_calls_in_expr(expr),
            Expr::Like { expr, pattern, .. } => {
                self.resolve_sequence_calls_in_expr(expr)?;
                self.resolve_sequence_calls_in_expr(pattern)
            }
            Expr::Extract { source, .. } => self.resolve_sequence_calls_in_expr(source),
            Expr::Array(items) => {
                for it in items.iter_mut() {
                    self.resolve_sequence_calls_in_expr(it)?;
                }
                Ok(())
            }
            // Window / subquery / etc — sequence calls inside these
            // are uncommon and require separate row-eval; leave
            // untouched for now and rely on the eval-time error
            // (no sequence_resolver attached).
            _ => Ok(()),
        }
    }

    /// v7.29 (mailrs round-23a) — SERIAL/BIGSERIAL columns get their
    /// PG-style implicit sequence `<table>_<column>_seq` ON FIRST
    /// ADDRESS rather than at CREATE TABLE time, so pre-7.29 data
    /// directories gain addressability without a storage migration.
    /// The sequence is born synced to the column's current MAX so
    /// `nextval` immediately after creation continues the series.
    pub(crate) fn ensure_implicit_sequence(&mut self, seq_name: &str) {
        if self.active_catalog().sequences().contains_key(seq_name) {
            return;
        }
        let Some(rest) = seq_name.strip_suffix("_seq") else {
            return;
        };
        let mut found: Option<(String, String, i64)> = None;
        for tname in self.active_catalog().table_names() {
            let Some(table) = self.active_catalog().get(&tname) else {
                continue;
            };
            for (i, col) in table.schema().columns.iter().enumerate() {
                if col.auto_increment && alloc::format!("{tname}_{}", col.name) == rest {
                    let next = table.next_auto_value(i).unwrap_or(1);
                    found = Some((tname.clone(), col.name.clone(), next - 1));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let Some((tname, cname, last)) = found else {
            return;
        };
        let def = spg_storage::SequenceDef {
            name: seq_name.to_string(),
            data_type: spg_storage::SequenceDataType::BigInt,
            start: 1,
            increment: 1,
            min_value: 1,
            max_value: i64::MAX,
            cache: 1,
            cycle: false,
            owned_by: Some((tname, cname)),
            last_value: last.max(0),
            is_called: last > 0,
            owner: None,
            acl: alloc::vec::Vec::new(),
        };
        let _ = self.active_catalog_mut().create_sequence(def, true);
    }

    /// v7.17.0 Phase 1.1 — evaluate a single nextval/currval/
    /// setval call. `args` are already pre-resolved Expr nodes
    /// (literals) — we extract their constant values.
    fn eval_sequence_call(
        &mut self,
        op: &str,
        args: &[Expr],
    ) -> Result<Value<'static>, EngineError> {
        // v7.37.17 — lastval(): the value most recently returned by
        // nextval() in this session. PG errors when nextval hasn't
        // been called yet; SPG matches.
        if op == "lastval" {
            let Some(seq) = self.last_sequence_used.clone() else {
                return Err(EngineError::Unsupported(
                    "lastval is not yet defined in this session".into(),
                ));
            };
            let v = self
                .active_catalog()
                .sequence_current_value(&seq)
                .map_err(EngineError::Storage)?;
            return Ok(Value::BigInt(v));
        }
        if args.is_empty() {
            return Err(EngineError::Unsupported(alloc::format!(
                "{op}() takes at least one argument"
            )));
        }
        let seq_name = match &args[0] {
            Expr::Literal(spg_sql::ast::Literal::String(s)) => {
                // v7.17 dump-compat — pg_dump emits sequence
                // names schema-qualified (`'public.posts_id_seq'`).
                // SPG is single-schema; strip a leading
                // `public.` / `pg_catalog.` so the catalog lookup
                // matches the bare-name CREATE SEQUENCE used.
                let trimmed = s
                    .strip_prefix("public.")
                    .or_else(|| s.strip_prefix("pg_catalog."))
                    .unwrap_or(s);
                trimmed.to_string()
            }
            // v7.17 dump-compat — pg_dump also emits
            // `nextval('public.posts_id_seq'::regclass)`
            // where the cast wraps the literal. Peel the cast
            // and continue.
            Expr::Cast { expr, .. } => {
                if let Expr::Literal(spg_sql::ast::Literal::String(s)) = expr.as_ref() {
                    let trimmed = s
                        .strip_prefix("public.")
                        .or_else(|| s.strip_prefix("pg_catalog."))
                        .unwrap_or(s);
                    trimmed.to_string()
                } else {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "{op}() first argument must be a literal sequence name"
                    )));
                }
            }
            other => {
                return Err(EngineError::Unsupported(alloc::format!(
                    "{op}() first argument must be a literal sequence name, got {other:?}"
                )));
            }
        };
        self.ensure_implicit_sequence(&seq_name);
        // v7.39 (read01 round 60) — sequence privileges. PG: nextval needs USAGE
        // or UPDATE, currval needs USAGE or SELECT, setval needs UPDATE alone.
        // The message names the SEQUENCE, not a table.
        {
            use spg_storage::priv_bits as pb;
            let wanted = match op {
                "nextval" => pb::USAGE | pb::UPDATE,
                "currval" | "lastval" => pb::USAGE | pb::SELECT,
                "setval" => pb::UPDATE,
                _ => 0,
            };
            if wanted != 0 {
                self.acl_require_sequence(&seq_name, wanted)?;
            }
        }
        match op {
            "nextval" => {
                let v = self
                    .active_catalog_mut()
                    .sequence_next_value(&seq_name)
                    .map_err(EngineError::Storage)?;
                // Register for lastval().
                self.last_sequence_used = Some(seq_name.clone());
                Ok(Value::BigInt(v))
            }
            "currval" => {
                let v = self
                    .active_catalog()
                    .sequence_current_value(&seq_name)
                    .map_err(EngineError::Storage)?;
                Ok(Value::BigInt(v))
            }
            "setval" => {
                if args.len() < 2 || args.len() > 3 {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "setval() takes 2 or 3 arguments, got {}",
                        args.len()
                    )));
                }
                let value = match &args[1] {
                    Expr::Literal(spg_sql::ast::Literal::Integer(n)) => *n,
                    other => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "setval() value argument must be a literal integer, got {other:?}"
                        )));
                    }
                };
                let is_called = if args.len() == 3 {
                    match &args[2] {
                        Expr::Literal(spg_sql::ast::Literal::Bool(b)) => *b,
                        other => {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "setval() is_called argument must be a literal BOOL, got {other:?}"
                            )));
                        }
                    }
                } else {
                    true
                };
                let v = self
                    .active_catalog_mut()
                    .sequence_set_value(&seq_name, value, is_called)
                    .map_err(EngineError::Storage)?;
                Ok(Value::BigInt(v))
            }
            other => Err(EngineError::Unsupported(alloc::format!(
                "unknown sequence op {other:?}"
            ))),
        }
    }
}
