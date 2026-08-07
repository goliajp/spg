//! Round 665 — the four sum/avg accumulators are one.
//!
//! `sum` and `avg` kept their running state in four independently written
//! places: `FusedAcc`'s fields, `AggState`'s fields, and twice more as
//! loose locals in `accumulate_groups`. That was deliberate hand-inlining,
//! not drift — `FusedAcc`'s own doc comment said "field-for-field the same
//! running state the single-spec sum/avg fast path keeps in locals".
//!
//! What it cost is on the record. Round 626 had to add a SMALLINT arm to
//! one copy that the other three already carried, so `SELECT sum(x)` over
//! an ordinary smallint column answered "sum/avg need numeric, got
//! smallint". Round 664 needed FOUR edits to reach the whole family, and
//! found three of them only by running a different SQL shape and watching
//! the wrong answer come back — reading did not reveal them, because the
//! three parallel loops in the fused block are not symmetric.
//!
//! The shapes below exist because each one reached a DIFFERENT copy before
//! the collapse. They are the regression net for the collapse itself: if a
//! second accumulator ever reappears, one of these stops matching.

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

fn seed(e: &mut Engine) {
    e.execute("CREATE TABLE acc(s SMALLINT, i INT, b BIGINT, r REAL, d DOUBLE PRECISION, n NUMERIC(12,3), v INTERVAL, m MONEY, t TEXT, g INT)")
        .unwrap();
    e.execute(
        "INSERT INTO acc VALUES \
         (10,100,1000,1.5,2.5,3.125,'1 day','4.00','abcd',1), \
         (20,200,2000,2.5,3.5,4.250,'2 days','7.00','ef',1), \
         (30,300,3000,3.0,4.0,5.625,'3 days','9.00','ghijkl',2)",
    )
    .unwrap();
}

/// Every numeric type the accumulator accepts, through the no-GROUP-BY
/// fused path. A copy that missed one variant is how round 626's smallint
/// bug looked.
#[test]
fn round665_every_accepted_type_sums() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(one(&mut e, "SELECT sum(s) FROM acc"), "60");
    assert_eq!(one(&mut e, "SELECT sum(i) FROM acc"), "600");
    assert_eq!(one(&mut e, "SELECT sum(b) FROM acc"), "6000");
    assert_eq!(one(&mut e, "SELECT sum(r) FROM acc"), "7");
    assert_eq!(one(&mut e, "SELECT sum(d) FROM acc"), "10");
    assert_eq!(one(&mut e, "SELECT sum(n) FROM acc"), "13.000");
    assert_eq!(one(&mut e, "SELECT sum(v) FROM acc"), "6 days");
    assert_eq!(one(&mut e, "SELECT sum(m) FROM acc"), "$20.00");
    // And the rejection, which every copy also had to word identically.
    let err = e.execute("SELECT sum(t) FROM acc").expect_err("refused");
    assert!(format!("{err}").contains("sum/avg need numeric"), "{err}");
}

/// The same types again, grouped — a different copy before the collapse.
#[test]
fn round665_every_accepted_type_sums_grouped() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        one(&mut e, "SELECT g, sum(s) FROM acc GROUP BY g ORDER BY g"),
        "1|30,2|30"
    );
    assert_eq!(
        one(&mut e, "SELECT g, sum(n) FROM acc GROUP BY g ORDER BY g"),
        "1|7.375,2|5.625"
    );
    assert_eq!(
        one(&mut e, "SELECT g, sum(m) FROM acc GROUP BY g ORDER BY g"),
        "1|$11.00,2|$9.00"
    );
    assert_eq!(
        one(&mut e, "SELECT g, sum(v) FROM acc GROUP BY g ORDER BY g"),
        "1|3 days,2|3 days"
    );
}

/// `count` lives inside `NumAcc` so the accumulator takes ONE base
/// pointer. That is a measured requirement, not a style choice: with it
/// beside the struct, `sum(int)` over 500k rows lost ~8% (paired, n=12,
/// p=0.04). The shapes here read count through every route that survives.
#[test]
fn round665_count_still_answers_from_inside_the_struct() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(one(&mut e, "SELECT count(*) FROM acc"), "3");
    assert_eq!(one(&mut e, "SELECT count(i) FROM acc"), "3");
    assert_eq!(one(&mut e, "SELECT count(DISTINCT g) FROM acc"), "2");
    assert_eq!(
        one(&mut e, "SELECT avg(i) FROM acc"),
        "200.0000000000000000"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT g, count(*), avg(i) FROM acc GROUP BY g ORDER BY g"
        ),
        "1|2|150.0000000000000000,2|1|300.0000000000000000"
    );
    // NULLs are skipped by count(col) but not count(*) — the two read the
    // same field now, so this is worth holding.
    e.execute("INSERT INTO acc(g) VALUES (3)").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*), count(i) FROM acc"), "4|3");
}

/// The fused block runs three parallel loops and the middle one is a
/// `length()` shortcut that accumulates nothing numeric. That asymmetry is
/// what hid a site from reading in round 664, so it gets its own shape.
#[test]
fn round665_the_length_shortcut_still_takes_its_own_path() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(one(&mut e, "SELECT sum(length(t)) FROM acc"), "12");
    assert_eq!(
        one(
            &mut e,
            "SELECT g, sum(length(t)) FROM acc GROUP BY g ORDER BY g"
        ),
        "1|6,2|6"
    );
    // Fused beside a numeric sum, so both loops run over one scan.
    assert_eq!(
        one(&mut e, "SELECT sum(length(t)), sum(i) FROM acc"),
        "12|600"
    );
}

/// sum and avg share one accumulator slot when they read the same column;
/// that sharing is the reason the fused path exists, so it gets a pin.
#[test]
fn round665_sum_and_avg_share_one_slot() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        one(&mut e, "SELECT sum(i), avg(i), count(*) FROM acc"),
        "600|200.0000000000000000|3"
    );
    assert_eq!(
        one(&mut e, "SELECT sum(n), avg(n) FROM acc"),
        "13.000|4.3333333333333333"
    );
}
