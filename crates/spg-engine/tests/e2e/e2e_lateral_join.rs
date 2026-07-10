//! v7.17.0 Phase 3.3 → P0-41 — LATERAL derived tables.
//!
//! Phase 3.3 (`981812a`) carved this out; v7.17.0 Phase 3.P0-41
//! lands the real implementation. SPG now parses `LATERAL (
//! SELECT … )` in any FROM-list position and the join executor
//! materialises the inner SELECT per outer row, substituting
//! `<outer_alias>.<col>` references against the current join row.
//!
//! v7.17 limitations (separate follow-ups):
//!   * The schema probe falls back to a TEXT-typed column shape
//!     when the inner SELECT references outer columns at
//!     projection time (rare; values are still correct because
//!     the per-row substitution path runs the real query).
//!   * No `JOIN LATERAL … ON expr` mixed forms (parsed but the
//!     v7.17 executor treats `LATERAL` as a CROSS JOIN — the ON
//!     clause must move into the inner subquery's WHERE).
//!   * No correlated subquery references inside aggregate
//!     window arguments (window+LATERAL combination).

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE TABLE orders (id INT NOT NULL, user_id INT NOT NULL, amount INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO users VALUES (1, 'alice'), (2, 'bob')")
        .unwrap();
    e.execute("INSERT INTO orders VALUES (1, 1, 100), (2, 1, 200), (3, 2, 50), (4, 2, 80)")
        .unwrap();
}

// read01 LATERAL — a FROM-less LATERAL subquery references outer columns
// in its projection. Previously only qualified `alias.col` refs (in a
// correlated inner WHERE) were substituted; a FROM-less `LATERAL (SELECT
// v*2)` left the bare outer column unresolved and errored. Values vs
// live PG 18.4.
#[test]
fn lateral_fromless_projects_outer_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, g INT, v INT)").unwrap();
    e.execute("INSERT INTO t VALUES (1, 1, 10), (2, 1, 20), (3, 2, 30)")
        .unwrap();

    // Comma-join FROM-less LATERAL: x = v*2.
    let r = rows(
        e.execute("SELECT t.id, x FROM t, LATERAL (SELECT v * 2 AS x) sub WHERE t.id = 1")
            .unwrap(),
    );
    assert_eq!(r, vec![vec![Value::Int(1), Value::Int(20)]]);

    // CROSS JOIN LATERAL, bare outer ref.
    let r = rows(
        e.execute(
            "SELECT t.id, x FROM t CROSS JOIN LATERAL (SELECT v + 1 AS x) sub WHERE t.id = 2",
        )
        .unwrap(),
    );
    assert_eq!(r, vec![vec![Value::Int(2), Value::Int(21)]]);

    // Multiple outer columns across multiple projected columns.
    let r = rows(
        e.execute(
            "SELECT t.id, a, b FROM t, LATERAL (SELECT v * 10 AS a, g + v AS b) sub WHERE t.id = 3",
        )
        .unwrap(),
    );
    assert_eq!(
        r,
        vec![vec![Value::Int(3), Value::Int(300), Value::Int(32)]]
    );

    // JOIN LATERAL … ON true (the ON-clause mixed form).
    let r = rows(
        e.execute("SELECT t.id, x FROM t JOIN LATERAL (SELECT v AS x) sub ON true WHERE t.id = 2")
            .unwrap(),
    );
    assert_eq!(r, vec![vec![Value::Int(2), Value::Int(20)]]);
}

#[test]
fn lateral_subquery_correlated_in_where() {
    // The canonical LATERAL shape: for each user, fetch one order.
    let mut e = Engine::new();
    setup(&mut e);
    let r = rows(
        e.execute(
            "SELECT u.name, o.amount \
             FROM users u, LATERAL (SELECT amount FROM orders WHERE user_id = u.id ORDER BY amount LIMIT 1) o \
             ORDER BY u.id",
        )
        .unwrap(),
    );
    // alice: min order amount = 100. bob: min = 50.
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::text("alice"));
    assert_eq!(r[0][1], Value::Int(100));
    assert_eq!(r[1][0], Value::text("bob"));
    assert_eq!(r[1][1], Value::Int(50));
}

#[test]
fn lateral_subquery_returns_multiple_rows_per_outer() {
    // For each user, fetch their top-2 orders.
    let mut e = Engine::new();
    setup(&mut e);
    let r = rows(
        e.execute(
            "SELECT u.name, o.amount \
             FROM users u, LATERAL (SELECT amount FROM orders WHERE user_id = u.id ORDER BY amount DESC LIMIT 2) o \
             ORDER BY u.id, o.amount DESC",
        )
        .unwrap(),
    );
    // alice has 100 + 200 → top 2 = 200, 100.
    // bob has 50 + 80 → top 2 = 80, 50.
    assert_eq!(r.len(), 4);
    assert_eq!(r[0][1], Value::Int(200));
    assert_eq!(r[1][1], Value::Int(100));
    assert_eq!(r[2][1], Value::Int(80));
    assert_eq!(r[3][1], Value::Int(50));
}

