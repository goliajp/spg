//! Clock-call rewriting split out of `lib.rs` (lib.rs split 4):
//! folds the zero-argument clock functions (`NOW()` /
//! `CURRENT_TIMESTAMP` / `CURRENT_DATE` / `unix_timestamp()`) and their
//! bare-identifier forms into synthetic `Cast` literals so a single
//! instant is captured per statement and `apply_function` never needs a
//! clock dependency. Walks SELECT / INSERT (rows + ON CONFLICT) /
//! UPDATE / DELETE statement trees, recursing through subqueries, CTEs,
//! window functions, and CASE branches. `value_to_literal` (runtime
//! `Value` → AST `Literal`) lives here too — the substitution path in
//! `substitute.rs` and the view-expansion path in the crate root drive
//! it. Free functions; the prepare/bind path drives `rewrite_clock_calls`.

use spg_sql::ast::{Expr, Literal, SelectStatement, Statement};
use spg_storage::Value;

use crate::eval;
use crate::substitute::walk_select_exprs_mut;

pub(crate) fn value_to_literal(v: Value) -> Literal {
    match v {
        Value::Null => Literal::Null,
        Value::SmallInt(n) => Literal::Integer(i64::from(n)),
        Value::Int(n) => Literal::Integer(i64::from(n)),
        Value::BigInt(n) => Literal::Integer(n),
        Value::Float(x) => Literal::Float(x),
        Value::Text(s) | Value::Json(s) => Literal::String(s.into_owned()),
        Value::Bool(b) => Literal::Bool(b),
        Value::Vector(v) => Literal::Vector(v.into_owned()),
        Value::Numeric {
            scaled,
            scale,
            kind,
        } => Literal::String(eval::format_numeric_kind(kind, scaled, scale)),
        // v7.38.8 — decoded, with the text kept for Display. This exit
        // is what `constfold` hands back, and handing back a STRING is
        // what made a folded constant cost a coercion on every row: the
        // fold removed the cast from the tree and left the parse in the
        // loop. The text is the same text, so nothing printed changes.
        Value::Date(d) => Literal::Date {
            days: d,
            text: eval::format_date(d),
        },
        Value::Timestamp(t) => Literal::Timestamp {
            micros: t,
            text: eval::format_timestamp(t),
        },
        // v7.17.0 Phase 3.P0-69 — UUID round-trips via canonical
        // hyphenated text. Without this arm the fallback below
        // renders `Debug` form ("Uuid([85, …])") which the
        // engine's Text → Uuid coerce can't parse, breaking
        // prepared-bind round-trip from the spg-sqlx adapter.
        Value::Uuid(b) => Literal::String(spg_storage::format_uuid(&b)),
        // v7.16.0 — BYTEA round-trip for the spg-sqlx Bind path.
        // PG-canonical text rep is `\x` + lowercase hex; the
        // engine's coerce_value already accepts that on the
        // text → bytea direction.
        Value::Bytes(b) => Literal::String(eval::format_bytea_hex(&b)),
        // Arrays ride the AST natively (mailrs embed round-12) —
        // the prior `{a,b,c}` text form only worked where a column
        // type drove the re-parse; `= ANY($1)` has no column
        // context and saw a bare Text value.
        Value::TextArray(items) => Literal::TextArray(items),
        Value::IntArray(items) => Literal::IntArray(items),
        Value::BigIntArray(items) => Literal::BigIntArray(items),
        Value::Interval {
            months,
            days,
            micros,
            kind,
        } => Literal::Interval {
            months,
            days,
            micros,
            text: eval::format_interval(months, days, micros),
        },
        // SQ8 / halfvec cells dequantise to f32 before reaching the
        // substitute walker; pgwire's Bind path handles that.
        Value::Sq8Vector(q) => Literal::Vector(spg_storage::quantize::dequantize(&q)),
        Value::HalfVector(h) => Literal::Vector(h.to_f32_vec()),
        // v7.5.0 — Value is #[non_exhaustive]; future variants
        // render as Debug-form String literal until explicit
        // mapping is added.
        v => Literal::String(alloc::format!("{v:?}")),
    }
}

