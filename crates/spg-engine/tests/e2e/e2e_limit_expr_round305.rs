//! v7.39 (round 305, V23) — a non-constant row count.
//!
//! `LIMIT (SELECT 4)` / `LIMIT greatest(2,3)` parsed and were then
//! refused ("LIMIT over a non-constant expression is not yet
//! supported"). PG evaluates the clause once, before the query runs, so
//! any expression is legal as long as it yields a single value and
//! mentions no column. Round 284 had already folded the constant family;
//! the residual was recorded as V23.
//!
//! It was held back for a reason, not for difficulty: every executor
//! reads the row count as `Option<u32>` and takes `None` for "no limit",
//! so an unresolved expression reaching execution would not error — it
//! would silently return the whole table. The nesting tests below exist
//! for that: each one puts a non-constant clause in a different position
//! and checks the row count actually took effect.
//!
//! Expected values all measured against live PG 18.4 (2026-07-21).

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

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t23 (id int, v int)").unwrap();
    e.execute(
        "INSERT INTO t23 VALUES (1,10),(2,20),(3,30),(4,40),(5,50),\
         (6,60),(7,70),(8,80),(9,90),(10,100)",
    )
    .unwrap();
    e
}

#[test]
fn subquery_and_function_row_counts() {
    let mut e = seeded();
    assert_eq!(
        rows(&mut e, "SELECT id FROM t23 ORDER BY id LIMIT (SELECT 4)"),
        ["1", "2", "3", "4"]
    );
    assert_eq!(
        rows(&mut e, "SELECT id FROM t23 ORDER BY id LIMIT greatest(2,3)"),
        ["1", "2", "3"]
    );
    // OFFSET takes the same grammar.
    assert_eq!(
        rows(&mut e, "SELECT id FROM t23 ORDER BY id OFFSET (SELECT 7)"),
        ["8", "9", "10"]
    );
    // Both clauses non-constant at once, in both orders — round 314
    // (V39) taught the parser the second one, which this round had to
    // spell around.
    assert_eq!(
        rows(
            &mut e,
            "SELECT id FROM t23 ORDER BY id LIMIT (SELECT 3) OFFSET (SELECT 2)"
        ),
        ["3", "4", "5"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT id FROM t23 ORDER BY id OFFSET (SELECT 2) LIMIT (SELECT 3)"
        ),
        ["3", "4", "5"]
    );
}

/// PG applies the bigint coercion to the evaluated value too, not just
/// to a literal: the folded and the evaluated form must agree.
#[test]
fn evaluated_row_count_coerces_like_a_constant_one() {
    let mut e = seeded();
    // Half away from zero — same rule round 239 pinned for constants.
    assert_eq!(
        rows(&mut e, "SELECT id FROM t23 ORDER BY id LIMIT (SELECT 2.5)").len(),
        3
    );
    assert_eq!(
        rows(&mut e, "SELECT id FROM t23 ORDER BY id LIMIT (SELECT 2.4)").len(),
        2
    );
    // A string coerces by content.
    assert_eq!(
        rows(&mut e, "SELECT id FROM t23 ORDER BY id LIMIT (SELECT '3')").len(),
        3
    );
    // NULL is PG's "no limit" — not zero rows.
    assert_eq!(rows(&mut e, "SELECT id FROM t23 LIMIT (SELECT NULL)").len(), 10);
    assert_eq!(rows(&mut e, "SELECT id FROM t23 OFFSET (SELECT NULL)").len(), 10);
    // A subquery that returns no row is NULL, so also "no limit".
    assert_eq!(
        rows(&mut e, "SELECT id FROM t23 LIMIT (SELECT 1 WHERE false)").len(),
        10
    );
}