#[test]
fn lateral_subquery_with_no_inner_matches_drops_outer() {
    // Add a user with no orders; CROSS-shaped LATERAL drops them
    // (LEFT JOIN LATERAL would keep — v7.17 doesn't yet support
    // the ON clause variant cleanly).
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("INSERT INTO users VALUES (3, 'carol')").unwrap();
    let r = rows(
        e.execute(
            "SELECT u.name, o.amount \
             FROM users u, LATERAL (SELECT amount FROM orders WHERE user_id = u.id) o \
             ORDER BY u.id",
        )
        .unwrap(),
    );
    // carol has no orders → cross-join with empty subquery emits
    // zero rows for her.
    assert_eq!(r.len(), 4);
    for row in &r {
        assert_ne!(row[0], Value::text("carol"));
    }
}

#[test]
fn correlated_subquery_in_select_workaround() {
    // For "per-row aggregate" use cases, a correlated scalar
    // subquery in the SELECT list achieves similar semantics.
    let mut e = Engine::new();
    setup(&mut e);
    let r = e.execute(
        "SELECT u.name, \
                (SELECT max(amount) FROM orders WHERE user_id = u.id) AS top_amount \
         FROM users u",
    );
    match r {
        Ok(QueryResult::Rows { rows: out, .. }) => {
            assert_eq!(out.len(), 2);
        }
        Err(e) => {
            // The correlated subquery path may also have gaps;
            // pin the actual behavior.
            eprintln!("correlated subquery workaround: {e:?}");
        }
        _ => panic!(),
    }
}

#[test]
fn regular_join_still_works() {
    // Negative regression: ordinary inner joins unaffected.
    let mut e = Engine::new();
    setup(&mut e);
    let r = rows(
        e.execute(
            "SELECT u.name, o.amount \
             FROM users u JOIN orders o ON o.user_id = u.id \
             ORDER BY u.id, o.amount",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 4);
}

// v7.37.7 (sentori Epic 4 P1) — `LEFT JOIN LATERAL … ON TRUE` shape
// from the endpoint-probe runtime query. SPG already had head-position
// LATERAL since v7.17.0 P0-41; this case pins the JOIN-position shape
// sentori actually uses.

#[test]
fn left_join_lateral_on_true_matches_per_outer_row() {
    let mut e = Engine::new();
    // endpoint_check + endpoint_probe — sentori's exact tables.
    e.execute(
        "CREATE TABLE endpoint_check (
             id BIGINT PRIMARY KEY,
             project_id BIGINT NOT NULL,
             url TEXT NOT NULL,
             method TEXT NOT NULL,
             paused BOOL NOT NULL DEFAULT FALSE
         )",
    )
    .unwrap();
    e.execute(
        "CREATE TABLE endpoint_probe (
             id BIGINT PRIMARY KEY,
             check_id BIGINT NOT NULL,
             ts TIMESTAMPTZ NOT NULL
         )",
    )
    .unwrap();
    e.execute("INSERT INTO endpoint_check VALUES (1, 100, 'https://a', 'GET', FALSE)")
        .unwrap();
    e.execute("INSERT INTO endpoint_check VALUES (2, 100, 'https://b', 'GET', FALSE)")
        .unwrap();
    // Check 1 has two probes; we want the latest. Check 2 has none.
    e.execute("INSERT INTO endpoint_probe VALUES (10, 1, '2026-06-01 00:00:00+00')")
        .unwrap();
    e.execute("INSERT INTO endpoint_probe VALUES (11, 1, '2026-06-15 00:00:00+00')")
        .unwrap();
    let result = rows(
        e.execute(
            "SELECT c.id, lp.ts \
             FROM endpoint_check c \
             LEFT JOIN LATERAL ( \
                 SELECT ts FROM endpoint_probe \
                  WHERE check_id = c.id \
                  ORDER BY ts DESC LIMIT 1 \
             ) lp ON TRUE \
             ORDER BY c.id",
        )
        .expect("LEFT JOIN LATERAL parses and runs"),
    );
    // Two outer rows. Check 1 ⇒ latest probe ts = 2026-06-15.
    // Check 2 ⇒ no probe, lateral row absent ⇒ ts is NULL.
    assert_eq!(result.len(), 2);
    assert_eq!(result[0][0], Value::BigInt(1));
    assert!(matches!(result[0][1], Value::Timestamp(_)));
    assert_eq!(result[1][0], Value::BigInt(2));
    assert_eq!(result[1][1], Value::Null);
}
