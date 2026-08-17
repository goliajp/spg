//! In-process sqllogictest runner — drives `spg_engine::Engine` directly,
//! no socket, no daemon. ~1 ms per record on a recent laptop.
//!
//! Output formatting follows the SQLite sqllogictest convention:
//! - `I` (integer) — base-10 integer text
//! - `T` (text) — cell value as-is, empty string rendered as `(empty)`
//! - `R` (real) — three-decimal-fraction
//! - `B` (bool) — `0` or `1`
//! - NULL — the literal string `NULL`
//! - Vector — `[a, b, c]` with each component formatted as a real
//!
//! Sort modes:
//! - `nosort` — engine row order
//! - `rowsort` — sort rows lexicographically by their joined cell string
//! - `valuesort` — sort the flat list of cell values

use crate::parser::{ExpectedQuery, Record, SortMode};
use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Pass,
    /// Why the record failed.
    Fail(String),
    /// Why the runner skipped (directive, unsupported form, …).
    Skip(String),
}

#[derive(Debug, Default)]
pub struct RunOutcome {
    pub pass: usize,
    pub fail: usize,
    pub skip: usize,
    pub per_record: Vec<Outcome>,
    /// r1052 (S2.3) — pg_regress-style unified diff blocks, one per
    /// failing query record, ready to concatenate into `slt.diffs`.
    pub diffs: Vec<String>,
}

#[derive(Debug)]
pub struct Runner {
    pub engine: Engine,
    /// Set by a failing query in `run_one`, collected by `run`.
    pending_diff: Option<String>,
}

impl Default for Runner {
    fn default() -> Self {
        Self::new()
    }
}

impl Runner {
    pub fn new() -> Self {
        // Inject a deterministic clock so corpus tests that touch
        // `NOW()` / `CURRENT_DATE` get a stable value across runs.
        // The fixed value is 2025-06-15T12:00:00.000000Z =
        // 1_749_988_800_000_000 microseconds since epoch.
        let mut engine = Engine::new().with_clock(fixed_test_clock);
        // v7.37.16 — two-way `SPG_MVCC_INPLACE` override (default is
        // now ON; `=0` runs the corpus on the legacy path).
        match std::env::var("SPG_MVCC_INPLACE").ok().as_deref() {
            Some(v)
                if v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off") =>
            {
                engine.set_mvcc_inplace(false);
            }
            Some(v)
                if v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on") =>
            {
                engine.set_mvcc_inplace(true);
            }
            _ => {}
        }
        Self {
            engine,
            pending_diff: None,
        }
    }

    /// Run a parsed `.test` file's records against a fresh engine and tally.
    /// `halt` short-circuits the rest.
    pub fn run(&mut self, records: &[Record]) -> RunOutcome {
        let mut out = RunOutcome::default();
        for record in records {
            let outcome = self.run_one(record);
            match &outcome {
                Outcome::Pass => out.pass += 1,
                Outcome::Fail(_) => out.fail += 1,
                Outcome::Skip(_) => out.skip += 1,
            }
            if let Some(d) = self.pending_diff.take() {
                out.diffs.push(d);
            }
            out.per_record.push(outcome);
            if matches!(record, Record::Halt) {
                break;
            }
        }
        out
    }

    /// r1052 (S2.2 `--record`) — execute a statement for its state
    /// effect only.
    ///
    /// # Errors
    /// The engine's error text.
    pub fn exec_statement(&mut self, sql: &str) -> Result<(), String> {
        self.engine
            .execute(sql)
            .map(|_| ())
            .map_err(|e| format!("{e}"))
    }

