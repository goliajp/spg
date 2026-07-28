//! v7.39 (round 591) — a set operation scanned its whole right side once
//! per left row.
//!
//! Round 589's sweep left INTERSECT at 85x and EXCEPT at 26x, both about
//! 1.6 s. INTERSECT, EXCEPT and their ALL forms all ask the same question —
//! "is this left row over there?" — and all four answered it with
//! `peer_rows.iter().any(row_eq_norm)`.
//!
//! The measurement that named it is the asymmetry: `500k INTERSECT 1000`
//! took 1665 ms while the same two inputs the other way round took 20 ms. A
//! left row that MATCHES stops the scan early; a left row that does not pays
//! for the whole right side, and in the slow direction 499,000 of them do
//! not match. Holding the left at 100k and raising the right from 100 to
//! 10,000 walks the cost from 35 ms to 2848 — (left x right), at about
//! 3.2 ns a comparison.
//!
//! Round 485 had already solved this shape for DISTINCT, and this reuses its
//! machinery rather than inventing a second one: bucket by `norm_hash_row`,
//! whose only guarantee is the one needed here — rows `row_eq_norm` calls
//! equal hash the same — and settle every bucket with the exact comparator,
//! so a collision costs time and never an answer.
//!
//!     left 100000  right   100    35.21 ->  16.90 ms   PG  8.75
//!     left 100000  right  1000   314.97 ->  16.58      PG 10.23
//!     left 100000  right 10000  2848.31 ->  11.68      PG 14.13
//!     left  10000  right  1000    30.33 ->   0.94      PG 13.74
//!     left 200000  right  1000   787.77 ->  21.74      PG 21.87
//!     left   1000  right100000    15.85 ->   8.25      PG 31.80
//!
//! SPG wins four of those six now and ties a fifth. On the 500k shapes that
//! started it, INTERSECT 1665 -> 66.8 ms against PG's 18.4, and EXCEPT
//! 1634 -> 70.0 against 61.8 — near parity. UNION was already bucketed and
//! did not move (62.5 against PG's 85.6, a win before and after).
//!
//! What the pins are for. The hash decides which rows are COMPARED; the
//! comparator still decides which are equal, so the risk is a value pair the
//! comparator calls equal while the hash separates them — NULLs, a NUMERIC
//! against an INT of the same value, a double against either, text differing
//! only in trailing blanks. The multiset forms add a second risk: they must
//! cancel one occurrence per occurrence, not all of them. All 20 shapes here
//! were checked against live PG18 and matched.

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

fn one(e: &mut Engine, sql: &str) -> String {
    vals(e, sql).first().cloned().unwrap_or_default()
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE sa (a INT, b TEXT, c NUMERIC(10,2))").unwrap();
    e.execute("CREATE TABLE sb (a INT, b TEXT, c NUMERIC(10,2))").unwrap();
    e.execute(
        "INSERT INTO sa VALUES (1,'x',1.00),(1,'x',1.00),(2,'y',2.50),(3,NULL,NULL),\
         (3,NULL,NULL),(4,'z',0.00),(NULL,NULL,NULL)",
    )
    .unwrap();
    e.execute(
        "INSERT INTO sb VALUES (1,'x',1.00),(2,'y',2.50),(2,'y',2.50),(5,'w',5.00),\
         (NULL,NULL,NULL),(3,NULL,NULL)",
    )
    .unwrap();
    e
}

/// The four operators over rows carrying duplicates and NULLs — a NULL row
/// is a value like any other to a set operation, and has to find its twin.
#[test]
fn round591_set_operators_over_duplicates_and_nulls() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT * FROM (SELECT a,b,c FROM sa INTERSECT SELECT a,b,c FROM sb) q ORDER BY 1,2,3"
        ),
        vec!["1|x|1.00", "2|y|2.50", "3|NULL|NULL", "NULL|NULL|NULL"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT * FROM (SELECT a,b,c FROM sa INTERSECT ALL SELECT a,b,c FROM sb) q \
             ORDER BY 1,2,3"
        ),
        vec!["1|x|1.00", "2|y|2.50", "3|NULL|NULL", "NULL|NULL|NULL"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT * FROM (SELECT a,b,c FROM sa EXCEPT SELECT a,b,c FROM sb) q ORDER BY 1,2,3"
        ),
        vec!["4|z|0.00"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT * FROM (SELECT a,b,c FROM sa EXCEPT ALL SELECT a,b,c FROM sb) q ORDER BY 1,2,3"
        ),
        vec!["1|x|1.00", "3|NULL|NULL", "4|z|0.00"],
        "one right occurrence cancels one left occurrence, not all of them"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT * FROM (SELECT a,b,c FROM sb EXCEPT ALL SELECT a,b,c FROM sa) q ORDER BY 1,2,3"
        ),
        vec!["2|y|2.50", "5|w|5.00"],
        "and the same the other way round"
    );
    assert_eq!(
        vals(&mut e, "SELECT * FROM (SELECT b FROM sa INTERSECT SELECT b FROM sb) q ORDER BY 1"),
        vec!["x", "y", "NULL"],
        "and a NULL on both sides is a value that meets its twin"
    );
}

