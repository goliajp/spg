//! v7.39.13 — `timetz` can be compared, and it orders the way
//! PostgreSQL orders it.
//!
//! Found by this version's own composite-component sweep, not reported.
//! Two defects in a type SPG stores, renders and accepts in DDL:
//!
//! ```text
//!                    PG 18.6   SPG 7.39.12
//!   k = k            true      ERROR: operator does not exist:
//!                                     time with time zone =
//!                                     time with time zone
//!   k > '…'          rows      the same error
//!   ORDER BY k       see below wrong among values sharing an instant
//! ```
//!
//! The order is a PAIR, and only the first half was implemented: the
//! UTC-equivalent instant, then the OFFSET DESCENDING. Values that name
//! one instant in different zones are DISTINCT — `'07:00:00+00' =
//! '02:00:00-05'` is FALSE on PostgreSQL 18.6 — so ordering by the
//! instant alone called four of these equal and a stable sort returned
//! them in insertion order:
//!
//! ```text
//!   PG 18.6        SPG 7.39.12
//!   07:00:00+01    07:00:00+01
//!   06:59:59+00    06:59:59+00
//!   09:00:00+02    07:00:00+00
//!   07:00:00+00    02:00:00-05
//!   02:00:00-05    09:00:00+02
//!   01:00:00-06    01:00:00-06
//! ```
//!
//! Every row below was measured on PostgreSQL 18.6 before anything was
//! changed.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    rows.iter()
        .map(|r| {
            r.values
                .iter()
                .map(spg_engine::eval::value_to_text)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// Six values: two at distinct instants, four sharing 07:00 UTC in four
/// different zones. Inserted in an order that is neither the answer nor
/// its reverse, so insertion order cannot pass for a sort.
fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int NOT NULL, k timetz NOT NULL)")
        .unwrap();
    e.execute(
        "INSERT INTO t VALUES (1,'07:00:00+00'),(2,'02:00:00-05'),(3,'09:00:00+02'),\
         (4,'06:59:59+00'),(5,'07:00:00+01'),(6,'01:00:00-06')",
    )
    .unwrap();
    e
}

#[test]
fn order_by_timetz_matches_postgres() {
    let mut e = seeded();
    assert_eq!(
        rows(&mut e, "SELECT id FROM t ORDER BY k"),
        ["5", "4", "3", "1", "2", "6"]
    );
    assert_eq!(
        rows(&mut e, "SELECT id FROM t ORDER BY k DESC"),
        ["6", "2", "1", "3", "4", "5"]
    );
}

/// The instant is the FIRST half of the key: a later instant sorts
/// after an earlier one whatever the zones say.
#[test]
fn the_instant_decides_before_the_zone_does() {
    let mut e = seeded();
    assert_eq!(
        rows(
            &mut e,
            "SELECT '07:00:00+01'::timetz < '06:59:59+00'::timetz"
        ),
        ["true"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT '12:00:00-05'::timetz > '02:00:00-05'::timetz"
        ),
        ["true"]
    );
}

/// And the zone is the SECOND half, descending — the part that was
/// missing. All four of these name 07:00 UTC.
#[test]
fn the_zone_breaks_the_tie_descending() {
    let mut e = seeded();
    assert_eq!(
        rows(
            &mut e,
            "SELECT '09:00:00+02'::timetz < '07:00:00+00'::timetz"
        ),
        ["true"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT '07:00:00+00'::timetz < '02:00:00-05'::timetz"
        ),
        ["true"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT '02:00:00-05'::timetz < '01:00:00-06'::timetz"
        ),
        ["true"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT '07:00:00+00'::timetz = '02:00:00-05'::timetz"
        ),
        ["false"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT '07:00:00+00'::timetz = '07:00:00+00'::timetz"
        ),
        ["true"]
    );
}

