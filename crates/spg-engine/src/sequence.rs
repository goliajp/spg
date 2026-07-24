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

/// v7.39 (round 417) — MySQL advisory-lock names are arbitrary strings;
/// SPG's shared advisory registry is keyed by i64. Fold the name via
/// FNV-1a-64 (canonical spec, no seed, deterministic across runs) and
/// bit-cast to i64. Collisions are cryptographically negligible for the
/// practical namespace an app touches.
fn mysql_lock_key(name: &str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV_OFFSET_BASIS
    for &b in name.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3); // FNV_PRIME
    }
    h as i64
}

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
                // v7.39 (round 279) — advisory locks resolve here too.
                // They mutate engine state, and the value dispatch only
                // ever sees `&EvalContext`; this statement-level pass is
                // the established home for a state-changing function
                // (nextval / setval land here for the same reason).
                if let Some(v) = self.eval_advisory_call(&lc, args)? {
                    *expr = Expr::Literal(value_to_literal(v));
                    return Ok(());
                }
                // v7.39 (round 287) — the large-object functions land here
                // for the same reason: lo_from_bytea / lo_put / lo_unlink
                // mutate the catalog, and the value dispatch only ever
                // holds `&EvalContext`. lo_get is read-only but resolves
                // alongside them so the whole family reads one way.
                if let Some(v) = self.eval_large_object_call(&lc, args)? {
                    // `lo_get` yields bytea, and the AST has no bytes
                    // literal — folding it to a bare literal would hand
                    // downstream a TEXT of the hex form, so `length()`
                    // answered 12 for five bytes. Re-attach the type.
                    *expr = if matches!(v, spg_storage::Value::Bytes(_)) {
                        Expr::Cast {
                            expr: alloc::boxed::Box::new(Expr::Literal(value_to_literal(v))),
                            target: spg_sql::ast::CastTarget::Bytea,
                        }
                    } else {
                        Expr::Literal(value_to_literal(v))
                    };
                    return Ok(());
                }
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
    /// v7.39 (round 279) — evaluate an advisory-lock call against the
    /// shared registry, or `None` when this is not one.
    ///
    /// PG's key is either one bigint or two ints packed into one; both
    /// spellings address the same space, which is why they fold into a
    /// single i64 here.
    pub(crate) fn eval_advisory_call(
        &mut self,
        lc: &str,
        args: &[Expr],
    ) -> Result<Option<spg_storage::Value<'static>>, EngineError> {
        // v7.39 (round 417) — MySQL's advisory-lock family:
        //   GET_LOCK(name, timeout) / RELEASE_LOCK(name) / IS_FREE_LOCK(name)
        //   IS_USED_LOCK(name) / RELEASE_ALL_LOCKS().
        // Route to the same shared registry PG advisory locks use, hashing
        // the string name to an i64 with FNV-1a-64. NULL name → NULL for
        // every arm, matching MariaDB. Only under a MySQL session — a PG
        // session keeps errors on these names.
        if self.backslash_escapes
            && let Some(v) = self.eval_mysql_lock_call(lc, args)?
        {
            return Ok(Some(v));
        }
        let is_take = matches!(
            lc,
            "pg_advisory_lock"
                | "pg_advisory_xact_lock"
                | "pg_advisory_lock_shared"
                | "pg_advisory_xact_lock_shared"
        );
        let is_try = matches!(
            lc,
            "pg_try_advisory_lock"
                | "pg_try_advisory_xact_lock"
                | "pg_try_advisory_lock_shared"
                | "pg_try_advisory_xact_lock_shared"
        );
        let is_unlock = matches!(lc, "pg_advisory_unlock" | "pg_advisory_unlock_shared");
        if lc == "pg_advisory_unlock_all" {
            self.advisory_unlock_all();
            return Ok(Some(spg_storage::Value::Null));
        }
        if !(is_take || is_try || is_unlock) {
            return Ok(None);
        }
        let Some(key) = Self::advisory_key_of(args) else {
            // A non-literal key (a column, a parameter) is left for the
            // value dispatch, which keeps the old permissive answer.
            return Ok(None);
        };
        if is_take {
            // The blocking form cannot wait while the engine lock is
            // held; it takes the lock when free and returns void either
            // way, which is PG's answer for every uncontended call.
            let _ = self.advisory_try_lock(key);
            return Ok(Some(spg_storage::Value::Null));
        }
        if is_try {
            return Ok(Some(spg_storage::Value::Bool(self.advisory_try_lock(key))));
        }
        Ok(Some(spg_storage::Value::Bool(self.advisory_unlock(key))))
    }

    /// One bigint, or two ints packed high/low into the same space.
    fn advisory_key_of(args: &[Expr]) -> Option<i64> {
        let int_of = |e: &Expr| -> Option<i64> {
            match e {
                Expr::Literal(spg_sql::ast::Literal::Integer(n)) => Some(*n),
                _ => None,
            }
        };
        match args {
            [a] => int_of(a),
            [a, b] => {
                let (hi, lo) = (int_of(a)?, int_of(b)?);
                Some((hi << 32) | (lo & 0xFFFF_FFFF))
            }
            _ => None,
        }
    }

    /// v7.39 (round 417) — MySQL GET_LOCK / RELEASE_LOCK / IS_FREE_LOCK /
    /// IS_USED_LOCK / RELEASE_ALL_LOCKS. Returns None when the call is not
    /// one of these (so the caller falls through to the sequence / PG
    /// advisory path); returns Some(value) when it handled it (including a
    /// NULL for a non-literal or NULL name — matches MariaDB).
    fn eval_mysql_lock_call(
        &mut self,
        lc: &str,
        args: &[Expr],
    ) -> Result<Option<spg_storage::Value<'static>>, EngineError> {
        // Pull the lock name as a string literal, or NULL when either the
        // whole argument is NULL or the name is a non-literal (a bound
        // parameter / column / expression — MariaDB itself refuses these
        // with the same NULL answer for the constant-fold path).
        let string_lit = |e: &Expr| -> Option<Option<String>> {
            match e {
                Expr::Literal(spg_sql::ast::Literal::String(s)) => Some(Some(s.clone())),
                Expr::Literal(spg_sql::ast::Literal::Null) => Some(None),
                _ => None,
            }
        };
        match lc {
            "release_all_locks" => {
                if !args.is_empty() {
                    return Ok(None);
                }
                Ok(Some(spg_storage::Value::Int(self.advisory_unlock_all_count())))
            }
            "get_lock" => {
                // GET_LOCK(name, timeout) — timeout is not honoured (there is
                // no other writer to wait for; we take when free, and if
                // held by another session return 0 immediately). MariaDB's
                // own default is "wait up to timeout"; SPG's single-process
                // model makes an uncontended lock always available and a
                // contended one impossible to release from a peer, so the
                // wait is a no-op either way.
                if args.len() != 2 {
                    return Ok(None);
                }
                let Some(name) = string_lit(&args[0]) else {
                    return Ok(None);
                };
                let Some(name) = name else {
                    return Ok(Some(spg_storage::Value::Null));
                };
                let key = mysql_lock_key(&name);
                let got = self.advisory_try_lock(key);
                Ok(Some(spg_storage::Value::Int(i32::from(got))))
            }
            "release_lock" => {
                if args.len() != 1 {
                    return Ok(None);
                }
                let Some(name) = string_lit(&args[0]) else {
                    return Ok(None);
                };
                let Some(name) = name else {
                    return Ok(Some(spg_storage::Value::Null));
                };
                let key = mysql_lock_key(&name);
                let me = self.current_session_id();
                Ok(Some(match self.advisory_holder(key) {
                    None => spg_storage::Value::Null,
                    Some(owner) if owner == me => {
                        // We hold it; release one level.
                        let _ = self.advisory_unlock(key);
                        spg_storage::Value::Int(1)
                    }
                    Some(_) => spg_storage::Value::Int(0),
                }))
            }
            "is_free_lock" => {
                if args.len() != 1 {
                    return Ok(None);
                }
                let Some(name) = string_lit(&args[0]) else {
                    return Ok(None);
                };
                let Some(name) = name else {
                    return Ok(Some(spg_storage::Value::Null));
                };
                let key = mysql_lock_key(&name);
                let free = self.advisory_holder(key).is_none();
                Ok(Some(spg_storage::Value::Int(i32::from(free))))
            }
            "is_used_lock" => {
                if args.len() != 1 {
                    return Ok(None);
                }
                let Some(name) = string_lit(&args[0]) else {
                    return Ok(None);
                };
                let Some(name) = name else {
                    return Ok(Some(spg_storage::Value::Null));
                };
                let key = mysql_lock_key(&name);
                Ok(Some(match self.advisory_holder(key) {
                    None => spg_storage::Value::Null,
                    Some(owner) => spg_storage::Value::BigInt(i64::from(owner)),
                }))
            }
            _ => Ok(None),
        }
    }

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

