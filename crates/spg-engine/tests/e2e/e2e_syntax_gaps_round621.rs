//! v7.39 (round 621) — four spellings PG takes and SPG refused.
//!
//! Each is small on its own; together they are the shape of what a dump or a
//! generated statement runs into.
//!
//!   * `DROP FUNCTION f() CASCADE` — a parse error. `DROP TABLE` and `DROP
//!     INDEX` have accepted the trailer since v7.14, and pg_dump writes it, so
//!     this failed in the middle of a restore. SPG drops the function either
//!     way; it tracks no dependents to cascade to, which is the reading the
//!     other two already give it;
//!   * `overlaps(s1, e1, s2, e2)` — `function overlaps(date, date, date, date)
//!     does not exist`, while the operator form `(s1,e1) OVERLAPS (s2,e2)`
//!     worked, because the parser lowers that one. The function spelling is
//!     what survives a tool generating SQL from a function catalogue;
//!   * `FROM ONLY t` — read as a table NAMED `only`, so the query failed on
//!     `relation "only" does not exist`. `TRUNCATE ONLY` has taken it as a
//!     no-op since v7.14;
//!   * `CREATE TABLE t ()` — refused as needing at least one column. PG
//!     creates it and `INSERT … DEFAULT VALUES` puts a row in it.
//!
//! The last one had a conformance case asserting the refusal — `# CREATE TABLE
//! with no columns is a parse error` — which was SPG's own behaviour written
//! down as if it were the rule. Measured against live PG18 and corrected
//! there; the gate caught it, which is what it is for.
//!
//! Measured and NOT closed: `CREATE TABLE c () INHERITS (p)` still fails,
//! because table INHERITANCE is not implemented at all — `CREATE TABLE c (b
//! INT) INHERITS (p)` fails the same way. The empty column list was only the
//! first thing it hit. Filed as F14, re-scoped.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
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

/// The DROP trailer pg_dump writes.
#[test]
fn round621_drop_function_takes_cascade() {
    let mut e = Engine::new();
    for tail in ["CASCADE", "RESTRICT", ""] {
        e.execute("CREATE OR REPLACE FUNCTION f1() RETURNS INT AS 'SELECT 1' LANGUAGE SQL")
            .unwrap();
        e.execute(&format!("DROP FUNCTION f1() {tail}"))
            .unwrap_or_else(|err| panic!("DROP FUNCTION f1() {tail}: {err}"));
    }
    e.execute("CREATE OR REPLACE FUNCTION f2() RETURNS INT AS 'SELECT 2' LANGUAGE SQL")
        .unwrap();
    e.execute("DROP FUNCTION IF EXISTS f2() CASCADE").unwrap();
    e.execute("DROP FUNCTION IF EXISTS f2() CASCADE")
        .expect("IF EXISTS is still idempotent with the trailer");
}

/// The function spelling of OVERLAPS, beside the operator one.
#[test]
fn round621_overlaps_as_a_function() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT overlaps(DATE '2020-01-01', DATE '2020-06-01', DATE '2020-03-01', DATE '2020-09-01')"
        ),
        vec!["true"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT (DATE '2020-01-01', DATE '2020-06-01') OVERLAPS (DATE '2020-03-01', DATE '2020-09-01')"
        ),
        vec!["true"],
        "the operator form, which always worked, agrees"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT overlaps(DATE '2020-01-01', DATE '2020-02-01', DATE '2020-03-01', DATE '2020-04-01')"
        ),
        vec!["false"],
        "disjoint periods"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT overlaps(DATE '2020-01-01', DATE '2020-03-01', DATE '2020-03-01', DATE '2020-05-01')"
        ),
        vec!["false"],
        "half-open: touching endpoints do not overlap"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT overlaps(DATE '2020-06-01', DATE '2020-01-01', DATE '2020-03-01', DATE '2020-09-01')"
        ),
        vec!["true"],
        "the endpoints may be given in either order"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT overlaps(NULL::DATE, DATE '2020-06-01', DATE '2020-03-01', DATE '2020-09-01')"
        ),
        vec!["NULL"]
    );
    assert_eq!(
        vals(&mut e, "SELECT overlaps(1, 5, 4, 9), overlaps(1, 5, 6, 9)"),
        vec!["true|false"],
        "and it is not date-only"
    );
}

/// `ONLY`, which named a table before.
#[test]
fn round621_from_only() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE op (a INT)").unwrap();
    e.execute("INSERT INTO op VALUES (1),(2)").unwrap();
    assert_eq!(vals(&mut e, "SELECT count(*) FROM ONLY op"), vec!["2"]);
    assert_eq!(
        vals(&mut e, "SELECT a FROM ONLY op WHERE a > 1"),
        vec!["2"],
        "with the rest of the query around it"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM ONLY op o JOIN op p ON o.a = p.a"
        ),
        vec!["2"],
        "and under an alias, in a join"
    );
    e.execute("CREATE TABLE only_ (b INT)").unwrap();
    e.execute("INSERT INTO only_ VALUES (7)").unwrap();
    assert_eq!(
        vals(&mut e, "SELECT b FROM only_"),
        vec!["7"],
        "a table whose name merely starts with the word is untouched"
    );
}

/// A table with no columns at all.
#[test]
fn round621_zero_column_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE empty ()").unwrap();
    e.execute("INSERT INTO empty DEFAULT VALUES").unwrap();
    assert_eq!(vals(&mut e, "SELECT count(*) FROM empty"), vec!["1"]);
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM information_schema.columns WHERE table_name = 'empty'"
        ),
        vec!["0"],
        "no columns is what it says"
    );
    e.execute("INSERT INTO empty DEFAULT VALUES").unwrap();
    assert_eq!(vals(&mut e, "SELECT count(*) FROM empty"), vec!["2"]);
    e.execute("DROP TABLE empty").unwrap();
}
