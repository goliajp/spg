//! v7.39 (round 214) — EXCLUDE intra-batch O(N²)→O(N log N) fast path. A
//! single multi-row INSERT / COPY of a booking table used to run a pairwise
//! O(N²) overlap check over the batch (measured O(N²), r213: 3.36s for 8000
//! rows). For the common single-`&&` form the sorted-adjacency test proves
//! disjointness in O(N log N) (~4ms for 8000). These pins lock CORRECTNESS,
//! not speed: a large disjoint batch is accepted, and an overlap buried
//! anywhere in a large batch is still rejected (the fast path falls back to
//! the exact loop, which produces PG's byte-identical error).

use spg_engine::Engine;

fn excl_table() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE bk (during int4range, EXCLUDE USING gist (during WITH &&))")
        .unwrap();
    e
}

#[test]
fn large_disjoint_batch_accepted() {
    let mut e = excl_table();
    let n = 2000;
    let mut sql = String::from("INSERT INTO bk VALUES ");
    for i in 0..n {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push_str(&format!("('[{},{})')", 2 * i, 2 * i + 1));
    }
    e.execute(&sql).unwrap();
}

#[test]
fn overlap_buried_mid_batch_rejected() {
    // Row 1000 duplicates row 0's range — the fast sorted-adjacency test
    // must still catch it (soundness: a non-adjacent overlap always implies
    // an adjacent one after sorting).
    let mut e = excl_table();
    let n = 2000;
    let mut sql = String::from("INSERT INTO bk VALUES ");
    for i in 0..n {
        if i > 0 {
            sql.push_str(", ");
        }
        let (lo, hi) = if i == 1000 {
            (0, 1)
        } else {
            (2 * i, 2 * i + 1)
        };
        sql.push_str(&format!("('[{lo},{hi})')"));
    }
    let err = e.execute(&sql).unwrap_err().to_string();
    assert!(
        err.contains("conflicting key value violates exclusion constraint"),
        "{err}"
    );
}

#[test]
fn adjacent_touching_ranges_accepted() {
    // [0,2), [2,4), [4,6) — touch at exclusive bounds, no overlap. The fast
    // path's adjacency check must NOT false-positive on a shared boundary.
    let mut e = excl_table();
    e.execute("INSERT INTO bk VALUES ('[0,2)'), ('[2,4)'), ('[4,6)'), ('[6,8)')")
        .unwrap();
}

#[test]
fn batch_with_nulls_and_empty_ranges_disjoint() {
    // NULL and empty ranges are exempt; the surrounding real ranges are
    // disjoint, so the whole batch is accepted.
    let mut e = excl_table();
    e.execute("INSERT INTO bk VALUES ('[1,3)'), (NULL), ('empty'), ('[5,7)'), (NULL)")
        .unwrap();
}

#[test]
fn batch_null_does_not_mask_a_real_overlap() {
    // A NULL in the batch must not perturb overlap detection among the real
    // ranges: [1,5) and [3,7) still collide.
    let mut e = excl_table();
    let err = e
        .execute("INSERT INTO bk VALUES ('[1,5)'), (NULL), ('[3,7)')")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("conflicting key value violates exclusion constraint"),
        "{err}"
    );
}
