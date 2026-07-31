//! v7.39 (round 645) — `CREATE TABLE c (…) INHERITS (p)`.
//!
//! PG table inheritance was a parse error. Round 642 found it behind a
//! corrupted corpus baseline; round 644 fixed `FROM ONLY`, which is the
//! keyword that makes the parent/child distinction expressible. This
//! round builds the feature on the machinery those two left behind.
//!
//! Inheritance differs from a declarative partition in three ways, all
//! measured on PG18 and all of them load-bearing here:
//!
//!   * **The parent holds rows of its own.** A partition parent never
//!     does, so its union is just the children; an inheritance parent
//!     is itself a term — `FROM ONLY parent`, or expanding it would
//!     recurse.
//!   * **`INSERT INTO parent` does not route.** The row stays in the
//!     parent. Routing is keyed on the partition-parent role, which an
//!     inheritance parent does not have, so this is right by default.
//!   * **`DROP TABLE parent` errors.** "cannot drop table par because
//!     other objects depend on it / table ch depends on table par", and
//!     the child survives. Only a partition parent takes its children
//!     with it (round 642).
//!
//! A child may add columns of its own, so the union's terms name the
//! PARENT's columns rather than `*`.
//!
//! Round 645 refused `UPDATE` and `DELETE` through an inheritance
//! parent rather than answer half of them; round 646 does the fan-out
//! and CHECK inheritance. See `e2e_inheritance_dml_round646`.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
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
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn one(e: &mut Engine, sql: &str) -> String {
    rows(e, sql).join(",")
}

fn family() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE par (a INT NOT NULL, b TEXT DEFAULT 'd')")
        .unwrap();
    e.execute("CREATE TABLE ch (c BOOL) INHERITS (par)").unwrap();
    e.execute("INSERT INTO par (a) VALUES (1)").unwrap();
    e.execute("INSERT INTO ch (a, c) VALUES (2, true)").unwrap();
    e
}

#[test]
fn round645_the_child_takes_the_parents_columns_first() {
    let mut e = family();
    assert_eq!(
        one(
            &mut e,
            "SELECT string_agg(attname, ',' ORDER BY attnum) FROM pg_attribute \
             WHERE attrelid = (SELECT oid FROM pg_class WHERE relname = 'ch') AND attnum > 0"
        ),
        "a,b,c"
    );
    // NOT NULL and DEFAULT come with the column.
    assert_eq!(
        rows(
            &mut e,
            "SELECT attname, attnotnull FROM pg_attribute \
             WHERE attrelid = (SELECT oid FROM pg_class WHERE relname = 'ch') \
             AND attnum > 0 ORDER BY attnum"
        ),
        vec!["a|true", "b|false", "c|false"]
    );
    assert_eq!(one(&mut e, "SELECT b FROM ch"), "d");
}

#[test]
fn round645_the_parent_sees_its_own_rows_and_its_childrens() {
    let mut e = family();
    assert_eq!(one(&mut e, "SELECT count(*) FROM par"), "2");
    // The parent's OWN row is the one ONLY returns — a partition parent
    // would answer 0 here.
    assert_eq!(one(&mut e, "SELECT count(*) FROM ONLY par"), "1");
    assert_eq!(one(&mut e, "SELECT count(*) FROM ch"), "1");
    // The parent's shape, not the child's: `c` is not in the output.
    assert_eq!(
        rows(&mut e, "SELECT b, a FROM par ORDER BY a"),
        vec!["d|1", "d|2"]
    );
    // …and each row says which table it lives in.
    assert_eq!(
        rows(
            &mut e,
            "SELECT tableoid::regclass::text, a FROM par ORDER BY a"
        ),
        vec!["par|1", "ch|2"]
    );
}

#[test]
fn round645_insert_into_the_parent_stays_in_the_parent() {
    let mut e = family();
    e.execute("INSERT INTO par (a) VALUES (3)").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM ONLY par"), "2");
    assert_eq!(one(&mut e, "SELECT count(*) FROM ch"), "1");
}

#[test]
fn round645_the_catalogs_record_the_relationship() {
    let mut e = family();
    assert_eq!(
        rows(
            &mut e,
            "SELECT c.relname, p.relname, i.inhseqno FROM pg_inherits i \
             JOIN pg_class c ON c.oid = i.inhrelid JOIN pg_class p ON p.oid = i.inhparent"
        ),
        vec!["ch|par|1"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT relname, relhassubclass FROM pg_class \
             WHERE relname IN ('par','ch') ORDER BY relname"
        ),
        vec!["ch|false", "par|true"]
    );
}

/// Unlike a partition parent, which round 642 made take its children
/// with it.
#[test]
fn round645_dropping_the_parent_errors_and_the_child_survives() {
    let mut e = family();
    let err = e.execute("DROP TABLE par").expect_err("PG refuses this");
    assert!(
        err.to_string()
            .contains("cannot drop table par because other objects depend on it"),
        "unexpected message: {err}"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_class WHERE relname IN ('par','ch')"
        ),
        "2"
    );
}

#[test]
fn round645_multiple_parents_and_their_order() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE pa (a INT)").unwrap();
    e.execute("CREATE TABLE pb (b INT)").unwrap();
    e.execute("CREATE TABLE kid () INHERITS (pa, pb)").unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT string_agg(attname, ',' ORDER BY attnum) FROM pg_attribute \
             WHERE attrelid = (SELECT oid FROM pg_class WHERE relname = 'kid') AND attnum > 0"
        ),
        "a,b"
    );
    // inhseqno is the parent's position in the child's parent list.
    assert_eq!(
        rows(
            &mut e,
            "SELECT p.relname, i.inhseqno FROM pg_inherits i \
             JOIN pg_class p ON p.oid = i.inhparent \
             WHERE i.inhrelid = (SELECT oid FROM pg_class WHERE relname = 'kid') \
             ORDER BY i.inhseqno"
        ),
        vec!["pa|1", "pb|2"]
    );
}

#[test]
fn round645_a_parent_that_does_not_exist_is_an_error() {
    let mut e = Engine::new();
    assert!(
        e.execute("CREATE TABLE orphan (a INT) INHERITS (nosuch)")
            .is_err(),
        "inheriting from a missing table must fail"
    );
}
