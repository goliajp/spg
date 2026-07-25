//! read01 round 469 (C11) — TEMPORARY sequences and views were permanent.
//!
//! Round 436 gave temporary TABLES a per-session namespace and round 437
//! taught the catalog listings about them. Sequences and views were left
//! behind: the keyword parsed and was thrown away, so `CREATE TEMPORARY
//! SEQUENCE s1` made a permanent sequence.
//!
//! Measured against a live SPG server before this round, two connections:
//! session B saw session A's temp sequence and view in `pg_class` /
//! `pg_views`, could call `nextval('s1')` on it, and could select from the
//! view. PG18 answers 0 for both and raises `relation "s1" does not exist`.
//!
//! Every expectation below is copied from a PG18 run.

use spg_engine::{Engine, QueryResult};

fn scalar(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql} -> {other:?}"),
    }
}

#[test]
fn round469_a_temp_sequence_shadows_a_permanent_one_and_leaves_it_alone() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE perm").unwrap();
    assert_eq!(scalar(&mut e, "SELECT nextval('perm')"), "1");
    e.execute("CREATE TEMPORARY SEQUENCE perm").unwrap();
    // The temporary one is fresh — it does not inherit the permanent
    // sequence's counter.
    assert_eq!(scalar(&mut e, "SELECT nextval('perm')"), "1");
    e.execute("DROP SEQUENCE perm").unwrap();
    // And the permanent one was never advanced while it was shadowed.
    assert_eq!(scalar(&mut e, "SELECT nextval('perm')"), "2");
}

#[test]
fn round469_a_temp_view_shadows_a_permanent_one() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE base (id INT)").unwrap();
    e.execute("INSERT INTO base VALUES (1),(2),(3)").unwrap();
    e.execute("CREATE VIEW vw AS SELECT id FROM base WHERE id < 3")
        .unwrap();
    assert_eq!(scalar(&mut e, "SELECT count(*) FROM vw"), "2");
    e.execute("CREATE TEMPORARY VIEW vw AS SELECT id FROM base")
        .unwrap();
    assert_eq!(scalar(&mut e, "SELECT count(*) FROM vw"), "3");
    e.execute("DROP VIEW vw").unwrap();
    assert_eq!(scalar(&mut e, "SELECT count(*) FROM vw"), "2");
    // Shadowed or not, the listing shows one `vw` — never the mangled name
    // alongside it.
    assert_eq!(
        scalar(&mut e, "SELECT count(*) FROM pg_views WHERE viewname='vw'"),
        "1"
    );
}

#[test]
fn round469_another_session_sees_neither() {
    // The defect, in the shape it was measured on the wire: a second
    // connection could list and use both.
    let mut e = Engine::new();
    e.set_current_session(1);
    e.execute("CREATE TABLE base (id INT)").unwrap();
    e.execute("INSERT INTO base VALUES (1),(2)").unwrap();
    e.execute("CREATE TEMPORARY SEQUENCE s1").unwrap();
    e.execute("CREATE TEMPORARY VIEW v1 AS SELECT id FROM base")
        .unwrap();
    assert_eq!(
        scalar(&mut e, "SELECT count(*) FROM pg_class WHERE relname='s1'"),
        "1"
    );

    e.set_current_session(2);
    assert_eq!(
        scalar(&mut e, "SELECT count(*) FROM pg_class WHERE relname='s1'"),
        "0",
        "session 2 must not see session 1's temporary sequence"
    );
    assert_eq!(
        scalar(&mut e, "SELECT count(*) FROM pg_views WHERE viewname='v1'"),
        "0",
        "session 2 must not see session 1's temporary view"
    );
    assert!(
        e.execute("SELECT nextval('s1')").is_err(),
        "session 2 must not be able to advance session 1's temporary sequence"
    );
    assert!(
        e.execute("SELECT count(*) FROM v1").is_err(),
        "session 2 must not be able to read session 1's temporary view"
    );
}

#[test]
fn round469_they_die_with_the_session() {
    let mut e = Engine::new();
    e.set_current_session(1);
    e.execute("CREATE TABLE base (id INT)").unwrap();
    e.execute("CREATE TEMPORARY SEQUENCE s1").unwrap();
    e.execute("CREATE TEMPORARY VIEW v1 AS SELECT id FROM base")
        .unwrap();
    e.end_session(1);

    e.set_current_session(2);
    // Nothing left behind under the mangled name either.
    assert_eq!(
        scalar(
            &mut e,
            "SELECT count(*) FROM pg_class WHERE relname LIKE '%s1'"
        ),
        "0"
    );
    assert_eq!(scalar(&mut e, "SELECT count(*) FROM pg_views"), "0");
}
