//! 7.38.3 — Describe answers for the predicate shapes, and the inline
//! CHECK on ADD COLUMN.
//!
//! sentori step 41: `SELECT user_key IS NOT NULL AS addressable` came
//! back from Describe with NO COLUMNS, because `describe_expr` had no
//! arm for the null test and fell through to "I cannot type this",
//! which abandons the whole statement's column list. Nested inside
//! another operator the same subexpression described fine — the arm
//! that consumed it never asked what it was. Their report named three
//! shapes; the hole was wider, and this pins the class.

use spg_engine::Engine;

fn described(e: &Engine, sql: &str) -> Vec<String> {
    let stmt = spg_sql::parser::parse_statement(sql).expect("parse");
    let (_, cols) = e.describe_prepared(&stmt);
    cols.iter().map(|c| c.name.clone()).collect()
}

#[test]
fn pin_v7383_predicates_describe_their_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE dq (id INT, k TEXT, h TEXT, n INT)")
        .unwrap();
    // Every one of these is BOOLEAN in PG18 (checked against its own
    // view columns, 2026-08-19). None of them described before.
    for sql in [
        "SELECT id, k IS NOT NULL AS b FROM dq",
        "SELECT id, k IS NULL AS b FROM dq",
        "SELECT id, NOT (k IS NULL) AS b FROM dq",
        "SELECT id, (k = 'a') IS TRUE AS b FROM dq",
        "SELECT id, (k = 'a') IS NOT TRUE AS b FROM dq",
        "SELECT id, k LIKE 'a%' AS b FROM dq",
        "SELECT id, k NOT LIKE 'a%' AS b FROM dq",
        "SELECT id, n IN (1, 2) AS b FROM dq",
        "SELECT id, ~ n AS b FROM dq",
        "SELECT id, + n AS b FROM dq",
    ] {
        assert_eq!(
            described(&e, sql),
            vec!["id".to_string(), "b".to_string()],
            "Describe went silent on: {sql}"
        );
    }
    // Unaliased, PG's own name for a projected expression.
    assert_eq!(
        described(&e, "SELECT id, k IS NOT NULL FROM dq"),
        vec!["id".to_string(), "?column?".to_string()]
    );
    // The shapes that already worked must keep working.
    for sql in [
        "SELECT id, k = 'a' AS b FROM dq",
        "SELECT id, (k IS NOT NULL AND h IS NOT NULL) AS b FROM dq",
        "SELECT id, CASE WHEN k IS NULL THEN false ELSE true END AS b FROM dq",
    ] {
        assert_eq!(described(&e, sql).len(), 2, "{sql}");
    }
}
