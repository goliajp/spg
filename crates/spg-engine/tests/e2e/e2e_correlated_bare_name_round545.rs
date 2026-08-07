//! v7.39 (round 545) — a correlated subquery written the ordinary way.
//!
//! SQL resolves an unqualified name innermost-first and walks OUTWARD
//! when the inner scope does not supply it. SPG only ever looked
//! inward, so the plainest correlated subquery there is did not run at
//! all:
//!
//!     SELECT v, (SELECT w FROM ob WHERE bid = aid) FROM oa
//!     PG18  x|B1, y|B2        SPG  ERROR: column "aid" does not exist
//!
//! Only the qualified spelling (`oa.aid`) worked, which is why this
//! survived so long: the tests that exercised correlation, and the
//! catalog queries that had been fixed alongside them, all wrote the
//! qualifier. It surfaced from pg_dump, whose type query correlates on
//! a bare `typrelid`.
//!
//! The fix is one half, not two. `substitute_in_select` splices the
//! outer row's values into the subquery, and it now splices bare names
//! the inner scope does not supply — determined from the catalog, and
//! only when the whole inner scope can be enumerated (a CTE, a view or
//! a set-returning FROM entry makes it unknowable, and an unknowable
//! scope leaves bare names alone).
//!
//! Teaching `select_is_correlated` the same rule was tried and
//! REVERTED: nine tests said no. The runtime already routes a
//! bare-correlated subquery to the per-row path — the pre-resolver
//! hands it back unreplaced — and claiming correlation up front pushed
//! shapes onto a path they are not resolved on
//! (`SELECT pg_typeof((SELECT count(*) FROM (VALUES (1)) b(y)))` came
//! back "subquery reached row eval"). The per-row behaviour is pinned
//! below rather than assumed.
//!
//! And a catalog's own name is rewritten before the engine sees it
//! (`pg_type` becomes `__spg_pg_type`) in the FROM clause but not in a
//! qualifier, so `WHERE te.oid = pg_type.typelem` — how pg_dump asks
//! whether a type is an array type — reported a missing FROM-clause
//! entry for a table that was right there.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE oa (aid INT, v TEXT)").unwrap();
    e.execute("CREATE TABLE ob (bid INT, w TEXT)").unwrap();
    e.execute("INSERT INTO oa VALUES (1, 'x'), (2, 'y')")
        .unwrap();
    e.execute("INSERT INTO ob VALUES (1, 'B1'), (2, 'B2')")
        .unwrap();
    e
}

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

/// The bare outer reference, in every shape a subquery takes.
#[test]
fn round545_bare_outer_column_correlates() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT v, (SELECT w FROM ob WHERE bid = aid) FROM oa ORDER BY v"
        ),
        vec!["x|B1", "y|B2"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT v FROM oa WHERE EXISTS (SELECT 1 FROM ob WHERE bid = aid) ORDER BY v"
        ),
        vec!["x", "y"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT v FROM oa WHERE aid IN (SELECT bid FROM ob WHERE w > '') ORDER BY v"
        ),
        vec!["x", "y"]
    );
    // Mixed: the inner side qualified, the outer side bare.
    assert_eq!(
        rows(
            &mut e,
            "SELECT v, (SELECT w FROM ob WHERE ob.bid = aid) FROM oa ORDER BY v"
        ),
        vec!["x|B1", "y|B2"]
    );
}

/// The subquery is evaluated PER OUTER ROW, not once and reused — the
/// hazard a wrong "uncorrelated" verdict would create.
#[test]
fn round545_bare_correlation_is_evaluated_per_row() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE oa (aid INT, v TEXT)").unwrap();
    e.execute("CREATE TABLE ob (bid INT, w TEXT)").unwrap();
    e.execute("INSERT INTO oa VALUES (1, 'x'), (2, 'y')")
        .unwrap();
    // Only the first outer row has a match.
    e.execute("INSERT INTO ob VALUES (1, 'B1')").unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT v FROM oa WHERE EXISTS (SELECT 1 FROM ob WHERE bid = aid) ORDER BY v"
        ),
        vec!["x"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT v, (SELECT w FROM ob WHERE bid = aid) FROM oa ORDER BY v"
        ),
        vec!["x|B1", "y|NULL"]
    );
}

/// A name BOTH scopes supply belongs to the inner one, as in PG.
#[test]
fn round545_a_shared_name_resolves_inward() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE sa (k INT, v TEXT)").unwrap();
    e.execute("CREATE TABLE sb (k INT, w TEXT)").unwrap();
    e.execute("INSERT INTO sa VALUES (1, 'x'), (2, 'y')")
        .unwrap();
    e.execute("INSERT INTO sb VALUES (1, 'B1')").unwrap();
    // `k = 1` is sb's k, so the count is the same for every outer row.
    assert_eq!(
        rows(
            &mut e,
            "SELECT v, (SELECT count(*) FROM sb WHERE k = 1) FROM sa ORDER BY v"
        ),
        vec!["x|1", "y|1"]
    );
}

/// A name neither scope supplies is still an error, not a NULL.
#[test]
fn round545_an_unknown_name_is_still_an_error() {
    let mut e = engine();
    let err = format!(
        "{}",
        e.execute("SELECT v, (SELECT w FROM ob WHERE bid = nosuchcol) FROM oa")
            .expect_err("no such column anywhere")
    );
    assert!(err.contains("nosuchcol"), "message was {err}");
}

/// An uncorrelated subquery stays uncorrelated — the new rule must not
/// drag every subquery onto the per-row path.
#[test]
fn round545_uncorrelated_stays_uncorrelated() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT v, (SELECT count(*) FROM ob) FROM oa ORDER BY v"
        ),
        vec!["x|2", "y|2"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT v FROM oa WHERE aid > (SELECT min(bid) FROM ob) ORDER BY v"
        ),
        vec!["y"]
    );
}

/// The write path correlates on a bare name too.
#[test]
fn round545_update_correlates_on_a_bare_name() {
    let mut e = engine();
    e.execute("UPDATE oa SET v = (SELECT w FROM ob WHERE bid = aid)")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT aid, v FROM oa ORDER BY aid"),
        vec!["1|B1", "2|B2"]
    );
}

/// A catalog correlates on a bare name — the shape pg_dump writes.
#[test]
fn round545_catalog_correlates_on_a_bare_name() {
    let mut e = engine();
    // typrelid is 0 for a base type, so the subquery finds no pg_class
    // row and the answer is NULL — which is why pg_dump guards it with
    // a CASE. What matters is that the reference resolves at all.
    assert_eq!(
        rows(
            &mut e,
            "SELECT typname, (SELECT relkind FROM pg_class WHERE oid = typrelid) \
             FROM pg_type WHERE typname = 'int4'"
        ),
        vec!["int4|NULL"]
    );
}

/// And on its own name as a qualifier, which the rewrite had hidden.
#[test]
fn round545_catalog_name_as_a_qualifier() {
    let mut e = engine();
    // pg_dump's is-this-an-array-type test: _int4's typelem is int4,
    // and int4's typarray is 1007 — _int4 itself.
    assert_eq!(
        rows(
            &mut e,
            "SELECT typname, (SELECT typarray FROM pg_type te WHERE te.oid = pg_type.typelem) \
             FROM pg_type WHERE typname = '_int4'"
        ),
        vec!["_int4|1007"]
    );
}
