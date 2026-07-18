//! v7.39 (round 216) — the UPDATE existing-row scan now rides the persistent
//! range-exclusion index too (INSERT got it in r215). Each planned new row
//! probes O(log n), EXCLUDING the rows being updated in the same statement
//! (their pre-images are replaced). Measured: single-row UPDATE stream
//! N=8000 2.13s→0.065s (~33x), ratio ~2.0 = O(N log N). These pins lock
//! correctness: an UPDATE into another row's range is rejected via the index;
//! an UPDATE that only touches its own range is accepted.

use spg_engine::Engine;

fn booking(n: usize) -> Engine {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE bk (id int PRIMARY KEY, during int4range, \
         EXCLUDE USING gist (during WITH &&))",
    )
    .unwrap();
    for i in 0..n {
        e.execute(&format!(
            "INSERT INTO bk VALUES ({i}, '[{},{})')",
            10 * i,
            10 * i + 5
        ))
        .unwrap();
    }
    e
}

#[test]
fn update_into_other_rows_range_rejected_at_scale() {
    let mut e = booking(300);
    // Move row 0 onto row 100's range [1000,1005) → overlap → rejected.
    let err = e
        .execute("UPDATE bk SET during = '[1001,1004)' WHERE id = 0")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("conflicting key value violates exclusion constraint"),
        "{err}"
    );
}

#[test]
fn update_within_own_range_accepted() {
    // Shrinking a row within its own range must not self-collide (the probe
    // excludes the updated row's pre-image).
    let mut e = booking(300);
    e.execute("UPDATE bk SET during = '[0,3)' WHERE id = 0")
        .unwrap();
}

#[test]
fn update_into_gap_accepted() {
    // A gap between existing rows (…,5)…[10,…) is free.
    let mut e = booking(300);
    e.execute("UPDATE bk SET during = '[6,9)' WHERE id = 0")
        .unwrap();
}

#[test]
fn update_stream_then_overlap_rejected() {
    // After moving every row to a fresh non-overlapping block (the O(log n)
    // stream), an insert onto one of the moved ranges is still caught.
    let mut e = booking(200);
    for i in 0..200 {
        let lo = 100_000 + 10 * i;
        e.execute(&format!(
            "UPDATE bk SET during = '[{lo},{})' WHERE id = {i}",
            lo + 5
        ))
        .unwrap();
    }
    let err = e
        .execute("INSERT INTO bk VALUES (9999, '[100501,100504)')")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("conflicting key value violates exclusion constraint"),
        "{err}"
    );
}

#[test]
fn two_row_swap_update_accepted() {
    // Swapping two rows' ranges in one multi-row UPDATE: both pre-images are
    // excluded, so the swap doesn't self-reject.
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE bk (id int PRIMARY KEY, during int4range, \
         EXCLUDE USING gist (during WITH &&))",
    )
    .unwrap();
    e.execute("INSERT INTO bk VALUES (1, '[1,5)'), (2, '[10,15)')")
        .unwrap();
    // id=1 → [10,15), id=2 → [1,5): a full swap. Each new range would collide
    // with the OTHER's pre-image, but both are being updated → accepted.
    e.execute(
        "UPDATE bk SET during = CASE id WHEN 1 THEN '[10,15)'::int4range \
         ELSE '[1,5)'::int4range END WHERE id IN (1, 2)",
    )
    .unwrap();
}
