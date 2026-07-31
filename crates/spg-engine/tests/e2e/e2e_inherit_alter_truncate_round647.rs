//! v7.39 (round 647) — the two statements that still said "inheritance
//! is not a thing here", and the oracle leak that hid a third.
//!
//! `ALTER TABLE c INHERIT p` and `NO INHERIT p` were accepted and
//! ignored since v7.37.18, whose comment gave the reason: SPG has no
//! PG-style inheritance. Round 645 gave it one and the reason went
//! stale — leaving `NO INHERIT` reporting success while the child
//! stayed attached, which is the worst shape a statement can have. The
//! catalog and the answer disagreed and nothing said so.
//!
//! `TRUNCATE` had the mirror of it. `ONLY` was absorbed as a no-op since
//! v7.14, on the same reasoning `FROM ONLY` used until round 644.
//! Measured on PG18: a plain TRUNCATE of a parent empties its children
//! too, `TRUNCATE ONLY <inheritance parent>` empties the parent alone,
//! and `TRUNCATE ONLY <partitioned parent>` is not a no-op but an error
//! — a partitioned parent holds nothing, so the spelling can only be a
//! mistake.
//!
//! The round also found a second leak in the differential oracle. Round
//! 642 established that its global state has to be part of the corpus
//! reset; `DROP SCHEMA public CASCADE` takes tables, types and
//! functions, but a PUBLICATION is not schema-scoped and survived —
//! which is how `p543` sat there for a hundred rounds. The runner drops
//! them now. A composite type left by a probe under the name `pt` cost
//! this round a measurement too, though that one the reset would have
//! caught.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

fn family() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ip (a INT)").unwrap();
    e.execute("CREATE TABLE ic () INHERITS (ip)").unwrap();
    e.execute("INSERT INTO ip VALUES (1)").unwrap();
    e.execute("INSERT INTO ic VALUES (2)").unwrap();
    e
}

#[test]
fn round647_no_inherit_actually_detaches() {
    let mut e = family();
    assert_eq!(one(&mut e, "SELECT count(*) FROM ip"), "2");
    e.execute("ALTER TABLE ic NO INHERIT ip").unwrap();
    // The parent stops seeing the child's rows…
    assert_eq!(one(&mut e, "SELECT count(*) FROM ip"), "1");
    // …the child keeps everything it had…
    assert_eq!(one(&mut e, "SELECT count(*) FROM ic"), "1");
    // …and the catalog agrees, which is what used to be false.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_inherits i JOIN pg_class p ON p.oid = i.inhparent \
             WHERE p.relname = 'ip'"
        ),
        "0"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT relhassubclass FROM pg_class WHERE relname = 'ip'"
        ),
        "false"
    );
}

#[test]
fn round647_inherit_attaches_again() {
    let mut e = family();
    e.execute("ALTER TABLE ic NO INHERIT ip").unwrap();
    e.execute("ALTER TABLE ic INHERIT ip").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM ip"), "2");
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_inherits i JOIN pg_class p ON p.oid = i.inhparent \
             WHERE p.relname = 'ip'"
        ),
        "1"
    );
}

#[test]
fn round647_detaching_what_is_not_attached_is_an_error() {
    let mut e = family();
    e.execute("CREATE TABLE other (a INT)").unwrap();
    assert!(
        e.execute("ALTER TABLE ic NO INHERIT other").is_err(),
        "ic does not inherit from other"
    );
    // …and attaching twice is refused rather than duplicated.
    assert!(
        e.execute("ALTER TABLE ic INHERIT ip").is_err(),
        "already a child of ip"
    );
    // A missing parent is an error either way.
    assert!(e.execute("ALTER TABLE ic INHERIT nosuch").is_err());
}

#[test]
fn round647_detaching_one_parent_leaves_the_others() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE pa (a INT)").unwrap();
    e.execute("CREATE TABLE pb (b INT)").unwrap();
    e.execute("CREATE TABLE kid () INHERITS (pa, pb)").unwrap();
    e.execute("ALTER TABLE kid NO INHERIT pa").unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT p.relname FROM pg_inherits i JOIN pg_class p ON p.oid = i.inhparent \
             WHERE i.inhrelid = (SELECT oid FROM pg_class WHERE relname = 'kid')"
        ),
        "pb"
    );
}

#[test]
fn round647_truncate_reaches_the_children() {
    let mut e = family();
    e.execute("TRUNCATE ip").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM ip"), "0");
    assert_eq!(one(&mut e, "SELECT count(*) FROM ic"), "0");
}

#[test]
fn round647_truncate_only_stops_at_the_parent() {
    let mut e = family();
    e.execute("TRUNCATE ONLY ip").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM ONLY ip"), "0");
    assert_eq!(one(&mut e, "SELECT count(*) FROM ic"), "1");
}

/// A partitioned parent holds nothing, so the spelling can only be a
/// mistake — and PG says so rather than accepting it.
#[test]
fn round647_truncate_only_a_partitioned_parent_is_refused() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE pt (k INT) PARTITION BY RANGE (k)")
        .unwrap();
    e.execute("CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (10)")
        .unwrap();
    e.execute("INSERT INTO pt VALUES (1)").unwrap();
    let err = e
        .execute("TRUNCATE ONLY pt")
        .expect_err("PG refuses this outright");
    assert!(
        err.to_string()
            .contains("cannot truncate only a partitioned table"),
        "unexpected message: {err}"
    );
    assert_eq!(one(&mut e, "SELECT count(*) FROM pt"), "1");
    // Without ONLY it empties the tree.
    e.execute("TRUNCATE pt").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM pt"), "0");
}

#[test]
fn round647_a_table_named_only_still_truncates() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE only (a INT)").unwrap();
    e.execute("INSERT INTO only VALUES (1)").unwrap();
    e.execute("TRUNCATE only").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM only"), "0");
}
