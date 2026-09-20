//! 9.0.0 — `DROP TABLE t` succeeded while a view read `t`.
//!
//! The view was left answering `relation "t" does not exist`, and
//! nothing had said so. Measured against PostgreSQL 18.6, which
//! refuses:
//!
//! ```text
//!   ERROR:  cannot drop table t because other objects depend on it
//!   DETAIL:  view v depends on table t
//!           view v2 depends on view v
//!   HINT:  Use DROP ... CASCADE to drop the dependent objects too.
//! ```
//!
//! `CASCADE` was parsed and thrown away, so it had nothing to do; it
//! takes the dependents now and says what it took, as PG does (one
//! object on one line, more than one behind a count).
//!
//! The dependency test is the rename of D6 run to the SAME name, so the
//! two questions cannot drift apart: a reference the rename would
//! follow is exactly a reference DROP must refuse — a CTE of that name
//! and a FROM item aliased to it shadow it in both.

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| spg_engine::eval::value_to_text(r.values.first().expect("one column")))
        .collect()
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE t (a int, b int)",
        "CREATE VIEW v AS SELECT a FROM t",
        "CREATE VIEW v2 AS SELECT count(*) AS n FROM v",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

#[test]
fn a_drop_that_would_break_a_view_is_refused() {
    let mut e = seeded();
    let got = err(&mut e, "DROP TABLE t");
    for want in [
        "cannot drop table t because other objects depend on it",
        "DETAIL:  view v depends on table t",
        "view v2 depends on view v",
        "HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
    ] {
        assert!(got.contains(want), "{got}\n  want: {want}");
    }
    // The table is still there, and so is the view.
    assert_eq!(
        col(&mut e, "SELECT count(*) FROM pg_tables WHERE tablename='t'"),
        vec!["1".to_string()]
    );
    // A view another view reads cannot go either.
    let got = err(&mut e, "DROP VIEW v");
    assert!(
        got.contains("cannot drop view v because other objects depend on it"),
        "{got}"
    );
    assert!(got.contains("DETAIL:  view v2 depends on view v"), "{got}");
}

#[test]
fn cascade_takes_the_dependents_and_says_what_it_took() {
    let mut e = seeded();
    e.execute("DROP TABLE t CASCADE").unwrap();
    let notices: Vec<String> = e.take_notices().into_iter().map(|n| n.message).collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    for want in [
        "drop cascades to 2 other objects",
        "DETAIL:  drop cascades to view v",
        "drop cascades to view v2",
    ] {
        assert!(notices[0].contains(want), "{}\n  want: {want}", notices[0]);
    }
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM pg_views WHERE viewname IN ('v','v2')"
        ),
        vec!["0".to_string()]
    );
}

#[test]
fn a_single_dependent_reads_as_pg_words_it() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t1 (a int)").unwrap();
    e.execute("CREATE VIEW w AS SELECT a FROM t1").unwrap();
    e.execute("DROP TABLE t1 CASCADE").unwrap();
    let notices: Vec<String> = e.take_notices().into_iter().map(|n| n.message).collect();
    assert_eq!(notices, vec!["drop cascades to view w".to_string()]);
}

#[test]
fn a_shadowed_name_is_not_a_dependency() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a int)").unwrap();
    // The view reads a CTE named `t`, not the table, so the table is
    // free to go — as it is on PostgreSQL.
    e.execute("CREATE VIEW vs AS WITH t AS (SELECT 9 AS a) SELECT a FROM t")
        .unwrap();
    e.execute("DROP TABLE t").unwrap();
    assert_eq!(col(&mut e, "SELECT a FROM vs"), vec!["9".to_string()]);
}
