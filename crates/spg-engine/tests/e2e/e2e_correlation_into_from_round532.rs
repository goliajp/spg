//! v7.39 (round 532) — an outer reference inside a subquery's FROM clause.
//!
//! Round 530 recorded that a LATERAL body two scopes out answered
//! "missing FROM-clause entry". Measuring it found the cause is not
//! about LATERAL: a correlated subquery is run by splicing the outer
//! row's values into it, and that walk covered every clause EXCEPT the
//! FROM one.
//!
//!     SELECT (SELECT l.k FROM b, LATERAL (SELECT b.d + a.id AS k) l
//!             WHERE b.id = a.id) FROM a
//!     PG18  101, NULL      SPG  missing FROM-clause entry for "a"
//!
//! A JOIN's ON has the same shape and failed the same way:
//!
//!     SELECT (SELECT count(*) FROM b JOIN a2 ON a2.id = a.id …) FROM a
//!
//! The identical reference one clause over — in the subquery's own
//! WHERE — always worked, which is what made this look like a LATERAL
//! problem rather than a missing branch of the walk.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a (id INT, v INT)").unwrap();
    e.execute("CREATE TABLE b (id INT, d INT)").unwrap();
    e.execute("INSERT INTO a VALUES (1,10),(2,20)").unwrap();
    e.execute("INSERT INTO b VALUES (1,100)").unwrap();
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

/// The shape round 530 recorded.
#[test]
fn round532_lateral_body_sees_the_enclosing_correlation() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT (SELECT l.k FROM b, LATERAL (SELECT b.d + a.id AS k) l \
             WHERE b.id = a.id) FROM a ORDER BY id"
        ),
        vec!["101", "NULL"]
    );
    // The same correlation WITHOUT the LATERAL always worked, and is
    // the reading the one above has to match.
    assert_eq!(
        rows(
            &mut e,
            "SELECT (SELECT b.d + a.id FROM b WHERE b.id = a.id) FROM a ORDER BY id"
        ),
        vec!["101", "NULL"]
    );
}

/// A JOIN's ON is the other clause the walk skipped.
#[test]
fn round532_join_on_sees_the_enclosing_correlation() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT (SELECT count(*) FROM b JOIN a a2 ON a2.id = a.id \
             WHERE b.id = 1) FROM a ORDER BY id"
        ),
        vec!["1", "1"]
    );
}

/// A LATERAL at the top level — one scope, not two — kept working.
#[test]
fn round532_one_level_lateral_unchanged() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT a.id, l.k FROM a, LATERAL (SELECT a.v + 1 AS k) l ORDER BY a.id"
        ),
        vec!["1|11", "2|21"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT a.id, l.k FROM a JOIN b ON b.id = a.id, \
             LATERAL (SELECT b.d + a.v AS k) l"
        ),
        vec!["1|110"]
    );
}

/// A name that belongs to a SIBLING of the FROM list is not an outer
/// reference and must not be spliced — `b.d` above resolves inside the
/// subquery, not against the row being correlated on.
#[test]
fn round532_sibling_names_are_left_alone() {
    let mut e = engine();
    e.execute("INSERT INTO b VALUES (2, 200)").unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT (SELECT sum(l.k) FROM b, LATERAL (SELECT b.d * 2 AS k) l) FROM a ORDER BY id"
        ),
        vec!["600", "600"]
    );
}
