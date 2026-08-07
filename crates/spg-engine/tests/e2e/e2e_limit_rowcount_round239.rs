//! v7.39 (round 239) — the row-count clause (LIMIT / OFFSET / FETCH
//! FIRST) and row-expression sweep against live PG18.4 (2026-07-19).
//! Interval arithmetic, justify_*, age, row comparisons and IS [NOT]
//! NULL over rows all matched outright; what the sweep found:
//!
//!   * a FROM-less SELECT IGNORED its LIMIT and OFFSET entirely —
//!     `SELECT 1 LIMIT 0` returned the row (the SRF and aggregate arms
//!     applied them; the scalar tail didn't);
//!   * PG's row count is a bigint with its coercion rules, not an
//!     integer token: `LIMIT 2.5` rounds half away from zero to 3,
//!     `LIMIT '2'` coerces by content, `LIMIT 'a'` is an input-syntax
//!     error on the value — SPG rejected all of these at parse;
//!   * a negative count is PG's "LIMIT must not be negative" (2201W) /
//!     "OFFSET must not be negative" (2201X), not a generic parse error;
//!   * a row-comparison arity mismatch takes PG's wording, "unequal
//!     number of entries in row expressions".

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

#[test]
fn fromless_select_honours_limit_and_offset() {
    let mut e = Engine::new();
    // Both used to be ignored outright on this path.
    assert!(rows(&mut e, "SELECT 1 LIMIT 0").is_empty());
    assert!(rows(&mut e, "SELECT 1 OFFSET 1").is_empty());
    assert_eq!(rows(&mut e, "SELECT 1 LIMIT 5"), ["1"]);
    assert_eq!(rows(&mut e, "SELECT 1 OFFSET 0"), ["1"]);
    // The table paths that already worked must keep working.
    e.execute("CREATE TABLE lt (a int)").unwrap();
    e.execute("INSERT INTO lt VALUES (1),(2),(3)").unwrap();
    assert!(rows(&mut e, "SELECT a FROM lt LIMIT 0").is_empty());
    assert_eq!(
        rows(&mut e, "SELECT a FROM lt ORDER BY a LIMIT 2"),
        ["1", "2"]
    );
}

#[test]
fn row_count_takes_bigint_coercion_not_an_integer_token() {
    let mut e = Engine::new();
    // A numeric rounds half away from zero (PG's numeric→bigint cast).
    assert_eq!(
        rows(&mut e, "SELECT generate_series(1,5) LIMIT 2.5"),
        ["1", "2", "3"]
    );
    assert_eq!(
        rows(&mut e, "SELECT generate_series(1,5) LIMIT 3.5"),
        ["1", "2", "3", "4"]
    );
    assert_eq!(
        rows(&mut e, "SELECT generate_series(1,5) LIMIT 2.2"),
        ["1", "2"]
    );
    assert_eq!(
        rows(&mut e, "SELECT generate_series(1,5) OFFSET 2.5"),
        ["4", "5"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT generate_series(1,3) FETCH FIRST 1.5 ROWS ONLY"
        ),
        ["1", "2"]
    );
    // A string coerces by content; one that won't names the value.
    assert_eq!(
        rows(&mut e, "SELECT generate_series(1,5) LIMIT '2'"),
        ["1", "2"]
    );
    let got = err(&mut e, "SELECT 1 LIMIT 'a'");
    assert!(
        got.contains("invalid input syntax for type bigint: \"a\""),
        "{got}"
    );
}

#[test]
fn negative_row_counts_take_pgs_wording() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT 1 LIMIT -1", "LIMIT must not be negative"),
        ("SELECT 1 LIMIT -2.5", "LIMIT must not be negative"),
        ("SELECT 1 OFFSET -1", "OFFSET must not be negative"),
        // FETCH FIRST shares LIMIT's wording in PG.
        (
            "SELECT 1 FETCH FIRST -1 ROWS ONLY",
            "LIMIT must not be negative",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql}\n  want {want:?}\n  got  {got:?}");
    }
}

#[test]
fn row_expression_arity_mismatch_takes_pgs_wording() {
    let mut e = Engine::new();
    let got = err(&mut e, "SELECT ROW(1,2) = ROW(1,2,3)");
    assert!(
        got.contains("unequal number of entries in row expressions"),
        "{got}"
    );
    // The working row comparisons are untouched.
    assert_eq!(rows(&mut e, "SELECT (ROW(1,2) = ROW(1,2))::text"), ["true"]);
    assert_eq!(rows(&mut e, "SELECT (ROW(1,2) < ROW(2,1))::text"), ["true"]);
    assert_eq!(
        rows(&mut e, "SELECT ((1,5) IN ((1,2),(3,4)))::text"),
        ["false"]
    );
}
