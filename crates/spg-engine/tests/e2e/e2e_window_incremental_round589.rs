//! v7.39 (round 589) — every row recomputed its whole window frame.
//!
//! A warm sweep of 30 ordinary SQL shapes against PG18 put this at the top
//! by a wide margin. `sum(x) OVER (PARTITION BY g)` over 500k rows in 50
//! partitions took **36.6 seconds** where PG takes 7.65 ms, and the classic
//! running total `sum(x) OVER (ORDER BY t)` took 1.46 s over 20k rows
//! against PG's 1.10 ms.
//!
//! The aggregate arm of `compute_window_partition` ran
//!
//!     for i in 0..slice.len() {
//!         let (lo, hi) = frame_bounds_for_row(&eff, i, slice)?;
//!         for j in lo..=hi { … }
//!     }
//!
//! and the two commonest frames — an unordered window's whole partition,
//! and an ordered window's start-to-current-row — give every row a frame
//! that starts at `lo == 0`. So the inner loop re-added the same rows once
//! per row: O(partition²). Measured, the cost tracked rows x partition size
//! exactly — 20k rows in 20 partitions 187 ms, in 1 partition 2934 ms, and
//! 80k in 20 partitions 2418 ms.
//!
//! Both frames only ever GROW as `i` advances, so the accumulators now live
//! outside the row loop and each row extends them over just the rows that
//! entered. Any other frame shape, and any EXCLUDE (which can drop a row
//! already folded in), resets per row exactly as before.
//!
//! 20k rows, warm, against PG18 through the same client:
//!
//!     sum OVER (PARTITION BY p)              138.28 ->  4.32 ms   PG 0.72
//!     sum OVER (PARTITION BY p ORDER BY id)   73.98 ->  6.69      PG 0.70
//!     sum OVER (ORDER BY id)                1460.03 ->  3.62      PG 0.71
//!     max OVER (PARTITION BY p ORDER BY id)   73.78 ->  6.06      PG 0.73
//!     ROWS BETWEEN 2 PRECEDING AND CURRENT     9.55 ->  5.94      PG 0.72
//!
//! and the 500k shape that started the round: 36,614 -> 212.87 ms, a 172x
//! cut that takes it from 4800x PG to 27.8x. Still a loss, no longer a
//! category difference.
//!
//! What the pins are for. An accumulator carried across rows is only
//! correct while the frame grows, and three things could break it: a RANGE
//! frame whose end jumps over a peer group (all tied rows must still read
//! the same value), min/max — which are monotone under insertion but not
//! under removal — and EXCLUDE, which removes. All 20 shapes here were
//! checked against live PG18 and matched byte for byte.

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

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE wd (id INT, p INT, v INT, f NUMERIC(10,2), s TEXT)")
        .unwrap();
    e.execute(
        "INSERT INTO wd VALUES \
         (1,1,10,1.50,'a'),(2,1,NULL,2.25,'b'),(3,1,30,NULL,'c'),(4,1,30,4.75,NULL),\
         (5,2,5,0.10,'x'),(6,2,5,0.20,'y'),(7,2,5,0.30,'z'),\
         (8,3,NULL,NULL,NULL),\
         (9,4,-7,9.99,'m'),(10,4,100,-1.25,'n')",
    )
    .unwrap();
    e
}

/// The unordered frame: one value for the whole partition, computed once
/// and given to every row.
#[test]
fn round589_whole_partition_frames() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, sum(v) OVER (PARTITION BY p), count(v) OVER (PARTITION BY p), \
             count(*) OVER (PARTITION BY p) FROM wd ORDER BY id"
        ),
        vec![
            "1|70|3|4",
            "2|70|3|4",
            "3|70|3|4",
            "4|70|3|4",
            "5|15|3|3",
            "6|15|3|3",
            "7|15|3|3",
            // An all-NULL partition sums to NULL, not 0.
            "8|NULL|0|1",
            "9|93|2|2",
            "10|93|2|2",
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, min(v) OVER (PARTITION BY p), max(v) OVER (PARTITION BY p) \
             FROM wd WHERE p = 4 ORDER BY id"
        ),
        vec!["9|-7|100", "10|-7|100"]
    );
    // No PARTITION BY and no ORDER BY is one frame over everything.
    assert_eq!(
        vals(
            &mut e,
            "SELECT sum(v) OVER (), count(*) OVER () FROM wd ORDER BY id"
        )
        .first()
        .cloned()
        .unwrap_or_default(),
        "178|10"
    );
}