pub(crate) fn rewrite_clock_calls(
    stmt: &mut Statement,
    now_micros: Option<i64>,
    // v7.39 (round 349, M6) — `NOW(3)` is MariaDB's spelling and PG has
    // no `now(integer)` at all (measured), so the precision argument on
    // THAT name is dialect-gated.
    mysql: bool,
    // v7.39 (round 523) — the session zone's offset at `now`. The
    // local-clock family (`current_date`, `localtimestamp`,
    // `current_time`) reads the SESSION's wall clock; only the
    // timestamptz spellings name a zone-free instant.
    tz_offset: i64,
) {
    let Some(now) = now_micros else {
        return;
    };
    match stmt {
        Statement::Select(s) => rewrite_select_clock(s, now, mysql, tz_offset),
        Statement::Insert(ins) => {
            for row in &mut ins.rows {
                for e in row {
                    rewrite_expr_clock(e, now, mysql, tz_offset);
                }
            }
            // `ON CONFLICT … DO UPDATE SET created_at = NOW()` —
            // the upsert assignments carry clock calls too (mailrs
            // embed round-12).
            if let Some(clause) = &mut ins.on_conflict
                && let spg_sql::ast::OnConflictAction::Update {
                    assignments,
                    where_,
                } = &mut clause.action
            {
                for (_, e) in assignments.iter_mut() {
                    rewrite_expr_clock(e, now, mysql, tz_offset);
                }
                if let Some(w) = where_ {
                    rewrite_expr_clock(w, now, mysql, tz_offset);
                }
            }
        }
        // `UPDATE … SET seen_at = NOW() WHERE …` / `DELETE … WHERE
        // ts < NOW()` (mailrs embed round-12 — previously only
        // SELECT / INSERT-rows were walked).
        Statement::Update(u) => {
            for (_, e) in &mut u.assignments {
                rewrite_expr_clock(e, now, mysql, tz_offset);
            }
            if let Some(w) = &mut u.where_ {
                rewrite_expr_clock(w, now, mysql, tz_offset);
            }
        }
        Statement::Delete(d) => {
            if let Some(w) = &mut d.where_ {
                rewrite_expr_clock(w, now, mysql, tz_offset);
            }
        }
        _ => {}
    }
}

fn rewrite_select_clock(s: &mut SelectStatement, now: i64, mysql: bool, tz_offset: i64) {
    // v7.38.7 — pin the name BEFORE the rewrite takes it away.
    //
    // Folding `now()` to a literal is what makes the clock stable across
    // a statement, but it also erases the name the column reports:
    // `SELECT now()::date` is named for its operand in PG (`now`), and
    // once the operand is a literal the cast has nothing to prefer, so
    // the column came back as `date`. Describe and the row stream both
    // read the rewritten tree, so both were wrong together and neither
    // could see why.
    //
    // Only for an item the user did NOT alias, and only when the name
    // actually changes — an alias the user wrote is never touched, and
    // an item whose name survives the fold is left exactly as it was.
    for item in &mut s.items {
        if let spg_sql::ast::SelectItem::Expr { expr, alias } = item
            && alias.is_none()
        {
            let before = spg_sql::ast::figure_column_name(expr);
            let mut probe = expr.clone();
            rewrite_expr_clock(&mut probe, now, mysql, tz_offset);
            let after = spg_sql::ast::figure_column_name(&probe);
            if before != after && before.is_some() {
                *alias = before;
            }
        }
    }

    // v7.25.1 (round-18) — shared traversal: CTE bodies, LATERAL
    // subqueries, JOIN ON, and UNION peers all get the clock
    // rewrite (NOW() inside a CTE previously survived to eval as
    // "unknown function `now`").
    let _ = walk_select_exprs_mut(s, &mut |e| {
        rewrite_expr_clock(e, now, mysql, tz_offset);
        Ok(())
    });
}

