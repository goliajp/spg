//! v7.39 (round 242) — PG's general grouping-element grammar, swept 18
//! cases against live PG18.4 (2026-07-19). The lone-ROLLUP/CUBE/GROUPING
//! SETS expansions, the grouping() bitmask and HAVING over it already
//! matched; what did not:
//!
//!   * mixing a plain key with an element (`GROUP BY a, ROLLUP (b)`) —
//!     the sets are the CARTESIAN PRODUCT of the elements' lists;
//!   * composite ROLLUP units (`ROLLUP ((a, b))` moves both as one);
//!   * a GROUPING SETS item that is itself a ROLLUP;
//!   * `GROUP BY DISTINCT`, which drops duplicate sets by content;
//!   * grouping() over a plain GROUP BY (constant 0) and its refusal —
//!     PG's 42803 "arguments to GROUPING must be grouping expressions of
//!     the associated query level" — for a non-key argument;
//!   * the folded mask is cast-wrapped: a bare integer select item is
//!     indistinguishable from a positional reference once `ORDER BY 1`
//!     substitutes it back, and the round-232 position check read the
//!     mask as an out-of-range position.

use spg_engine::{Engine, QueryResult};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE s (a text, b text, c int)").unwrap();
    e.execute("INSERT INTO s VALUES ('x','p',1),('x','q',2),('y','p',4),('y','q',8)")
        .unwrap();
    e
}

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Null => String::new(),
                        other => spg_engine::eval::value_to_text(other),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn mixed_and_composite_elements_expand_like_pg() {
    let mut e = seeded();
    // Plain key × ROLLUP — the product {(a,b),(a)}.
    assert_eq!(
        rows(&mut e, "SELECT a, b, sum(c) FROM s GROUP BY a, ROLLUP (b) ORDER BY a, b NULLS LAST"),
        ["x|p|1", "x|q|2", "x||3", "y|p|4", "y|q|8", "y||12"]
    );
    // Composite unit: (a, b) rolls up together — no (a)-only set.
    assert_eq!(
        rows(
            &mut e,
            "SELECT a, b, sum(c) FROM s GROUP BY ROLLUP ((a, b)) ORDER BY a NULLS LAST, b NULLS LAST"
        ),
        ["x|p|1", "x|q|2", "y|p|4", "y|q|8", "||15"]
    );
    // A GROUPING SETS item that is itself a ROLLUP.
    assert_eq!(
        rows(
            &mut e,
            "SELECT a, b, sum(c) FROM s GROUP BY GROUPING SETS (ROLLUP (a), (b)) \
             ORDER BY a NULLS LAST, b NULLS LAST"
        ),
        ["x||3", "y||12", "|p|5", "|q|10", "||15"]
    );
}

#[test]
fn group_by_distinct_drops_duplicate_sets() {
    let mut e = seeded();
    // ROLLUP (a), ROLLUP (a) expands to a duplicated {(a),()} pair;
    // DISTINCT collapses them (PG 15+).
    assert_eq!(
        rows(
            &mut e,
            "SELECT a, count(*) FROM s GROUP BY DISTINCT ROLLUP (a), ROLLUP (a) \
             ORDER BY a NULLS LAST"
        ),
        ["x|2", "y|2", "|4"]
    );
    // Without DISTINCT the duplicate sets stay (probed against PG).
    assert_eq!(
        rows(&mut e, "SELECT a, sum(c) FROM s GROUP BY GROUPING SETS ((a), (a)) ORDER BY a"),
        ["x|3", "x|3", "y|12", "y|12"]
    );
}

#[test]
fn grouping_over_a_plain_group_by_is_zero() {
    let mut e = seeded();
    // Used to die at eval with "unknown function `grouping`".
    assert_eq!(
        rows(&mut e, "SELECT grouping(a) FROM s GROUP BY a ORDER BY 1 LIMIT 1"),
        ["0"]
    );
    // A non-key argument (or no GROUP BY at all) is PG's 42803.
    for sql in [
        "SELECT grouping(c) FROM s GROUP BY a",
        "SELECT grouping(a) FROM s",
    ] {
        let got = format!("{}", e.execute(sql).unwrap_err());
        assert!(
            got.contains(
                "arguments to GROUPING must be grouping expressions of the associated query level"
            ),
            "{sql}: {got}"
        );
    }
}

#[test]
fn the_lone_element_expansions_are_unchanged() {
    let mut e = seeded();
    // Regression guard over the sweep's clean cases.
    assert_eq!(
        rows(
            &mut e,
            "SELECT a, b, sum(c) FROM s GROUP BY ROLLUP (a, b) ORDER BY a NULLS LAST, b NULLS LAST"
        ),
        ["x|p|1", "x|q|2", "x||3", "y|p|4", "y|q|8", "y||12", "||15"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT a, b, grouping(a, b) FROM s GROUP BY CUBE (a, b) \
             ORDER BY a NULLS LAST, b NULLS LAST"
        ),
        [
            "x|p|0", "x|q|0", "x||1", "y|p|0", "y|q|0", "y||1", "|p|2", "|q|2", "||3"
        ]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT a, sum(c) FROM s GROUP BY ROLLUP (a) HAVING grouping(a) = 0 ORDER BY a"
        ),
        ["x|3", "y|12"]
    );
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM s GROUP BY GROUPING SETS ((), ())"),
        ["4", "4"]
    );
}
