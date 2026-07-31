//! v7.39 (round 641) — what `xid` refuses is the type.
//!
//! Round 640 gave the transaction id an identity; this is the half that
//! identity is made of. PG gives `xid` equality and hashing and **no
//! ordering operator at all** — not because the feature is missing, but
//! because the counter wraps, so `<` between two ids does not mean what
//! it reads like. PG declines rather than answer wrongly. SPG, whose
//! `Value::Xid` carries a `u32` that compares perfectly well, answered
//! all of it: `xid < xid`, `min(xid)`, `greatest(xid, xid)`, `BETWEEN`.
//!
//! In the other direction SPG was too strict. PG has `xideqint4`, so
//! `'1'::xid = 1` answers there and errored here — and it has NO
//! commutator, so `1 = '1'::xid` is an error on PG too. Matching the
//! asymmetry is not pedantry: a query written the reversed way would
//! work here and break on the database SPG stands in for.
//!
//! Every expectation below is a PG18 reading, including the wording.
//!
//! Deliberately NOT matched, and recorded rather than quietly kept:
//!
//!   * `count(DISTINCT xid)` answers here and errors on PG. PG's
//!     DISTINCT aggregate sorts, so it needs an ordering operator; SPG's
//!     hashes, so it needs only equality — which xid has, and which
//!     carries no wraparound hazard. This is a superset with no
//!     correctness risk, so it stays.
//!   * `array_agg(x ORDER BY x)` over xid still answers. That one DOES
//!     need an order and should refuse; the refusal has to be threaded
//!     through six separate aggregate-sort call sites, which is the
//!     duplication F32 already tracks. Converge those first.
//!   * `'1'::xid = '1'::text` answers here and errors on PG. SPG models
//!     an unknown literal as Text and cannot tell it from a real
//!     `::text`; PG accepts the first and refuses the second. That gap
//!     is the unknown-type model's, not xid's.

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

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(err) => err.to_string(),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

#[test]
fn round641_xid_has_no_ordering_operator() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT '1'::xid < '2'::xid", "operator does not exist: xid < xid"),
        ("SELECT '1'::xid <= '2'::xid", "operator does not exist: xid <= xid"),
        ("SELECT '1'::xid > '2'::xid", "operator does not exist: xid > xid"),
        ("SELECT '1'::xid >= '2'::xid", "operator does not exist: xid >= xid"),
        // BETWEEN is `>=` and `<=`, and fails on the first of them.
        (
            "SELECT '2'::xid BETWEEN '1'::xid AND '3'::xid",
            "operator does not exist: xid >= xid",
        ),
        // Ordering against an integer is refused too — only equality
        // crosses the type boundary.
        ("SELECT '1'::xid < 2", "operator does not exist: xid < integer"),
    ] {
        assert!(
            err(&mut e, sql).contains(want),
            "{sql}: wanted {want}, got {}",
            err(&mut e, sql)
        );
    }
}

#[test]
fn round641_xid_has_equality_including_against_an_integer() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT '1'::xid = '1'::xid"), "true");
    assert_eq!(one(&mut e, "SELECT '1'::xid <> '2'::xid"), "true");
    // PG's `xideqint4`, which SPG refused.
    assert_eq!(one(&mut e, "SELECT '1'::xid = 1"), "true");
    assert_eq!(one(&mut e, "SELECT '1'::xid <> 2"), "true");
    // …with no commutator, exactly as PG has none.
    assert!(
        err(&mut e, "SELECT 1 = '1'::xid").contains("operator does not exist: integer = xid"),
        "the reversed spelling must fail the way it fails on PG"
    );
    // An unknown-typed literal reads through the type's input function
    // on either side; both of these answer on PG.
    assert_eq!(one(&mut e, "SELECT '1'::xid = '1'"), "true");
    assert_eq!(one(&mut e, "SELECT '1' = '1'::xid"), "true");
    assert_eq!(one(&mut e, "SELECT '1'::xid IN ('1','2')"), "true");
    // NULL is still NULL, not an operator error.
    assert_eq!(one(&mut e, "SELECT '1'::xid = NULL"), "NULL");
    assert_eq!(one(&mut e, "SELECT '1'::xid IS NULL"), "false");
}

#[test]
fn round641_no_extreme_of_a_transaction_id() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE xo (a xid)").unwrap();
    e.execute("INSERT INTO xo VALUES ('3'), ('1'), ('2')").unwrap();
    assert!(err(&mut e, "SELECT min(a) FROM xo").contains("function min(xid) does not exist"));
    assert!(err(&mut e, "SELECT max(a) FROM xo").contains("function max(xid) does not exist"));
    // GREATEST / LEAST get PG's other wording, because PG looks for a
    // comparison function rather than an aggregate.
    for sql in [
        "SELECT greatest('1'::xid, '2'::xid)",
        "SELECT least('1'::xid, '2'::xid)",
    ] {
        assert!(
            err(&mut e, sql).contains("could not identify a comparison function for type xid"),
            "{sql}: got {}",
            err(&mut e, sql)
        );
    }
    // Equality-only shapes keep working: hash grouping, DISTINCT, and a
    // hash join on the id all need nothing but `=`.
    assert_eq!(one(&mut e, "SELECT a FROM xo GROUP BY a ORDER BY a::text"), "1,2,3");
    assert_eq!(one(&mut e, "SELECT count(*) FROM (SELECT DISTINCT a FROM xo) q"), "3");
    assert_eq!(one(&mut e, "SELECT count(*) FROM xo x JOIN xo y ON x.a = y.a"), "3");
}

#[test]
fn round641_no_cast_between_an_integer_and_a_transaction_id() {
    let mut e = Engine::new();
    // PG has no such cast in either direction. The unknown-literal
    // spelling is a different thing — that is the input function.
    assert!(err(&mut e, "SELECT 1::xid").contains("cannot cast type integer to xid"));
    assert!(err(&mut e, "SELECT 1::BIGINT::xid").contains("cannot cast type bigint to xid"));
    assert_eq!(one(&mut e, "SELECT '1'::xid::text"), "1");
    // …and the reverse is refused as it was before this round.
    assert!(err(&mut e, "SELECT '1'::xid::int").contains("cannot cast"));
}

#[test]
fn round641_the_refusals_are_xids_alone() {
    let mut e = Engine::new();
    // The guard sits in the shared comparison path, so the neighbours
    // that DO have an ordering must still have it. `xid8` is the whole
    // point of the contrast: 64 bits, monotonic, so ordering means
    // something and PG gives it the full set.
    assert_eq!(one(&mut e, "SELECT '1'::xid8 < '2'::xid8"), "true");
    assert_eq!(one(&mut e, "SELECT 1 < 2"), "true");
    assert_eq!(one(&mut e, "SELECT 'a' < 'b'"), "true");
    assert_eq!(one(&mut e, "SELECT greatest(1, 2)"), "2");
    assert_eq!(one(&mut e, "SELECT min(x) FROM (SELECT 1 x UNION ALL SELECT 2) q"), "1");
}