/// v3.0.3 hot path: every recursion lands in exactly one `match` arm.
/// Literal / Column-with-qualifier (the dominant cases on a typical
/// AST) take a single pattern dispatch and exit. The clock-rewrite
/// targets (zero-arg `NOW` / `CURRENT_TIMESTAMP` / `CURRENT_DATE`
/// functions, and bare `CURRENT_TIMESTAMP` / `CURRENT_DATE` column
/// refs) sit on their own arms with match guards so the fall-through
/// to the recursive arms is unambiguous.
fn rewrite_expr_clock(e: &mut Expr, now: i64, mysql: bool, tz_offset: i64) {
    // Fast-path test on the no-recursion shapes first. We can't fold
    // them into the big match below because they need to *replace* `e`
    // outright; the recursive arms below match on its sub-fields.
    if let Some(replacement) = clock_replacement_for(e, now, mysql, tz_offset) {
        *e = replacement;
        return;
    }
    match e {
        Expr::Collate { expr, .. } | Expr::NamedArg { expr, .. } => {
            rewrite_expr_clock(expr, now, mysql, tz_offset)
        }
        Expr::Variadic(expr) => rewrite_expr_clock(expr, now, mysql, tz_offset),
        Expr::AggregateOrdered { call, order_by, .. } => {
            rewrite_expr_clock(call, now, mysql, tz_offset);
            for o in order_by.iter_mut() {
                rewrite_expr_clock(&mut o.expr, now, mysql, tz_offset);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_expr_clock(lhs, now, mysql, tz_offset);
            rewrite_expr_clock(rhs, now, mysql, tz_offset);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
            rewrite_expr_clock(expr, now, mysql, tz_offset);
        }
        Expr::FunctionCall { name, args } => {
            // v7.39 (read01 round 97) — the single-arg `age(t)` form is PG's
            // `age(date_trunc('day', current_timestamp), t)`: the age of `t`
            // relative to midnight today. The eval-time fallback anchors at
            // 2020-01-01 (no clock available there); when a real clock IS set,
            // inject today's midnight as an explicit first argument so the
            // two-arg path computes the correct wall-clock age. An integer
            // literal is the `age(xid)` overload and is left untouched.
            // v7.39 (round 518) — a cast to `xid` is the transaction-id
            // overload too, and it was not exempt: `age('1'::xid)` got the
            // midnight argument injected and then failed the temporal path
            // with "age() needs DATE or TIMESTAMP". Only an INTEGER literal
            // was being recognised as the other overload.
            // The index must stay BEHIND the arity check: reading `args[0]`
            // first panicked the connection thread on a zero-argument call.
            // v7.39 (round 668) — renamed, because the old name said `xid`
            // while the list also held bare integers, and that conflation is
            // what let `age(12345)` be described as "the xid overload" in two
            // tests. What this really decides is whether to inject a clock.
            //
            // Integer CASTS join it: they are not xids either, but they must
            // reach the refusal in `datetime::age` to be told so in PG's
            // words. Injecting a clock first turned `age(12345::bigint)` into
            // the two-timestamp form, which answered "age() needs DATE or
            // TIMESTAMP, got bigint" where PG says
            // "function age(bigint) does not exist".
            let takes_no_clock = args.first().is_some_and(|a| {
                matches!(a, Expr::Literal(Literal::Integer(_)))
                    // `Int` and `BigInt` are their own CastTarget variants,
                    // so the name list below never sees them. Measured: with
                    // only the names, `12345::smallint` reached the refusal
                    // and `12345::bigint` did not.
                    || matches!(
                        a,
                        Expr::Cast {
                            target: spg_sql::ast::CastTarget::Int
                                | spg_sql::ast::CastTarget::BigInt,
                            ..
                        }
                    )
                    || matches!(
                        a,
                        Expr::Cast { target: spg_sql::ast::CastTarget::Named(n), .. }
                            if n.eq_ignore_ascii_case("xid")
                                || n.eq_ignore_ascii_case("int2")
                                || n.eq_ignore_ascii_case("int4")
                                || n.eq_ignore_ascii_case("int8")
                                || n.eq_ignore_ascii_case("smallint")
                                || n.eq_ignore_ascii_case("integer")
                                || n.eq_ignore_ascii_case("int")
                                || n.eq_ignore_ascii_case("bigint")
                    )
            });
            if args.len() == 1 && name.eq_ignore_ascii_case("age") && !takes_no_clock {
                let midnight = now.div_euclid(86_400_000_000) * 86_400_000_000;
                let today = Expr::Cast {
                    expr: alloc::boxed::Box::new(Expr::Literal(Literal::Integer(midnight))),
                    target: spg_sql::ast::CastTarget::Timestamp,
                };
                args.insert(0, today);
            }
            for a in args {
                rewrite_expr_clock(a, now, mysql, tz_offset);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            rewrite_expr_clock(expr, now, mysql, tz_offset);
            rewrite_expr_clock(pattern, now, mysql, tz_offset);
        }
        Expr::Extract { source, .. } => rewrite_expr_clock(source, now, mysql, tz_offset),
        // v4.10 subquery nodes — recurse into the inner SELECT's
        // expression slots so e.g. SELECT NOW() in a scalar
        // subquery picks up the same instant as the outer query.
        Expr::ScalarSubquery(s) => rewrite_select_clock(s, now, mysql, tz_offset),
        Expr::Exists { subquery, .. } => rewrite_select_clock(subquery, now, mysql, tz_offset),
        Expr::InSubquery { expr, subquery, .. } => {
            rewrite_expr_clock(expr, now, mysql, tz_offset);
            rewrite_select_clock(subquery, now, mysql, tz_offset);
        }
        Expr::RowInSubquery { row, subquery, .. } => {
            for el in row {
                rewrite_expr_clock(el, now, mysql, tz_offset);
            }
            rewrite_select_clock(subquery, now, mysql, tz_offset);
        }
        Expr::RowCmpSubquery { row, subquery, .. } => {
            for el in row {
                rewrite_expr_clock(el, now, mysql, tz_offset);
            }
            rewrite_select_clock(subquery, now, mysql, tz_offset);
        }
        // v4.12 window functions — args + PARTITION BY + ORDER BY
        // may all reference clock literals.
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                rewrite_expr_clock(a, now, mysql, tz_offset);
            }
            for p in partition_by {
                rewrite_expr_clock(p, now, mysql, tz_offset);
            }
            for (e, _, _) in order_by {
                rewrite_expr_clock(e, now, mysql, tz_offset);
            }
        }
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => {}
        Expr::Array(items) => {
            for elem in items {
                rewrite_expr_clock(elem, now, mysql, tz_offset);
            }
        }
        Expr::ArraySubscript { target, index } => {
            rewrite_expr_clock(target, now, mysql, tz_offset);
            rewrite_expr_clock(index, now, mysql, tz_offset);
        }
        Expr::ArraySlice { target, lo, hi } => {
            rewrite_expr_clock(target, now, mysql, tz_offset);
            if let Some(l) = lo {
                rewrite_expr_clock(l, now, mysql, tz_offset);
            }
            if let Some(h) = hi {
                rewrite_expr_clock(h, now, mysql, tz_offset);
            }
        }
        Expr::AnyAll { expr, array, .. } => {
            rewrite_expr_clock(expr, now, mysql, tz_offset);
            rewrite_expr_clock(array, now, mysql, tz_offset);
        }
        Expr::InList { expr, list, .. } => {
            rewrite_expr_clock(expr, now, mysql, tz_offset);
            for item in list {
                rewrite_expr_clock(item, now, mysql, tz_offset);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                rewrite_expr_clock(o, now, mysql, tz_offset);
            }
            for (w, t) in branches {
                rewrite_expr_clock(w, now, mysql, tz_offset);
                rewrite_expr_clock(t, now, mysql, tz_offset);
            }
            if let Some(e) = else_branch {
                rewrite_expr_clock(e, now, mysql, tz_offset);
            }
        }
    }
}

