//! v7.40.11 — sentori §3.9, verified closed and pinned so it stays
//! closed.
//!
//! Reported against 7.39.11 and still listed open in their 7.40.9
//! ledger: "a correlated scalar subquery drops a `timestamptz`'s zone,
//! in what a client is shown and in `pg_typeof`". Their case G:
//!
//! ```text
//!                        7.39.11                     PG 18.6
//!   Describe        timestamp with time zone    (agreed)
//!   the value       2026-01-01 00:00:00         2026-01-01 00:00:00+00
//!   pg_typeof       timestamp without time zone timestamp with time zone
//! ```
//!
//! So a client was told one type and handed the text of another.
//!
//! Measured on this tree, all three agree with PostgreSQL 18.6 — the
//! 7.39.12 work on `value_to_literal_expr_typed` (which the assignment
//! half of the same report drove) closed the display and introspection
//! halves too. A ledger entry that says open while the defect is fixed
//! moves the case out of everyone's view just as surely as one that
//! says closed while it is open, so this is the evidence rather than a
//! claim.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn text(eng: &mut Engine, sql: &str) -> String {
    match eng.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Text(t) => t.to_string(),
            other => panic!("{sql}: {other:?}"),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

fn fixture() -> Engine {
    let mut eng = Engine::new();
    for sql in [
        "CREATE TABLE iss (id INT PRIMARY KEY, first_seen TIMESTAMPTZ)",
        "CREATE TABLE ev (issue_id INT, occurred_at TIMESTAMPTZ)",
        "INSERT INTO iss VALUES (1, NULL), (2, NULL)",
        "INSERT INTO ev VALUES (1, '2026-01-01 00:00:00+00'), (2, '2026-06-01 00:00:00+00')",
    ] {
        eng.execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
    }
    eng
}

/// Their case G: the value, and `pg_typeof`, through a correlated
/// scalar subquery — against the same column read directly.
#[test]
fn a_scalar_subquery_keeps_the_zone_and_the_type() {
    let mut eng = fixture();
    let direct = text(
        &mut eng,
        "SELECT occurred_at::text FROM ev WHERE issue_id = 1",
    );
    let through = text(
        &mut eng,
        "SELECT (SELECT max(e.occurred_at) FROM ev e WHERE e.issue_id = 1)::text",
    );
    assert_eq!(through, direct, "the two roads are one value");
    assert_eq!(
        through, "2026-01-01 00:00:00+00",
        "and it is what PG 18.6 answers"
    );
    assert_eq!(
        text(
            &mut eng,
            "SELECT pg_typeof((SELECT max(e.occurred_at) FROM ev e WHERE e.issue_id = 1))::text"
        ),
        "timestamp with time zone"
    );
}

/// Their case I: which types survive a scalar subquery at all. Four of
/// these six were wrong in 7.39.11.
#[test]
fn every_type_keeps_its_identity_through_a_scalar_subquery() {
    let mut eng = Engine::new();
    eng.execute(
        "CREATE TABLE ty (ts TIMESTAMPTZ, j JSONB, t TEXT, ia BIGINT[], u UUID, \
         n NUMERIC(10,2))",
    )
    .expect("ddl");
    eng.execute(
        "INSERT INTO ty VALUES ('2026-01-01+00', '{\"a\":1}', 'x', ARRAY[1,2]::bigint[], \
         '00000000-0000-0000-0000-000000000001', 1.50)",
    )
    .expect("insert");
    for (col, want) in [
        ("ts", "timestamp with time zone"),
        ("j", "jsonb"),
        ("t", "text"),
        ("ia", "bigint[]"),
        ("u", "uuid"),
        ("n", "numeric"),
    ] {
        assert_eq!(
            text(
                &mut eng,
                &format!("SELECT pg_typeof((SELECT {col} FROM ty))::text")
            ),
            want,
            "pg_typeof through a subquery over {col}"
        );
    }
}

/// Their case A/B: the correlated subquery in ORDER BY, which raised
/// `subquery reached row eval — engine resolver bug` in 7.39.11.
#[test]
fn a_correlated_scalar_subquery_orders_the_rows() {
    let mut eng = fixture();
    let got = match eng
        .execute(
            "SELECT i.id FROM iss i \
             ORDER BY (SELECT max(e.occurred_at) FROM ev e WHERE e.issue_id = i.id) DESC \
             NULLS LAST",
        )
        .expect("no resolver bug")
    {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match r.values[0] {
                Value::Int(n) => n,
                ref o => panic!("{o:?}"),
            })
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(got, vec![2, 1], "later event first");
}

/// Their case H, which is the one that moved stored data: assigning the
/// subquery into a `timestamptz` column under a non-UTC session must
/// not move the instant.
#[test]
fn the_assignment_does_not_move_the_instant() {
    let mut eng = fixture();
    eng.execute("SET TimeZone = 'Asia/Tokyo'").expect("set tz");
    eng.execute(
        "UPDATE iss SET first_seen = (SELECT min(occurred_at) FROM ev WHERE issue_id = 1) \
         WHERE id = 1",
    )
    .expect("update");
    let epoch = match eng
        .execute("SELECT extract(epoch FROM first_seen)::bigint FROM iss WHERE id = 1")
        .expect("read back")
    {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        epoch,
        Value::BigInt(1_767_225_600),
        "the instant must not depend on the session's zone"
    );
}