impl Engine {
    /// v7.39 (round 287) — PG's server-side large-object functions.
    ///
    /// Returns `Ok(None)` when `lc` is not one of them, or when an
    /// argument is not a literal (a column reference is left for the
    /// ordinary value dispatch, which reports the function as unknown —
    /// the same shape the sequence family uses).
    ///
    /// Offsets are 0-based, which is PG's convention here and NOT the
    /// 1-based one `substring` uses: `lo_get(o,1,3)` over 'Hello' is
    /// 'ell'. Measured, not assumed.
    pub(crate) fn eval_large_object_call(
        &mut self,
        lc: &str,
        args: &[Expr],
    ) -> Result<Option<spg_storage::Value<'static>>, EngineError> {
        use spg_storage::Value;
        if !matches!(
            lc,
            "lo_from_bytea"
                | "lo_get"
                | "lo_put"
                | "lo_unlink"
                | "lo_create"
                | "lo_creat"
                | "lo_open"
                | "loread"
                | "lowrite"
                | "lo_lseek"
                | "lo_lseek64"
                | "lo_tell"
                | "lo_tell64"
                | "lo_close"
                | "lo_truncate"
                | "lo_truncate64"
        ) {
            return Ok(None);
        }
        // Fold each argument through the catalog-aware literal
        // evaluator rather than pattern-matching token shapes: a bytea
        // argument arrives as `Cast { String, Bytea }`, not as a literal,
        // and the same is true of every other coerced spelling.
        let mut vals: alloc::vec::Vec<spg_storage::Value<'static>> =
            alloc::vec::Vec::with_capacity(args.len());
        for a in args {
            match crate::conversions::literal_expr_to_value_in(a.clone(), Some(self.active_catalog()))
            {
                Ok(v) => vals.push(v),
                // Not foldable (a column, a subquery) — leave the call
                // alone, exactly as the sequence family does.
                Err(_) => return Ok(None),
            }
        }
        let int_arg = |i: usize| -> Option<i64> {
            match vals.get(i)? {
                Value::SmallInt(n) => Some(i64::from(*n)),
                Value::Int(n) => Some(i64::from(*n)),
                Value::BigInt(n) => Some(*n),
                _ => None,
            }
        };
        let bytes_arg = |i: usize| -> Option<alloc::vec::Vec<u8>> {
            match vals.get(i)? {
                Value::Bytes(b) => Some(b.to_vec()),
                Value::Text(t) => crate::conversions::decode_bytea_literal(t).ok(),
                _ => None,
            }
        };
        let missing = |oid: i64| {
            EngineError::Unsupported(alloc::format!("large object {oid} does not exist"))
        };
        match lc {
            "lo_creat" | "lo_create" => {
                let Some(req) = int_arg(0) else {
                    return Ok(None);
                };
                // lo_creat(-1) and lo_create(0) both mean "pick an OID".
                let want = u32::try_from(req).unwrap_or(0);
                let oid = self
                    .active_catalog_mut()
                    .create_large_object(want, alloc::vec::Vec::new())
                    .map_err(EngineError::Unsupported)?;
                Ok(Some(Value::Int(i32::try_from(oid).unwrap_or(0))))
            }
            "lo_from_bytea" => {
                let (Some(req), Some(data)) = (int_arg(0), bytes_arg(1)) else {
                    return Ok(None);
                };
                let want = u32::try_from(req).unwrap_or(0);
                let oid = self
                    .active_catalog_mut()
                    .create_large_object(want, data)
                    .map_err(EngineError::Unsupported)?;
                Ok(Some(Value::Int(i32::try_from(oid).unwrap_or(0))))
            }
            "lo_get" => {
                let Some(oid) = int_arg(0) else {
                    return Ok(None);
                };
                let key = u32::try_from(oid).map_err(|_| missing(oid))?;
                let all = self
                    .active_catalog()
                    .large_object(key)
                    .ok_or_else(|| missing(oid))?
                    .to_vec();
                if args.len() == 1 {
                    return Ok(Some(Value::Bytes(all.into())));
                }
                let (Some(off), Some(len)) = (int_arg(1), int_arg(2)) else {
                    return Ok(None);
                };
                let start = usize::try_from(off).unwrap_or(0).min(all.len());
                let end = start
                    .saturating_add(usize::try_from(len).unwrap_or(0))
                    .min(all.len());
                Ok(Some(Value::Bytes(all[start..end].to_vec().into())))
            }
            "lo_put" => {
                let (Some(oid), Some(off), Some(data)) = (int_arg(0), int_arg(1), bytes_arg(2))
                else {
                    return Ok(None);
                };
                let key = u32::try_from(oid).map_err(|_| missing(oid))?;
                self.active_catalog_mut()
                    .put_large_object(key, usize::try_from(off).unwrap_or(0), &data)
                    .map_err(EngineError::Unsupported)?;
                // PG's lo_put returns void.
                Ok(Some(Value::Null))
            }
            "lo_unlink" => {
                let Some(oid) = int_arg(0) else {
                    return Ok(None);
                };
                let key = u32::try_from(oid).map_err(|_| missing(oid))?;
                if self.active_catalog_mut().unlink_large_object(key) {
                    Ok(Some(Value::Int(1)))
                } else {
                    Err(missing(oid))
                }
            }
            // ---- the descriptor family (round 306, V28) -------------
            //
            // These share one session-scoped table, emptied when the
            // transaction ends. Modes are PG's: INV_WRITE 0x20000,
            // INV_READ 0x40000. Only the write bit is remembered —
            // reading needs no permission in PG (measured: a
            // write-only descriptor reads fine).
            "lo_open" => {
                let (Some(oid), Some(mode)) = (int_arg(0), int_arg(1)) else {
                    return Ok(None);
                };
                if mode & 0x0006_0000 == 0 {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "invalid flags for opening a large object: {mode}"
                    )));
                }
                let key = u32::try_from(oid).map_err(|_| missing(oid))?;
                if self.active_catalog().large_object(key).is_none() {
                    return Err(missing(oid));
                }
                let fd = self.lo_next_fd;
                self.lo_next_fd = self.lo_next_fd.saturating_add(1);
                self.lo_descriptors.insert(
                    fd,
                    crate::LargeObjectDescriptor {
                        oid: key,
                        pos: 0,
                        writable: mode & 0x0002_0000 != 0,
                    },
                );
                Ok(Some(Value::Int(fd)))
            }
            "loread" => {
                let (Some(fd), Some(len)) = (int_arg(0), int_arg(1)) else {
                    return Ok(None);
                };
                let d = *self.lo_descriptor(fd)?;
                let all = self
                    .active_catalog()
                    .large_object(d.oid)
                    .ok_or_else(|| missing(i64::from(d.oid)))?;
                let start = usize::try_from(d.pos).unwrap_or(usize::MAX).min(all.len());
                // A negative length reads nothing — PG answers an empty
                // bytea rather than erroring.
                let want = usize::try_from(len).unwrap_or(0);
                let end = start.saturating_add(want).min(all.len());
                let out = all[start..end].to_vec();
                self.lo_descriptor_mut(fd)?.pos = end as u64;
                Ok(Some(Value::Bytes(out.into())))
            }
            "lowrite" => {
                let (Some(fd), Some(data)) = (int_arg(0), bytes_arg(1)) else {
                    return Ok(None);
                };
                let d = *self.lo_descriptor(fd)?;
                if !d.writable {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "large object descriptor {fd} was not opened for writing"
                    )));
                }
                let at = usize::try_from(d.pos).unwrap_or(usize::MAX);
                // `put_large_object` zero-fills a gap, which is what PG
                // does when the position is past the end.
                self.active_catalog_mut()
                    .put_large_object(d.oid, at, &data)
                    .map_err(EngineError::Unsupported)?;
                let written = data.len();
                self.lo_descriptor_mut(fd)?.pos = d.pos.saturating_add(written as u64);
                Ok(Some(Value::Int(i32::try_from(written).unwrap_or(0))))
            }
            "lo_lseek" | "lo_lseek64" => {
                let (Some(fd), Some(off), Some(whence)) =
                    (int_arg(0), int_arg(1), int_arg(2))
                else {
                    return Ok(None);
                };
                let d = *self.lo_descriptor(fd)?;
                let size = self
                    .active_catalog()
                    .large_object(d.oid)
                    .map_or(0i64, |b| b.len() as i64);
                let base = match whence {
                    0 => 0,
                    1 => i64::try_from(d.pos).unwrap_or(i64::MAX),
                    2 => size,
                    _ => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "invalid whence setting: {whence}"
                        )));
                    }
                };
                let target = base.saturating_add(off);
                if target < 0 {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "invalid large object seek target: {target}"
                    )));
                }
                self.lo_descriptor_mut(fd)?.pos = target as u64;
                Ok(Some(if lc == "lo_lseek" {
                    Value::Int(i32::try_from(target).unwrap_or(i32::MAX))
                } else {
                    Value::BigInt(target)
                }))
            }
            "lo_tell" | "lo_tell64" => {
                let Some(fd) = int_arg(0) else {
                    return Ok(None);
                };
                let pos = i64::try_from(self.lo_descriptor(fd)?.pos).unwrap_or(i64::MAX);
                Ok(Some(if lc == "lo_tell" {
                    Value::Int(i32::try_from(pos).unwrap_or(i32::MAX))
                } else {
                    Value::BigInt(pos)
                }))
            }
            "lo_close" => {
                let Some(fd) = int_arg(0) else {
                    return Ok(None);
                };
                // Validate before removing so a stale descriptor gets
                // the same wording every other call does.
                self.lo_descriptor(fd)?;
                let key = i32::try_from(fd).unwrap_or(-1);
                self.lo_descriptors.remove(&key);
                Ok(Some(Value::Int(0)))
            }
            "lo_truncate" | "lo_truncate64" => {
                let (Some(fd), Some(len)) = (int_arg(0), int_arg(1)) else {
                    return Ok(None);
                };
                let d = *self.lo_descriptor(fd)?;
                if !d.writable {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "large object descriptor {fd} was not opened for writing"
                    )));
                }
                self.active_catalog_mut()
                    .truncate_large_object(d.oid, usize::try_from(len).unwrap_or(0))
                    .map_err(EngineError::Unsupported)?;
                Ok(Some(Value::Int(0)))
            }
            _ => Ok(None),
        }
    }

    /// Look up an open descriptor, with PG's wording for a stale or
    /// never-opened one. The same message covers "closed", "from a
    /// finished transaction", and "never existed" — as it does in PG.
    fn lo_descriptor(&self, fd: i64) -> Result<&crate::LargeObjectDescriptor, EngineError> {
        let key = i32::try_from(fd).unwrap_or(-1);
        self.lo_descriptors.get(&key).ok_or_else(|| {
            EngineError::Unsupported(alloc::format!("invalid large-object descriptor: {fd}"))
        })
    }

    fn lo_descriptor_mut(
        &mut self,
        fd: i64,
    ) -> Result<&mut crate::LargeObjectDescriptor, EngineError> {
        let key = i32::try_from(fd).unwrap_or(-1);
        self.lo_descriptors.get_mut(&key).ok_or_else(|| {
            EngineError::Unsupported(alloc::format!("invalid large-object descriptor: {fd}"))
        })
    }
}