    /// r1052 (S2.2 `--record`) — the rendered actual cells for a query,
    /// exactly as `run_one` would compare them (same renderer, same
    /// sort), so a recorded expectation cannot disagree with a later
    /// run by construction.
    ///
    /// # Errors
    /// Engine error, or a query record that ran DDL/DML.
    pub fn query_actual(
        &mut self,
        sql: &str,
        type_string: &str,
        sort: SortMode,
    ) -> Result<Vec<String>, String> {
        match self.engine.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => Ok(render_rows(&rows, type_string, sort)),
            Ok(QueryResult::CommandOk { .. }) => {
                Err(format!("query record but ran DDL/DML: {sql}"))
            }
            Ok(_) => Err(format!("unexpected QueryResult variant: {sql}")),
            Err(e) => Err(format!("{e}")),
        }
    }

    fn run_one(&mut self, record: &Record) -> Outcome {
        match record {
            Record::Halt => Outcome::Pass,
            Record::Statement {
                directive,
                sql,
                expect_error,
            } => {
                if directive.skip {
                    return Outcome::Skip("directive".into());
                }
                let result = self.engine.execute(sql);
                match (result, expect_error) {
                    (Ok(_), false) => Outcome::Pass,
                    (Ok(_), true) => Outcome::Fail(format!("expected error, got ok: {sql}")),
                    (Err(_), true) => Outcome::Pass,
                    (Err(e), false) => Outcome::Fail(format!("{e}")),
                }
            }
            Record::Query {
                directive,
                sql,
                type_string,
                sort,
                expected,
            } => {
                if directive.skip {
                    return Outcome::Skip("directive".into());
                }
                let ExpectedQuery::Values(expected) = expected else {
                    return Outcome::Skip("hashed expected — runner doesn't hash yet".into());
                };
                let result = match self.engine.execute(sql) {
                    Ok(QueryResult::Rows { rows, .. }) => rows,
                    Ok(QueryResult::CommandOk { .. }) => {
                        return Outcome::Fail(format!("query record but ran DDL/DML: {sql}"));
                    }
                    // v7.5.0 — QueryResult is #[non_exhaustive].
                    Ok(_) => {
                        return Outcome::Fail(format!("unexpected QueryResult variant: {sql}"));
                    }
                    Err(e) => return Outcome::Fail(format!("{e}")),
                };
                let actual = render_rows(&result, type_string, *sort);
                if actual == *expected {
                    Outcome::Pass
                } else {
                    self.pending_diff = Some(unified_diff(directive.line, sql, expected, &actual));
                    Outcome::Fail(format!(
                        "row mismatch\n  expected: {expected:?}\n  actual:   {actual:?}"
                    ))
                }
            }
        }
    }
}

/// Flatten result rows into the cell-per-string list sqllogictest compares
/// against, formatted by `type_string` and sorted by `sort`.
fn render_rows(rows: &[spg_storage::Row], type_string: &str, sort: SortMode) -> Vec<String> {
    let types: Vec<char> = type_string.chars().collect();
    let mut rendered_rows: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            row.values
                .iter()
                .enumerate()
                .map(|(i, v)| render_cell(v, *types.get(i).unwrap_or(&'T')))
                .collect()
        })
        .collect();

    match sort {
        SortMode::NoSort => {}
        SortMode::RowSort => {
            rendered_rows.sort_by_key(|a| a.join("\u{0}"));
        }
        SortMode::ValueSort => {
            // Flatten, sort, regroup.
            let mut flat: Vec<String> = rendered_rows.iter().flatten().cloned().collect();
            flat.sort();
            let cols = type_string.len().max(1);
            rendered_rows = flat.chunks(cols).map(<[String]>::to_vec).collect();
        }
    }

    rendered_rows.into_iter().flatten().collect()
}

/// Deterministic clock used by the corpus runner so anything that
/// references `NOW()` / `CURRENT_TIMESTAMP` / `CURRENT_DATE` returns
/// the same instant on every test run.
/// Fixed at 2025-06-15T12:00:00.000000Z.
const TEST_CLOCK_MICROS: i64 = 1_749_988_800_000_000;

fn fixed_test_clock() -> i64 {
    TEST_CLOCK_MICROS
}

