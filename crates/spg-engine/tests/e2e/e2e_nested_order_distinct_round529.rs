//! v7.39 (round 529) — sweeping everyday query shapes, not a gap list.
//!
//! Round 528's best find came from outside the audit's inventory, so
//! this round ran fifteen shapes an application actually emits — window
//! frames, FILTER, LATERAL, recursive CTEs, ordered-set aggregates,
//! DISTINCT ON — against PG18 and compared. Three failed, and they are
//! two bugs:
//!
//!     SELECT * FROM (SELECT v AS w FROM t ORDER BY w) z
//!     PG18  rows          SPG  ERROR: column "w" does not exist
//!
//! The alias pass runs once per STATEMENT, so a SELECT nested in a FROM
//! clause, a CTE or a scalar subquery never got it — the same query
//! worked on its own and failed the moment anything wrapped it, which is
//! what generated SQL does constantly. Ordinals and real columns were
//! fine either way.
//!
//!     SELECT DISTINCT ON (g) v FROM t ORDER BY g, v DESC
//!     PG18  one row per g   SPG  ERROR: column "g" does not exist
//!
//! DISTINCT ON evaluated its keys against the PROJECTED row, so a key
//! that is not in the select list — the canonical "latest row per group"
//! — could not be read at all.
//!
//! Measuring that turned up a third thing nothing had reported: the
//! dedup ran AFTER the inner LIMIT.
//!
//!     … DISTINCT ON (g) … LIMIT 2   PG18  2 rows   SPG  1 row
//!
//! The limit had taken two rows of the same group before anything
//! deduplicated them, so a paginated DISTINCT ON returned short pages
//! and said nothing.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE s (id INT, g TEXT, v INT)").unwrap();
    e.execute("INSERT INTO s VALUES (1,'a',10),(2,'a',20),(3,'b',5),(4,'b',NULL)")
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

/// The wrappers generated SQL puts around a SELECT.
#[test]
fn round529_order_by_alias_survives_nesting() {
    let mut e = engine();
    let expect = vec!["5", "10", "20"];
    // On its own — this always worked.
    assert_eq!(
        rows(&mut e, "SELECT v AS w FROM s WHERE v IS NOT NULL ORDER BY w"),
        expect
    );
    // In a derived table.
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM (SELECT v AS w FROM s WHERE v IS NOT NULL ORDER BY w) z"
        ),
        expect
    );
    // In a CTE.
    assert_eq!(
        rows(
            &mut e,
            "WITH c AS (SELECT v AS w FROM s WHERE v IS NOT NULL ORDER BY w) SELECT * FROM c"
        ),
        expect
    );
    // And in a scalar subquery, which used to surface an internal
    // "engine resolver bug" message.
    assert_eq!(
        rows(
            &mut e,
            "SELECT (SELECT max(w) FROM (SELECT v AS w FROM s ORDER BY w) q)"
        ),
        vec!["20"]
    );
}

/// An aliased expression, with a LIMIT inside the derived table.
#[test]
fn round529_nested_alias_with_limit() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM (SELECT v * 2 AS w FROM s WHERE v IS NOT NULL \
             ORDER BY w DESC LIMIT 2) z"
        ),
        vec!["40", "20"]
    );
}

/// Ordinals and real columns kept working through the same wrappers.
#[test]
fn round529_nested_ordinal_and_column_unchanged() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM (SELECT v AS w FROM s WHERE v IS NOT NULL ORDER BY 1) z"
        ),
        vec!["5", "10", "20"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM (SELECT v FROM s WHERE v IS NOT NULL ORDER BY v) z"
        ),
        vec!["5", "10", "20"]
    );
}

/// "Latest row per group" — the DISTINCT ON key is not projected.
#[test]
fn round529_distinct_on_key_need_not_be_projected() {
    let mut e = engine();
    assert_eq!(
        rows(&mut e, "SELECT DISTINCT ON (g) v FROM s ORDER BY g, v DESC"),
        vec!["20", "NULL"]
    );
    // Projecting it still works, and gives the same grouping.
    assert_eq!(
        rows(&mut e, "SELECT DISTINCT ON (g) g, v FROM s ORDER BY g, v DESC"),
        vec!["a|20", "b|NULL"]
    );
    // Several keys: four distinct (g, v) pairs, so four rows. The
    // trailing NULL renders as an empty line in psql, which is how the
    // first reading of this measurement lost it — the same trap rounds
    // 508, 518 and 520 hit.
    assert_eq!(
        rows(&mut e, "SELECT DISTINCT ON (g, v) v FROM s ORDER BY g, v"),
        vec!["10", "20", "5", "NULL"]
    );
}

/// The limit applies to what the dedup left.
#[test]
fn round529_distinct_on_dedups_before_limit() {
    let mut e = engine();
    // Four rows, two groups: a LIMIT 2 is two GROUPS, not two rows of
    // the first one.
    assert_eq!(
        rows(
            &mut e,
            "SELECT DISTINCT ON (g) g, v FROM s ORDER BY g, v DESC LIMIT 2"
        ),
        vec!["a|20", "b|NULL"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT DISTINCT ON (g) v FROM s ORDER BY g, v DESC LIMIT 1"
        ),
        vec!["20"]
    );
    // OFFSET counts groups too.
    assert_eq!(
        rows(
            &mut e,
            "SELECT DISTINCT ON (g) g FROM s ORDER BY g OFFSET 1"
        ),
        vec!["b"]
    );
}
