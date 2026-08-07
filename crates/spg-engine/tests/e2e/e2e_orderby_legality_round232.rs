//! v7.39 (round 232) — the ORDER BY legality rules PG enforces before it
//! sorts. A set-operation / ordering sweep against live PG18.4
//! (2026-07-19) found SPG accepting all of these and answering something,
//! where PG rejects each by name (42P10):
//!
//!   * `ORDER BY <n>` past the end of the select list — SPG kept the
//!     integer literal as the sort key, so the term tied every row and
//!     vanished, and the query came back in table order;
//!   * `SELECT DISTINCT … ORDER BY <expr not in the select list>` — the
//!     de-duplication happens before the sort, so the key has no defined
//!     value;
//!   * `DISTINCT ON (…) … ORDER BY <key outside the ON list first>` —
//!     which row survives per group is decided by that ordering.
//!
//! The set-operation arity message also takes PG's wording.

use spg_engine::{Engine, QueryResult};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a int, b text)").unwrap();
    e.execute("INSERT INTO t VALUES (1,'x'),(2,'y'),(2,'y'),(3,NULL),(NULL,'z')")
        .unwrap();
    e
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

fn ok(e: &mut Engine, sql: &str) {
    match e.execute(sql) {
        Ok(QueryResult::Rows { .. }) => {}
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn out_of_range_order_by_position_is_rejected() {
    let mut e = seeded();
    for sql in [
        "SELECT a FROM t ORDER BY 2",
        "SELECT a, b FROM t ORDER BY 3",
        "SELECT a FROM t UNION SELECT a FROM t ORDER BY 2",
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains("is not in select list"), "{sql}: {got}");
    }
    // In-range positions still sort.
    ok(&mut e, "SELECT a, b FROM t ORDER BY 2");
    ok(&mut e, "SELECT a FROM t UNION SELECT a FROM t ORDER BY 1");
    // A wildcard's width isn't known here, so those are left alone rather
    // than guessed at.
    ok(&mut e, "SELECT * FROM t ORDER BY 2");
}

#[test]
fn select_distinct_sort_keys_must_be_output_columns() {
    let mut e = seeded();
    for sql in [
        "SELECT DISTINCT a FROM t ORDER BY b",
        "SELECT DISTINCT a FROM t ORDER BY a + 1",
    ] {
        let got = err(&mut e, sql);
        assert!(
            got.contains("for SELECT DISTINCT, ORDER BY expressions must appear in select list"),
            "{sql}: {got}"
        );
    }
    // The legal shapes: the key itself, its output alias, a position, and
    // a second output column.
    ok(&mut e, "SELECT DISTINCT a FROM t ORDER BY a");
    ok(&mut e, "SELECT DISTINCT a AS z FROM t ORDER BY z");
    ok(&mut e, "SELECT DISTINCT a FROM t ORDER BY 1");
    ok(&mut e, "SELECT DISTINCT a, b FROM t ORDER BY b");
    // Without DISTINCT a non-output sort key is fine (PG allows it).
    ok(&mut e, "SELECT a FROM t ORDER BY b");
}

#[test]
fn distinct_on_must_lead_the_ordering() {
    let mut e = seeded();
    let got = err(&mut e, "SELECT DISTINCT ON (a) a, b FROM t ORDER BY b");
    assert!(
        got.contains("SELECT DISTINCT ON expressions must match initial ORDER BY expressions"),
        "{got}"
    );
    let got = err(&mut e, "SELECT DISTINCT ON (a) a, b FROM t ORDER BY b, a");
    assert!(
        got.contains("must match initial ORDER BY expressions"),
        "{got}"
    );
    // Probed against PG18.4: a leading match is enough, the ON list need
    // not be exhausted, and once it is the remaining keys are free.
    ok(&mut e, "SELECT DISTINCT ON (a) a, b FROM t ORDER BY a, b");
    ok(
        &mut e,
        "SELECT DISTINCT ON (a) a, b FROM t ORDER BY a DESC, b",
    );
    ok(&mut e, "SELECT DISTINCT ON (a, b) a, b FROM t ORDER BY a");
    ok(&mut e, "SELECT DISTINCT ON (a, b) a, b FROM t ORDER BY b");
    ok(
        &mut e,
        "SELECT DISTINCT ON (a, b) a, b FROM t ORDER BY b, a",
    );
    ok(&mut e, "SELECT DISTINCT ON (b) a, b FROM t ORDER BY b, a");
    // No ORDER BY at all is legal.
    ok(&mut e, "SELECT DISTINCT ON (a) a, b FROM t");
}

#[test]
fn set_operation_arity_message_matches_pg() {
    let mut e = seeded();
    let got = err(&mut e, "SELECT a FROM t UNION SELECT a, b FROM t");
    assert_eq!(
        got,
        "unsupported: each UNION query must have the same number of columns"
    );
    let got = err(&mut e, "SELECT a FROM t INTERSECT SELECT a, b FROM t");
    assert!(got.contains("each INTERSECT query must have"), "{got}");
    let got = err(&mut e, "SELECT a FROM t EXCEPT SELECT a, b FROM t");
    assert!(got.contains("each EXCEPT query must have"), "{got}");
}