fn render_cell(v: &Value, ty: char) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        // v7.39 — REAL renders float4out shortest form.
        Value::Real(x) => spg_engine::eval::format_real(*x),
        Value::Float(x) => match ty {
            'I' => (*x as i64).to_string(),
            _ => format_real(*x),
        },
        Value::Text(s) => {
            if s.is_empty() {
                "(empty)".into()
            } else {
                s.to_string()
            }
        }
        Value::Bool(b) => (if *b { "1" } else { "0" }).into(),
        // v7.39 (bpchar epic) — padded stored form, like the pgwire display.
        Value::BpChar(s) => s.to_string(),
        Value::Vector(v) => {
            let cells: Vec<String> = v.iter().map(|x| format_real(f64::from(*x))).collect();
            format!("[{}]", cells.join(","))
        }
        // v6.0.1: SQ8 cells render dequantised, matching the
        // pgvector wire shape that the corpora assert.
        Value::Sq8Vector(q) => {
            let cells: Vec<String> = spg_storage::quantize::dequantize(q)
                .iter()
                .map(|x| format_real(f64::from(*x)))
                .collect();
            format!("[{}]", cells.join(","))
        }
        // v6.0.3: HalfVector cells also render dequantised on the
        // sqllogictest grid.
        Value::HalfVector(h) => {
            let cells: Vec<String> = h
                .to_f32_vec()
                .iter()
                .map(|x| format_real(f64::from(*x)))
                .collect();
            format!("[{}]", cells.join(","))
        }
        // r1039 — through the engine's own renderer, which knows the
        // three NUMERIC specials. Reading `scaled` and `scale` and
        // dropping `kind` printed a stored `'NaN'::numeric` as `0`,
        // because a special carries a canonical zero in those fields —
        // the harness could not express a value the engine stores
        // correctly, so a corpus file could not pin one.
        v @ Value::Numeric { .. } => spg_engine::eval::value_to_text(v),
        Value::Date(d) => spg_engine::eval::format_date(*d),
        Value::Timestamp(t) => spg_engine::eval::format_timestamp(*t),
        // v7.17.0 Phase 3.P0-32 — PG TIME canonical text form.
        Value::Time(us) => spg_engine::eval::format_time(*us),
        // v7.17.0 Phase 3.P0-33 — MySQL YEAR 4-digit zero-padded.
        Value::Year(y) => format!("{y:04}"),
        // v7.17.0 Phase 3.P0-34 — PG TIMETZ canonical text form.
        Value::TimeTz { us, offset_secs } => spg_engine::eval::format_timetz(*us, *offset_secs),
        // v7.17.0 Phase 3.P0-35 — PG MONEY canonical en_US text form.
        Value::Money(c) => spg_engine::eval::format_money(*c),
        // v7.17.0 Phase 3.P0-38 — PG range canonical text form.
        v @ Value::Range { .. } => spg_engine::format_range_text(v),
        // v7.17.0 Phase 3.P0-39 — PG hstore canonical text form.
        Value::Hstore(pairs) => spg_engine::format_hstore_text(pairs),
        // v7.17.0 Phase 3.P0-40 — 2D array canonical text form.
        Value::IntArray2D(rows) => spg_engine::format_int_2d_text_pub(rows),
        Value::BigIntArray2D(rows) => spg_engine::format_bigint_2d_text_pub(rows),
        Value::TextArray2D(rows) => spg_engine::format_text_2d_text_pub(rows),
        Value::Interval {
            months,
            days,
            micros,
        } => spg_engine::eval::format_interval(*months, *days, *micros),
        Value::Json(s) => s.to_string(),
        // v7.15.0 — TEXT[]/INT[]/BIGINT[] render as their PG-side
        // canonical text form so fixtures can assert on
        // array-returning functions (`show_trgm`, `string_to_array`,
        // …) instead of debug-format strings.
        Value::TextArray(items) => spg_engine::eval::format_text_array(items),
        Value::IntArray(items) => spg_engine::eval::format_int_array(items),
        Value::BigIntArray(items) => spg_engine::eval::format_bigint_array(items),
        // v7.38 P1 (轴 1 SQL conformance closure) — Inet / Cidr /
        // Macaddr / Macaddr8 render as their PG-canonical text form
        // (`10.0.0.5`, `10.0.0.0/8`, `00:50:56:c0:00:01`). Previously
        // fell through to the Debug-format fallback which leaked
        // `Inet { family: 4, bits: 32, addr: [10,0,0,5,…] }` into
        // sqllogictest output, failing pg_regress 35_inet_types.
        Value::Inet { family, bits, addr } => spg_engine::format_inet(*family, *bits, addr),
        Value::Cidr { family, bits, addr } => spg_engine::format_inet(*family, *bits, addr),
        Value::Macaddr(m) => spg_engine::format_macaddr(m),
        Value::Macaddr8(m) => spg_engine::format_macaddr8(m),
        // v7.5.0 — Value is #[non_exhaustive]; Debug-form fallback
        // for any future variant.
        // v7.39 — canonical PG text for every remaining variant.
        _ => spg_engine::eval::value_to_text(v),
    }
}

