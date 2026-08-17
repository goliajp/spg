//! r1053 — a subquery in the SELECT list describes (sentori report 4,
//! their third Describe wall, steps 30/86).
//!
//! `SELECT (SELECT 1) AS one` described as NOTHING — any scalar
//! subquery or EXISTS among the items collapsed the whole answer to
//! empty, and sqlx sizes rows by Describe. Three of sentori's four
//! affected statements are `EXISTS(…)` as the entire select list; the
//! fourth mixes FILTER aggregates with a scalar subquery.

use spg_engine::Engine;

fn describe(e: &Engine, sql: &str) -> Vec<(String, spg_storage::DataType)> {
    let stmt = spg_sql::parser::parse_statement(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let (_, cols) = e.describe_prepared(&stmt);
    cols.into_iter().map(|c| (c.name, c.ty)).collect()
}

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ps (id INT PRIMARY KEY, status TEXT, reason TEXT)")
        .unwrap();
    e
}

/// The whole-repro statement, and the EXISTS spelling of it.
#[test]
fn r1053_bare_subquery_items_describe() {
    let e = engine();
    use spg_storage::DataType as T;
    assert_eq!(
        describe(&e, "SELECT (SELECT 1) AS one"),
        [("one".to_string(), T::Int)]
    );
    assert_eq!(
        describe(&e, "SELECT EXISTS (SELECT 1 FROM ps) AS e"),
        [("e".to_string(), T::Bool)]
    );
    // Unaliased: PG names an EXISTS item `exists`, a bare scalar
    // subquery by its inner column.
    assert_eq!(
        describe(&e, "SELECT EXISTS (SELECT 1 FROM ps)"),
        [("exists".to_string(), T::Bool)]
    );
    assert_eq!(
        describe(&e, "SELECT (SELECT status FROM ps LIMIT 1)"),
        [("status".to_string(), T::Text)]
    );
}

/// The step-30 shape: FILTER aggregates beside a scalar subquery with
/// GROUP BY / ORDER BY / LIMIT inside.
#[test]
fn r1053_the_readiness_shape_describes_all_three() {
    let e = engine();
    use spg_storage::DataType as T;
    let cols = describe(
        &e,
        "SELECT count(*) FILTER (WHERE status = 'sent') AS sent, \
                count(*) FILTER (WHERE status = 'failed') AS failed, \
                (SELECT reason FROM ps GROUP BY 1 ORDER BY count(*) DESC LIMIT 1) AS top_reason \
         FROM ps",
    );
    assert_eq!(
        cols,
        [
            ("sent".to_string(), T::BigInt),
            ("failed".to_string(), T::BigInt),
            ("top_reason".to_string(), T::Text),
        ]
    );
}

/// A scalar subquery whose shape cannot be determined keeps the honest
/// NoData answer rather than guessing.
#[test]
fn r1053_undescribable_inner_stays_nodata() {
    let e = engine();
    // Two columns inside a scalar subquery is invalid at EXECUTION
    // time; describe must not invent a single-column answer for it.
    assert!(describe(&e, "SELECT (SELECT id, status FROM ps LIMIT 1)").is_empty());
}