/// The ordered frame runs from the partition start to the current row —
/// and under RANGE, to the end of the current row's peer group, so tied
/// rows all read the same value.
#[test]
fn round589_running_frames_and_range_ties() {
    let mut e = seed();
    // p = 2 is three rows all with v = 5: one peer group, so all three see
    // the whole partition.
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, sum(v) OVER (PARTITION BY p ORDER BY v) FROM wd WHERE p = 2 ORDER BY id"
        ),
        vec!["5|15", "6|15", "7|15"],
        "a RANGE frame includes the whole peer group"
    );
    // p = 1: v = 10, NULL, 30, 30. Ascending with NULLs last, the frame
    // grows 10 → 10+30+30 (the tie) → and the NULL row last sees all.
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, sum(v) OVER (PARTITION BY p ORDER BY v) FROM wd WHERE p = 1 ORDER BY id"
        ),
        vec!["1|10", "2|70", "3|70", "4|70"]
    );
    // ROWS is positional, so the same query steps one row at a time.
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, sum(v) OVER (PARTITION BY p ORDER BY id \
             ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM wd WHERE p = 1 ORDER BY id"
        ),
        vec!["1|10", "2|10", "3|40", "4|70"]
    );
    // Running max over a growing frame, and running count.
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, max(v) OVER (ORDER BY id), count(v) OVER (ORDER BY id) FROM wd ORDER BY id"
        ),
        vec![
            "1|10|1", "2|10|1", "3|30|2", "4|30|3", "5|30|4", "6|30|5", "7|30|6", "8|30|6",
            "9|30|7", "10|100|8",
        ]
    );
    // Descending, and with NULLs moved to the front.
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, sum(v) OVER (PARTITION BY p ORDER BY v DESC NULLS FIRST) \
             FROM wd WHERE p = 1 ORDER BY id"
        ),
        vec!["1|70", "2|NULL", "3|60", "4|60"]
    );
}

/// The shapes that must NOT take the carried accumulator: a frame that
/// shrinks, a sliding frame, and every EXCLUDE — which removes rows the
/// accumulator has already absorbed.
#[test]
fn round589_non_growing_frames_keep_recomputing() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, sum(v) OVER (PARTITION BY p ORDER BY id \
             ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) FROM wd WHERE p = 1 ORDER BY id"
        ),
        vec!["1|70", "2|60", "3|60", "4|30"],
        "a frame that shrinks"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, sum(v) OVER (PARTITION BY p ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
             FROM wd WHERE p = 1 ORDER BY id"
        ),
        vec!["1|10", "2|40", "3|60", "4|60"],
        "a sliding frame"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, sum(v) OVER (PARTITION BY p ORDER BY id \
             ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW) \
             FROM wd WHERE p = 1 ORDER BY id"
        ),
        vec!["1|60", "2|70", "3|40", "4|40"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, sum(v) OVER (PARTITION BY p ORDER BY v \
             RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE GROUP) \
             FROM wd WHERE p = 1 ORDER BY id"
        ),
        vec!["1|60", "2|70", "3|10", "4|10"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, sum(v) OVER (PARTITION BY p ORDER BY v \
             RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE TIES) \
             FROM wd WHERE p = 1 ORDER BY id"
        ),
        vec!["1|70", "2|70", "3|40", "4|40"]
    );
}

/// The accumulator carries typed state — an exact NUMERIC sum, an integer
/// sum that only falls back to float when it has to, and min/max over
/// values that are not numbers at all.
#[test]
fn round589_typed_accumulators_carry_correctly() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, sum(f) OVER (PARTITION BY p), avg(f) OVER (PARTITION BY p ORDER BY id) \
             FROM wd WHERE p = 1 ORDER BY id"
        ),
        vec![
            "1|8.50|1.50000000000000000000",
            "2|8.50|1.8750000000000000",
            "3|8.50|1.8750000000000000",
            "4|8.50|2.8333333333333333",
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, min(s) OVER (PARTITION BY p), max(s) OVER (PARTITION BY p ORDER BY id) \
             FROM wd WHERE p = 1 ORDER BY id"
        ),
        vec!["1|a|a", "2|a|b", "3|a|c", "4|a|c"],
        "text min/max, and NULL text never wins either"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, avg(v) OVER (ORDER BY id) FROM wd WHERE p = 4 ORDER BY id"
        ),
        vec!["9|-7.0000000000000000", "10|46.5000000000000000"]
    );
    // FILTER drops rows from the aggregate without changing the frame.
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, sum(v) FILTER (WHERE v > 5) OVER (PARTITION BY p), \
             count(*) FILTER (WHERE s IS NOT NULL) OVER (PARTITION BY p ORDER BY id) \
             FROM wd WHERE p = 1 ORDER BY id"
        ),
        vec!["1|70|1", "2|70|2", "3|70|3", "4|70|3"]
    );
}

/// At a size where the old path was quadratic, the answer has to be the
/// one a single pass gives — checked against sums computed independently.
#[test]
fn round589_scale_matches_a_single_pass() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE wbig (id INT, p INT)").unwrap();
    e.execute("INSERT INTO wbig SELECT gg, gg % 7 FROM generate_series(1, 5000) gg")
        .unwrap();
    // Every row of a partition sees that partition's total.
    let got = vals(
        &mut e,
        "SELECT DISTINCT p, sum(id) OVER (PARTITION BY p) FROM wbig ORDER BY p",
    );
    let want = vals(&mut e, "SELECT p, sum(id) FROM wbig GROUP BY p ORDER BY p");
    assert_eq!(got, want, "the window total is the GROUP BY total");
    // The running total's last row is the grand total, and its first is
    // the first value.
    let run = vals(
        &mut e,
        "SELECT sum(id) OVER (ORDER BY id) FROM wbig ORDER BY id",
    );
    assert_eq!(run.len(), 5000);
    assert_eq!(run[0], "1");
    assert_eq!(run[4999], (1..=5000i64).sum::<i64>().to_string());
    assert_eq!(run[9], (1..=10i64).sum::<i64>().to_string());
    // A single partition holding everything is the worst case for the old
    // code and must still agree.
    assert_eq!(
        vals(&mut e, "SELECT DISTINCT sum(id) OVER () FROM wbig"),
        vec![(1..=5000i64).sum::<i64>().to_string()]
    );
}