/// The operators the missing arm took with it: every comparison, not
/// only `=`.
#[test]
fn every_comparison_answers() {
    let mut e = seeded();
    assert_eq!(
        rows(
            &mut e,
            "SELECT id FROM t WHERE k > '06:59:59+00' ORDER BY id"
        ),
        ["1", "2", "3", "6"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT id FROM t WHERE k BETWEEN '09:00:00+02' AND '02:00:00-05' ORDER BY id"
        ),
        ["1", "2", "3"]
    );
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM t WHERE k <> '07:00:00+00'"),
        ["5"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT id FROM t WHERE k IN ('07:00:00+00','01:00:00-06') ORDER BY id"
        ),
        ["1", "6"]
    );
    assert_eq!(rows(&mut e, "SELECT max(k) FROM t"), ["01:00:00-06"]);
    assert_eq!(rows(&mut e, "SELECT min(k) FROM t"), ["07:00:00+01"]);
}

/// The rule every version of this repository is held to: creating an
/// index may not change an answer.
///
/// This is the row that caught the third defect. The B-tree key was the
/// INSTANT alone, like the sort key, so the four values sharing 07:00
/// UTC landed on one key — and `WHERE k > '07:00:00+00'` answered
/// NOTHING with the index against `2, 6` without it, because the two
/// rows above that bound share its instant and sort below it once the
/// zone is dropped. Too FEW, which no re-check can repair.
#[test]
fn an_index_over_timetz_does_not_change_the_answer() {
    let mut e = seeded();
    let before: Vec<Vec<String>> = QUESTIONS.iter().map(|q| rows(&mut e, q)).collect();
    // Before/after alone is self-consistent even when both are wrong,
    // so the scan's answers are pinned to PostgreSQL 18.6's too.
    assert_eq!(
        before,
        [
            vec!["1".to_string()],
            vec!["2".to_string()],
            vec!["1".to_string(), "2".into(), "3".into(), "6".into()],
            vec!["1".to_string(), "3".into(), "4".into(), "5".into()],
            vec!["1".to_string(), "2".into(), "3".into()],
            vec!["2".to_string(), "6".into()],
            vec!["3".to_string(), "4".into(), "5".into()],
            vec!["2".to_string(), "6".into()],
        ]
    );
    e.execute("CREATE INDEX t_k ON t (k)").unwrap();
    for (q, was) in QUESTIONS.iter().zip(&before) {
        assert_eq!(&rows(&mut e, q), was, "the index changed the answer to {q}");
    }
}

/// The last three are the DISCRIMINATING ones: their boundary shares
/// its instant with other rows, so a range read in a key space that
/// holds only the instant returns too FEW — the failure mode a superset
/// and a re-check cannot save.
const QUESTIONS: [&str; 8] = [
    "SELECT id FROM t WHERE k = '07:00:00+00' ORDER BY id",
    "SELECT id FROM t WHERE k = '02:00:00-05' ORDER BY id",
    "SELECT id FROM t WHERE k > '06:59:59+00' ORDER BY id",
    "SELECT id FROM t WHERE k <= '07:00:00+00' ORDER BY id",
    "SELECT id FROM t WHERE k BETWEEN '09:00:00+02' AND '02:00:00-05' ORDER BY id",
    "SELECT id FROM t WHERE k > '07:00:00+00' ORDER BY id",
    "SELECT id FROM t WHERE k < '07:00:00+00' ORDER BY id",
    "SELECT id FROM t WHERE k >= '02:00:00-05' ORDER BY id",
];

/// DISTINCT and GROUP BY ride the same order, so four values that share
/// an instant stay four.
#[test]
fn values_sharing_an_instant_stay_distinct() {
    let mut e = seeded();
    assert_eq!(rows(&mut e, "SELECT count(DISTINCT k) FROM t"), ["6"]);
    assert_eq!(
        rows(
            &mut e,
            "SELECT count(*) FROM (SELECT k FROM t GROUP BY k) g"
        ),
        ["6"]
    );
}