/// SQLite sqllogictest renders real numbers to three decimal places.
fn format_real(x: f64) -> String {
    // v7.39 (corpus reconcile) — render float8 exactly as PG's float8out
    // (and SPG's pgwire) does. The old SQLite-style fixed "{x:.3}" made
    // the grid disagree with every psql-derived expected value, which
    // seeded fixtures with three-decimal artefacts ("20.000") that then
    // masked real engine wins as failures.
    spg_engine::eval::format_float(x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_str;

    fn run_text(src: &str) -> RunOutcome {
        let records = parse_str(src).expect("parse");
        let mut r = Runner::new();
        r.run(&records)
    }

    #[test]
    fn statement_ok_pipeline() {
        let out = run_text(
            "statement ok\nCREATE TABLE t (a INT)\n\nstatement ok\nINSERT INTO t VALUES (1)\n",
        );
        assert_eq!(out.pass, 2);
        assert_eq!(out.fail, 0);
    }

    #[test]
    fn statement_error_must_actually_error() {
        let out = run_text("statement error\nNOT VALID SQL\n");
        assert_eq!(out.pass, 1);
        let out2 = run_text("statement error\nSELECT 1 FROM nope\n");
        // SELECT against unknown table is an error → matches expected.
        assert_eq!(out2.pass, 1);
        // ok-when-error-expected fails:
        let out3 = run_text("statement error\nSELECT 1\n");
        // SELECT 1 itself: SPG requires FROM, so it actually errors. Use a
        // construct that does succeed: CREATE TABLE.
        let _ = out3;
        let out4 = run_text("statement error\nCREATE TABLE z (a INT)\n");
        assert_eq!(out4.fail, 1);
    }

    #[test]
    fn query_matches_actual_rows() {
        let out = run_text(
            "statement ok\nCREATE TABLE t (a INT NOT NULL, b TEXT NOT NULL)\n\n\
             statement ok\nINSERT INTO t VALUES (1, 'alice')\n\n\
             statement ok\nINSERT INTO t VALUES (2, 'bob')\n\n\
             query IT rowsort\nSELECT * FROM t\n----\n1\nalice\n2\nbob\n",
        );
        assert_eq!(out.pass, 4);
        assert_eq!(out.fail, 0);
    }

    #[test]
    fn query_row_mismatch_fails() {
        let out = run_text(
            "statement ok\nCREATE TABLE t (a INT NOT NULL)\n\n\
             statement ok\nINSERT INTO t VALUES (1)\n\n\
             query I nosort\nSELECT * FROM t\n----\n42\n",
        );
        assert_eq!(out.fail, 1);
    }

    #[test]
    fn skipif_spg_skips() {
        let out = run_text("skipif spg\nstatement ok\nSELECT some_unknown_thing()\n");
        assert_eq!(out.skip, 1);
        assert_eq!(out.fail, 0);
    }

    #[test]
    fn halt_short_circuits() {
        let out =
            run_text("statement ok\nCREATE TABLE t (a INT)\n\nhalt\n\nstatement ok\nBROKEN SQL\n");
        assert_eq!(out.pass, 2); // CREATE + halt
        assert_eq!(out.fail, 0);
    }

    #[test]
    fn vector_query_renders_with_pgvector_style() {
        let out = run_text(
            "statement ok\nCREATE TABLE emb (id INT NOT NULL, v VECTOR(3) NOT NULL)\n\n\
             statement ok\nINSERT INTO emb VALUES (1, [1.0, 2.0, 3.0])\n\n\
             statement ok\nINSERT INTO emb VALUES (2, [4.0, 5.0, 6.0])\n\n\
             query IT nosort\nSELECT * FROM emb ORDER BY id\n----\n1\n[1,2,3]\n2\n[4,5,6]\n",
        );
        assert_eq!(out.pass, 4, "outcomes: {:#?}", out.per_record);
    }
}

/// r1052 (S2.3) — a pg_regress-shaped block: header with the source
/// line, the query, then `-expected` / `+actual` lines. Readable in a
/// terminal and in a review, which is the entire job.
fn unified_diff(line: usize, sql: &str, expected: &[String], actual: &[String]) -> String {
    let mut out = String::new();
    out.push_str(&format!("@ line {line}\n"));
    for l in sql.lines() {
        out.push_str(&format!("  {l}\n"));
    }
    out.push_str("  ----\n");
    let n = expected.len().max(actual.len());
    for i in 0..n {
        match (expected.get(i), actual.get(i)) {
            (Some(e), Some(a)) if e == a => out.push_str(&format!("  {e}\n")),
            (Some(e), Some(a)) => {
                out.push_str(&format!("- {e}\n"));
                out.push_str(&format!("+ {a}\n"));
            }
            (Some(e), None) => out.push_str(&format!("- {e}\n")),
            (None, Some(a)) => out.push_str(&format!("+ {a}\n")),
            (None, None) => {}
        }
    }
    out
}