/// The hash chooses which rows get compared, so any pair the comparator
/// calls equal must hash the same: a NUMERIC against an INT, a double
/// against either, and text that differs only in trailing blanks.
#[test]
fn round591_equal_values_share_a_bucket() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT * FROM (SELECT c FROM sa INTERSECT SELECT c*1.000 FROM sb) q ORDER BY 1"
        ),
        vec!["1.00", "2.50", "NULL"],
        "NUMERIC scales differ, values do not"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT * FROM (SELECT a FROM sa INTERSECT SELECT a::BIGINT FROM sb) q ORDER BY 1"
        ),
        vec!["1", "2", "3", "NULL"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT * FROM (SELECT a FROM sa INTERSECT SELECT a::DOUBLE PRECISION FROM sb) q \
             ORDER BY 1"
        ),
        vec!["1", "2", "3", "NULL"],
        "INT against double"
    );
    // Trailing blanks: TEXT keeps them and the two values are NOT equal, so
    // a UNION keeps both — the hash may collide them, the comparator splits
    // them.
    assert_eq!(
        vals(&mut e, "SELECT * FROM (SELECT 'a ' UNION SELECT 'a') q ORDER BY 1"),
        vec!["a", "a "]
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM (SELECT 'a '::TEXT INTERSECT SELECT 'a'::TEXT) q"),
        "0"
    );
}

/// The multiset forms cancel occurrence for occurrence, and the counts have
/// to come out right in both directions.
#[test]
fn round591_multiset_counts() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM (SELECT 1 FROM generate_series(1,5) \
             INTERSECT ALL SELECT 1 FROM generate_series(1,3)) q"
        ),
        "3",
        "min of the two counts"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM (SELECT 1 FROM generate_series(1,3) \
             EXCEPT ALL SELECT 1 FROM generate_series(1,5)) q"
        ),
        "0",
        "more on the right cancels everything"
    );
    // Overlapping residue classes: 200 rows over 7 values against 150 over 5.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM (SELECT gg % 7 FROM generate_series(1,200) gg \
             INTERSECT ALL SELECT gg % 5 FROM generate_series(1,150) gg) q"
        ),
        "144"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM (SELECT gg % 7 FROM generate_series(1,200) gg \
             EXCEPT ALL SELECT gg % 5 FROM generate_series(1,150) gg) q"
        ),
        "56"
    );
}

/// Empty sides, chains, and a set operation against itself.
#[test]
fn round591_empty_sides_and_chains() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT * FROM (SELECT a FROM sa EXCEPT SELECT a FROM sb WHERE false) q ORDER BY 1"
        ),
        vec!["1", "2", "3", "4", "NULL"],
        "an empty right subtracts nothing, and the left is still deduplicated"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM (SELECT a FROM sa WHERE false INTERSECT SELECT a FROM sb) q"
        ),
        "0"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT * FROM (SELECT a FROM sa EXCEPT SELECT a FROM sb INTERSECT SELECT a FROM sa) q \
             ORDER BY 1"
        ),
        vec!["4"],
        "chained left to right"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT * FROM (SELECT a FROM sa UNION SELECT a FROM sb EXCEPT SELECT 1) q ORDER BY 1"
        ),
        vec!["2", "3", "4", "5", "NULL"]
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM (SELECT a,b,c FROM sa INTERSECT SELECT a,b,c FROM sa) q"
        ),
        "5",
        "a table intersected with itself is its own distinct row count"
    );
}

/// At a size where the old scan decided the cost, the answer has to match
/// what the row counts say it should be.
#[test]
fn round591_scale_matches_the_counts() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT)").unwrap();
    e.execute("CREATE TABLE small (id INT)").unwrap();
    e.execute("INSERT INTO big SELECT gg FROM generate_series(1, 20000) gg")
        .unwrap();
    e.execute("INSERT INTO small SELECT gg FROM generate_series(1, 500) gg")
        .unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM (SELECT id FROM big INTERSECT SELECT id FROM small) q"
        ),
        "500"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM (SELECT id FROM big EXCEPT SELECT id FROM small) q"
        ),
        "19500"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM (SELECT id FROM small EXCEPT SELECT id FROM big) q"
        ),
        "0",
        "the small side is wholly contained"
    );
    // Disjoint inputs are the case that used to pay the full scan every row.
    e.execute("INSERT INTO small SELECT gg FROM generate_series(100001, 100200) gg")
        .unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM (SELECT id FROM small EXCEPT SELECT id FROM big) q"
        ),
        "200"
    );
}