/// Returns `Some(Expr)` when `e` is one of the clock-call shapes that
/// must be rewritten; otherwise `None` so the caller falls through to
/// the recursive walk. Identifies both function-call forms (`NOW()` /
/// `CURRENT_TIMESTAMP()` / `CURRENT_DATE()`) and bare-identifier forms
/// (`CURRENT_TIMESTAMP` / `CURRENT_DATE` as unqualified column refs,
/// which is how PG accepts them without parens).
fn clock_replacement_for(e: &Expr, now: i64, mysql: bool, tz_offset: i64) -> Option<Expr> {
    // v7.39 (round 349, M6) — the fractional-seconds precision argument:
    // `NOW(3)`, `CURRENT_TIMESTAMP(3)`, `CURTIME(3)`. MariaDB 11 renders
    // `NOW(3)` as `2026-07-22 12:46:41.541` and `NOW(6)` with six digits;
    // PG spells it `current_timestamp(3)` / `localtimestamp(3)` /
    // `current_time(3)`. EVERY parenthesised form was
    // `function now(integer) does not exist` here — a PG-line gap as much
    // as a MySQL one. (PG really has no `now(integer)`, and that one
    // error was right; it stays.)
    let precision = match e {
        Expr::FunctionCall { name, args }
            if args.len() == 1 && (mysql || !name.eq_ignore_ascii_case("now")) =>
        {
            match args.first() {
                // Out of range: leave the call alone so the ordinary
                // dispatcher reports it (MariaDB 11: `Too big precision
                // specified for 'current_timestamp'. Maximum is 6`), rather
                // than silently handing back full microseconds.
                Some(Expr::Literal(spg_sql::ast::Literal::Integer(n))) if (0..=6).contains(n) => {
                    Some(*n)
                }
                _ => return None,
            }
        }
        _ => None,
    };
    let (kind, name) = match e {
        Expr::FunctionCall { name, args } if args.is_empty() => (ClockSite::Fn, name.as_str()),
        Expr::FunctionCall { name, .. } if precision.is_some() => (ClockSite::Fn, name.as_str()),
        Expr::Column(c) if c.qualifier.is_none() => (ClockSite::BareIdent, c.name.as_str()),
        _ => return None,
    };
    // ASCII case-insensitive name match. Each entry decides what
    // synthetic literal the call expands to.
    //
    // v7.17.0 Phase 3.P0-29 — `unix_timestamp` (no args) joins this
    // table as MySQL's epoch-seconds equivalent of `now()`. Folded
    // to a BigInt literal here so apply_function never needs a
    // clock dependency.
    enum ClockShape {
        Timestamp,
        /// v7.38 (T-tstz Phase 1) — `now()` / `current_timestamp` /
        /// `clock_timestamp` / `statement_timestamp` / `transaction_timestamp`
        /// are `timestamp with time zone` in PG. Same instant as `Timestamp`,
        /// but the folded literal must carry the tz type so the projected
        /// column type and `pg_typeof` report it. `localtimestamp` stays
        /// `Timestamp` (PG types it without time zone).
        TimestampTz,
        Date,
        UnixSeconds,
        /// v7.37.17 (17.6 siblings) — curtime / utc_time render the
        /// time-of-day as 'HH:MM:SS' text (the MySQL spellings).
        TimeText,
        /// v7.39 (round 755, F31-B6) — PG's `localtime`: a true TIME
        /// value. The old fold to `current_timestamp` made
        /// `pg_typeof(localtime)` answer `timestamp without time
        /// zone`; PG18-measured it is `time without time zone`.
        TimeOfDay,
        /// v7.39 (round 755, F31-B6) — PG's `current_time`: a true
        /// TIMETZ value with the session offset and microseconds
        /// (PG18-measured `11:14:20.658952+00`); the old text shape
        /// answered `11:14:20` typed unknown.
        TimeOfDayTz,
    }
    let shape = match name.len() {
        3 if kind == ClockSite::Fn && name.eq_ignore_ascii_case("now") => {
            Some(ClockShape::TimestampTz)
        }
        12 if name.eq_ignore_ascii_case("current_date") => Some(ClockShape::Date),
        // v7.37.17 (17.6 siblings) — MySQL clock spellings. SPG's
        // unified clock has no session timezone, so the utc_*
        // family and the local family read the same instant.
        7 if kind == ClockSite::Fn
            && (name.eq_ignore_ascii_case("curdate")
                || name.eq_ignore_ascii_case("sysdate")
                || name.eq_ignore_ascii_case("curtime")) =>
        {
            Some(if name.eq_ignore_ascii_case("curdate") {
                ClockShape::Date
            } else if name.eq_ignore_ascii_case("sysdate") {
                ClockShape::Timestamp
            } else {
                ClockShape::TimeText
            })
        }
        8 if kind == ClockSite::Fn
            && (name.eq_ignore_ascii_case("utc_date") || name.eq_ignore_ascii_case("utc_time")) =>
        {
            Some(if name.eq_ignore_ascii_case("utc_date") {
                ClockShape::Date
            } else {
                ClockShape::TimeText
            })
        }
        13 if kind == ClockSite::Fn && name.eq_ignore_ascii_case("utc_timestamp") => {
            Some(ClockShape::Timestamp)
        }
        12 if name.eq_ignore_ascii_case("current_time") => Some(ClockShape::TimeOfDayTz),
        // v7.37.17 (17.6 siblings) — PG's clock family. localtime /
        // localtimestamp (parenless) fold into current_timestamp
        // for the drop-in model. transaction_timestamp / statement_
        // _timestamp / clock_timestamp all return current_timestamp
        // — SPG runs everything in a single unified clock.
        9 if kind == ClockSite::Fn && name.eq_ignore_ascii_case("localtime") => {
            Some(ClockShape::TimeOfDay)
        }
        14 if kind == ClockSite::Fn && name.eq_ignore_ascii_case("unix_timestamp") => {
            Some(ClockShape::UnixSeconds)
        }
        15 if kind == ClockSite::Fn && name.eq_ignore_ascii_case("clock_timestamp") => {
            Some(ClockShape::TimestampTz)
        }
        17 if name.eq_ignore_ascii_case("current_timestamp") => Some(ClockShape::TimestampTz),
        14 if name.eq_ignore_ascii_case("localtimestamp") => Some(ClockShape::Timestamp),
        19 if kind == ClockSite::Fn && name.eq_ignore_ascii_case("statement_timestamp") => {
            Some(ClockShape::TimestampTz)
        }
        21 if kind == ClockSite::Fn && name.eq_ignore_ascii_case("transaction_timestamp") => {
            Some(ClockShape::TimestampTz)
        }
        _ => None,
    };
    let shape = shape?;
    // A precision of `p` keeps p fractional digits; 0 keeps none. Both
    // oracles truncate rather than round (measured).
    let now = match precision {
        Some(p) => {
            let div = 10_i64.pow(6 - u32::try_from(p).unwrap_or(6));
            now.div_euclid(div) * div
        }
        None => now,
    };
    // v7.39 (round 523) — the LOCAL-clock family reads the session's
    // wall clock; the timestamptz spellings name an instant and are the
    // same number in every zone. `current_date` under `SET TimeZone =
    // 'Asia/Tokyo'` named yesterday for the first nine hours of every
    // day, and `localtimestamp` was nine hours off all day.
    let local = now.saturating_add(tz_offset);
    if matches!(shape, ClockShape::TimeText) {
        let day_us = local.rem_euclid(86_400_000_000);
        let day_secs = day_us / 1_000_000;
        let mut text = alloc::format!(
            "{:02}:{:02}:{:02}",
            day_secs / 3600,
            (day_secs / 60) % 60,
            day_secs % 60
        );
        if let Some(p) = precision
            && (1..=6).contains(&p)
        {
            let frac = day_us % 1_000_000;
            let digits = usize::try_from(p).unwrap_or(6);
            text.push('.');
            text.push_str(&alloc::format!("{frac:06}")[..digits]);
        }
        return Some(Expr::Literal(spg_sql::ast::Literal::String(text)));
    }
    if matches!(shape, ClockShape::TimeOfDay | ClockShape::TimeOfDayTz) {
        // The precision truncation already ran on `now` above, so the
        // fraction carried here is exact; both keywords keep PG's full
        // microsecond default.
        let day_us = local.rem_euclid(86_400_000_000);
        let day_secs = day_us / 1_000_000;
        let mut text = alloc::format!(
            "{:02}:{:02}:{:02}.{:06}",
            day_secs / 3600,
            (day_secs / 60) % 60,
            day_secs % 60,
            day_us % 1_000_000
        );
        let target = if matches!(shape, ClockShape::TimeOfDayTz) {
            let off_secs = tz_offset.div_euclid(1_000_000);
            let (sign, a) = if off_secs < 0 {
                ('-', -off_secs)
            } else {
                ('+', off_secs)
            };
            text.push(sign);
            text.push_str(&alloc::format!("{:02}", a / 3600));
            if (a / 60) % 60 != 0 {
                text.push_str(&alloc::format!(":{:02}", (a / 60) % 60));
            }
            spg_sql::ast::CastTarget::Named(alloc::string::String::from("timetz"))
        } else {
            spg_sql::ast::CastTarget::Named(alloc::string::String::from("time"))
        };
        return Some(Expr::Cast {
            expr: alloc::boxed::Box::new(Expr::Literal(spg_sql::ast::Literal::String(text))),
            target,
        });
    }
    let payload = match shape {
        ClockShape::TimestampTz => now,
        ClockShape::Timestamp => local,
        ClockShape::Date => local.div_euclid(86_400_000_000),
        ClockShape::UnixSeconds => now.div_euclid(1_000_000),
        ClockShape::TimeText | ClockShape::TimeOfDay | ClockShape::TimeOfDayTz => {
            unreachable!("handled above")
        }
    };
    let target = match shape {
        ClockShape::Timestamp => spg_sql::ast::CastTarget::Timestamp,
        ClockShape::TimestampTz => spg_sql::ast::CastTarget::Timestamptz,
        ClockShape::Date => spg_sql::ast::CastTarget::Date,
        ClockShape::UnixSeconds => spg_sql::ast::CastTarget::BigInt,
        ClockShape::TimeText | ClockShape::TimeOfDay | ClockShape::TimeOfDayTz => {
            unreachable!("handled above")
        }
    };
    Some(Expr::Cast {
        expr: alloc::boxed::Box::new(Expr::Literal(spg_sql::ast::Literal::Integer(payload))),
        target,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClockSite {
    Fn,
    BareIdent,
}
