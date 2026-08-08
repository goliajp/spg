//! `TABLE t` is PG's spelling of `SELECT * FROM t`, and PG accepts it
//! wherever a SELECT is accepted. SPG had it as a top-level statement
//! only: round 868 put it in a subquery while sampling the readiness
//! doc's claims and got a syntax error, with the CTE form failing
//! differently ("WITH body must be SELECT / …, got Table").
//!
//! Both were routing, not parsing — `parse_table_shorthand` has handed
//! back a desugared `SelectStatement` since the shorthand landed. The
//! nested positions are pinned here alongside the top-level ones,
//! because "shipped" had meant "shipped in the shape we happened to
//! test".

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| r.values.iter().map(|v| format!("{v:?}")).collect())
            .collect(),
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE w (id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO w VALUES (1,10),(2,20)").unwrap();
    e
}

#[test]
fn table_shorthand_is_accepted_wherever_a_select_is() {
    let mut e = seeded();
    let want = rows(&mut e, "SELECT * FROM w ORDER BY id");

    // Top level — these already worked.
    assert_eq!(rows(&mut e, "TABLE w"), want);
    assert_eq!(
        rows(&mut e, "TABLE w ORDER BY id DESC")
            .into_iter()
            .rev()
            .collect::<Vec<_>>(),
        want
    );
    assert_eq!(rows(&mut e, "TABLE w LIMIT 1").len(), 1);

    // Nested — these were the gap.
    assert_eq!(
        rows(&mut e, "SELECT * FROM (TABLE w) t ORDER BY id"),
        want,
        "a derived table may be spelled TABLE t"
    );
    assert_eq!(
        rows(&mut e, "WITH x AS (TABLE w) SELECT * FROM x ORDER BY id"),
        want,
        "a CTE body may be spelled TABLE t"
    );
    assert_eq!(
        rows(&mut e, "WITH x AS (TABLE w) SELECT count(*) FROM x"),
        vec![vec!["BigInt(2)".to_string()]]
    );
}
