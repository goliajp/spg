//! v7.38.19 — `SELECT … INTO t`, PostgreSQL's other spelling of CTAS.
//!
//! A comment in `ast.rs` has said since v7.38 that CTAS and `SELECT
//! INTO` lower to the same node. Only CTAS ever did: `SELECT i INTO t
//! FROM src` answered `syntax error at or near "INTO"`. The differential
//! found it while measuring what PostgreSQL tags each of the five
//! materialising forms with — a comment describing a capability the code
//! does not have, which is the defect this version spent its day on.
//!
//! Getting it to work took three layers, and each layer was a classifier
//! that decides "is this statement a read" by looking at the first word:
//!
//!   1. the parser, which had no `INTO` after a target list
//!   2. `pgwire`'s `is_read`, which already carried two exceptions for
//!      the same reason — `nextval` answered from a stub, and
//!      `FOR UPDATE` was silently ignored in autocommit
//!   3. the command tag, whose last arm is `other => other.to_string()`,
//!      so the statement was tagged the bare word `SELECT` with no count
//!
//! The `CREATE TABLE … AS` spelling was right at every layer, so the two
//! spellings of one statement disagreed about all three.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> usize {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows.len(),
        other => panic!("{sql}: expected rows, got {other:?}"),
    }
}

fn affected(e: &mut Engine, sql: &str) -> usize {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::CommandOk { affected, .. } => affected,
        other => panic!("{sql}: expected a command, got {other:?}"),
    }
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE src(i int, into_col int)").unwrap();
    e.execute("INSERT INTO src VALUES (1,9),(2,9),(3,9)")
        .unwrap();
    e
}

/// All four spellings PostgreSQL 18.4 accepts, each measured against it:
/// `SELECT 3`, `SELECT 3`, `SELECT 3`, `SELECT 2`.
#[test]
fn every_spelling_creates_the_table_and_reports_the_rows() {
    let mut e = seed();
    assert_eq!(affected(&mut e, "SELECT i INTO t1 FROM src"), 3);
    assert_eq!(rows(&mut e, "SELECT i FROM t1"), 3);

    assert_eq!(affected(&mut e, "SELECT i INTO TEMP t2 FROM src"), 3);
    assert_eq!(rows(&mut e, "SELECT i FROM t2"), 3);

    assert_eq!(affected(&mut e, "SELECT i INTO TABLE t3 FROM src"), 3);
    assert_eq!(rows(&mut e, "SELECT i FROM t3"), 3);

    // The tail binds to the BODY, as it does in PostgreSQL: two rows in,
    // two rows stored.
    assert_eq!(
        affected(&mut e, "SELECT i INTO t4 FROM src ORDER BY i LIMIT 2"),
        2
    );
    assert_eq!(rows(&mut e, "SELECT i FROM t4"), 2);

    assert_eq!(affected(&mut e, "SELECT i INTO UNLOGGED t5 FROM src"), 3);
    assert_eq!(rows(&mut e, "SELECT i FROM t5"), 3);
}

/// The negative controls, and they are the reason the classifiers match
/// a WHOLE WORD rather than a substring. Each of these is an ordinary
/// read and must stay one.
#[test]
fn a_column_named_into_something_is_still_a_read() {
    let mut e = seed();
    assert_eq!(rows(&mut e, "SELECT into_col FROM src"), 3);
    assert_eq!(rows(&mut e, "SELECT i FROM src WHERE into_col = 9"), 3);
    e.execute("CREATE TABLE points_into(i int)").unwrap();
    e.execute("INSERT INTO points_into VALUES (1)").unwrap();
    assert_eq!(rows(&mut e, "SELECT i FROM points_into"), 1);
    // And an ordinary SELECT is untouched.
    assert_eq!(rows(&mut e, "SELECT count(*) FROM src"), 1);
}

/// The two spellings must agree with each other, which is the property
/// that was broken at all three layers.
#[test]
fn the_two_spellings_agree() {
    let mut e = seed();
    let a = affected(&mut e, "SELECT i INTO one FROM src");
    let b = affected(&mut e, "CREATE TABLE two AS SELECT i FROM src");
    assert_eq!(a, b);
    assert_eq!(
        rows(&mut e, "SELECT i FROM one"),
        rows(&mut e, "SELECT i FROM two")
    );
}
