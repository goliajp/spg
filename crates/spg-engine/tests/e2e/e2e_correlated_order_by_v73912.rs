//! v7.39.12 — a correlated scalar subquery in `ORDER BY` answers.
//!
//! Reported by sentori against 7.39.11, and it predates it:
//!
//! ```text
//!   SELECT i.id FROM issues i
//!    ORDER BY (SELECT max(e.occurred_at) FROM events e
//!               WHERE e.issue_id = i.id) DESC NULLS LAST
//!   ERROR:  subquery reached row eval — engine resolver bug
//! ```
//!
//! The message names itself. An UNCORRELATED subquery in `ORDER BY` is
//! replaced by a literal before execution; a correlated one cannot be,
//! because it has a different value per row, so it reached the per-row
//! evaluator — the one place that cannot run a subquery.
//!
//! It is the ordering `backfill_split` uses, a shipped subcommand of
//! theirs, and the statement raised rather than mis-sorting. Their own
//! narrowing is the negative-control list below: everything adjacent
//! already answered, so it is the correlation AND the `ORDER BY`
//! position together, neither alone.
//!
//! Every expectation is PostgreSQL 18.6's own answer for the same data.

use spg_engine::{Engine, QueryResult};

fn ids(e: &mut Engine, sql: &str) -> String {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    rows.iter()
        .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
        .collect::<Vec<_>>()
        .join(",")
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE issues (id int)",
        "INSERT INTO issues VALUES (1),(2),(3)",
        "CREATE TABLE events (issue_id int, occurred_at timestamptz)",
        "INSERT INTO events VALUES (1,'2026-01-01'),(2,'2026-02-01'),(2,'2026-03-01')",
    ] {
        e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"));
    }
    e
}

const CORR: &str = "(SELECT max(e.occurred_at) FROM events e WHERE e.issue_id = i.id)";

#[test]
fn their_statement_answers_and_agrees_with_postgresql() {
    let mut e = seeded();
    assert_eq!(
        ids(
            &mut e,
            &format!("SELECT i.id FROM issues i ORDER BY {CORR} DESC NULLS LAST LIMIT 1")
        ),
        "2"
    );
}

#[test]
fn the_same_ordering_unlimited() {
    let mut e = seeded();
    assert_eq!(
        ids(
            &mut e,
            &format!("SELECT i.id FROM issues i ORDER BY {CORR} DESC NULLS LAST")
        ),
        "2,1,3"
    );
}

#[test]
fn ascending_with_no_nulls_clause() {
    // PG's default for ASC is NULLS LAST, so issue 3 (no events) is
    // last: 1,2,3.
    let mut e = seeded();
    assert_eq!(
        ids(
            &mut e,
            &format!("SELECT i.id FROM issues i ORDER BY {CORR}")
        ),
        "1,2,3"
    );
}

#[test]
fn the_adjacent_shapes_that_already_answered_still_do() {
    // sentori's own narrowing, kept as the control: if one of these
    // ever breaks, the fix reached further than the defect.
    let mut e = seeded();
    // The same correlated subquery in the SELECT list.
    assert_eq!(
        ids(
            &mut e,
            &format!("SELECT {CORR}::text FROM issues i ORDER BY i.id")
        ),
        "2026-01-01 00:00:00,2026-03-01 00:00:00,NULL"
    );
    // An uncorrelated subquery in ORDER BY.
    assert_eq!(
        ids(
            &mut e,
            "SELECT i.id FROM issues i ORDER BY (SELECT max(occurred_at) FROM events), i.id"
        ),
        "1,2,3"
    );
    // A correlated subquery in WHERE.
    assert_eq!(
        ids(
            &mut e,
            "SELECT i.id FROM issues i \
             WHERE (SELECT count(*) FROM events e WHERE e.issue_id = i.id) > 1"
        ),
        "2"
    );
}

#[test]
fn an_ordinary_order_by_is_unchanged() {
    // The other control: the per-row resolution must not touch a
    // statement whose keys hold no subquery.
    let mut e = seeded();
    assert_eq!(
        ids(&mut e, "SELECT id FROM issues ORDER BY id DESC"),
        "3,2,1"
    );
    assert_eq!(
        ids(&mut e, "SELECT id FROM issues ORDER BY id LIMIT 2"),
        "1,2"
    );
}