#[test]
fn rejected_row_counts_keep_pgs_wording() {
    let mut e = seeded();
    // The clause is evaluated once, before the scan, so a column has no
    // row to come from. PG names this case specifically.
    assert!(
        err(&mut e, "SELECT id FROM t23 ORDER BY id LIMIT id")
            .contains("argument of LIMIT must not contain variables"),
        "got: {}",
        err(&mut e, "SELECT id FROM t23 ORDER BY id LIMIT id")
    );
    assert!(
        err(&mut e, "SELECT id FROM t23 LIMIT (SELECT -1)")
            .contains("LIMIT must not be negative")
    );
    assert!(
        err(&mut e, "SELECT id FROM t23 OFFSET (SELECT -1)")
            .contains("OFFSET must not be negative")
    );
    assert!(
        err(&mut e, "SELECT id FROM t23 LIMIT (SELECT true)")
            .contains("must be type bigint, not type boolean")
    );
}

/// The whole reason V23 waited for its own round. A non-constant clause
/// in a position the resolution pass fails to reach would not error — it
/// would return every row. One case per nesting shape.
#[test]
fn nested_positions_all_apply_their_row_count() {
    let mut e = seeded();
    // Derived table.
    assert_eq!(
        rows(
            &mut e,
            "SELECT id FROM (SELECT id FROM t23 ORDER BY id LIMIT (SELECT 2)) s"
        )
        .len(),
        2
    );
    // CTE body.
    assert_eq!(
        rows(
            &mut e,
            "WITH c AS (SELECT id FROM t23 ORDER BY id LIMIT (SELECT 3)) SELECT id FROM c"
        )
        .len(),
        3
    );
    // UNION peer — the second branch carries the clause.
    assert_eq!(
        rows(
            &mut e,
            "SELECT id FROM t23 WHERE id = 1 \
             UNION ALL SELECT id FROM (SELECT id FROM t23 ORDER BY id LIMIT (SELECT 2)) u"
        )
        .len(),
        3
    );
    // Subquery buried inside an expression — `IN (...)` is the shape
    // where an unresolved clause would quietly change the answer rather
    // than error, since a wider inner set just matches more rows.
    assert_eq!(
        rows(
            &mut e,
            "SELECT id FROM t23 WHERE id IN \
             (SELECT id FROM t23 ORDER BY id LIMIT (SELECT 2)) ORDER BY id"
        ),
        ["1", "2"]
    );
    // Scalar subquery in the projection.
    assert_eq!(
        rows(
            &mut e,
            "SELECT (SELECT id FROM t23 ORDER BY id LIMIT (SELECT 1))"
        ),
        ["1"]
    );
    // The row-count expression's own subquery carries one too.
    assert_eq!(
        rows(
            &mut e,
            "SELECT id FROM t23 ORDER BY id \
             LIMIT (SELECT id FROM t23 ORDER BY id LIMIT (SELECT 1))"
        ),
        ["1"]
    );
}

/// PG's FETCH FIRST takes a constant or a PARENTHESISED expression —
/// `FETCH FIRST 1+1 ROWS ONLY` is a syntax error, `FETCH FIRST (1+1)` is
/// not. Round 284 recorded only the first half.
#[test]
fn fetch_first_takes_a_parenthesised_expression() {
    let mut e = seeded();
    assert_eq!(
        rows(&mut e, "SELECT id FROM t23 ORDER BY id FETCH FIRST (1+1) ROWS ONLY").len(),
        2
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT id FROM t23 ORDER BY id FETCH FIRST (SELECT 3) ROWS ONLY"
        )
        .len(),
        3
    );
    // Bare arithmetic stays a parse error, as in PG.
    assert!(!err(&mut e, "SELECT id FROM t23 FETCH FIRST 1+1 ROWS ONLY").is_empty());
}

/// Guard for the shape that would be worst to get wrong: an ordinary
/// constant clause must still limit. The resolution pass takes the slot
/// apart to inspect it, and an empty slot means "no limit".
#[test]
fn ordinary_constant_row_counts_still_apply() {
    let mut e = seeded();
    assert_eq!(rows(&mut e, "SELECT id FROM t23 ORDER BY id LIMIT 2"), ["1", "2"]);
    assert_eq!(rows(&mut e, "SELECT id FROM t23 ORDER BY id OFFSET 8"), ["9", "10"]);
    assert!(rows(&mut e, "SELECT id FROM t23 LIMIT 0").is_empty());
}


