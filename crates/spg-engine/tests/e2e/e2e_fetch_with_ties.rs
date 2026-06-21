//! v7.17.0 Phase 3.P0-49 — `FETCH FIRST <n> ROWS WITH TIES`
//! (SQL:2008).
//!
//! Phase 5.1 (`v7.16` carve-out) made the parser accept WITH TIES
//! but discarded the flag — the planner full-LIMIT-truncated even
//! when the customer asked for ties. P0-49 captures the flag in
//! the AST and extends the truncated tail through every row that
//! shares the last-kept row's ORDER BY key.
//!
//! Lock in:
//!   * extension across a single tied ORDER BY key (basic case)
//!   * multi-key ORDER BY tie matches the full key tuple
//!   * `FETCH FIRST … ROWS ONLY` (no ties) unchanged
//!   * `WITH TIES` without `ORDER BY` errors (PG-canonical)
//!   * `WITH TIES` with `OFFSET` still extends past the post-
//!     offset cutoff
//!   * `WITH TIES` honours DESC ordering for the tie group
//!   * `WITH TIES` on a row whose key is unique (no tie) emits
//!     exactly N rows

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

fn seed_scores(e: &mut Engine) {
    e.execute("CREATE TABLE t (id INT NOT NULL, score INT NOT NULL)")
        .unwrap();
    // Scores: 100, 90, 80, 80, 80, 70, 60.
    // Top-3 by score DESC: ids 1, 2, then 3 (tied with 4 and 5).
    // WITH TIES at 3 should emit ids {1, 2, 3, 4, 5}.
    e.execute(
        "INSERT INTO t VALUES \
            (1, 100), (2, 90), (3, 80), (4, 80), (5, 80), (6, 70), (7, 60)",
    )
    .unwrap();
}

#[test]
fn fetch_first_3_with_ties_extends_through_ties() {
    let mut e = Engine::new();
    seed_scores(&mut e);
    let mut r = rows(
        e.execute("SELECT id FROM t ORDER BY score DESC FETCH FIRST 3 ROWS WITH TIES")
            .unwrap(),
    );
    r.sort_by_key(|row| match row[0] {
        Value::Int(n) => n,
        _ => panic!(),
    });
    let ids: Vec<i32> = r
        .into_iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            _ => panic!(),
        })
        .collect();
    assert_eq!(ids, vec![1, 2, 3, 4, 5]);
}

#[test]
fn fetch_first_3_rows_only_keeps_exactly_n() {
    let mut e = Engine::new();
    seed_scores(&mut e);
    let r = rows(
        e.execute("SELECT id FROM t ORDER BY score DESC FETCH FIRST 3 ROWS ONLY")
            .unwrap(),
    );
    assert_eq!(r.len(), 3);
    // First three by score DESC are id 1, 2, then one of {3, 4, 5}.
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(2));
    match r[2][0] {
        Value::Int(n) => assert!((3..=5).contains(&n)),
        _ => panic!(),
    }
}

#[test]
fn with_ties_without_order_by_errors() {
    let mut e = Engine::new();
    seed_scores(&mut e);
    let err = e
        .execute("SELECT id FROM t FETCH FIRST 3 ROWS WITH TIES")
        .expect_err("WITH TIES without ORDER BY must error");
    let msg = format!("{err:?}");
    assert!(msg.to_uppercase().contains("ORDER BY"));
}

#[test]
fn with_ties_no_tie_at_cutoff_keeps_exactly_n() {
    // Row 3 (score 80) has ties below, but FETCH FIRST 2 stops at
    // id 2 (score 90) — id 2's score has no tie, so result is
    // exactly 2 rows.
    let mut e = Engine::new();
    seed_scores(&mut e);
    let r = rows(
        e.execute("SELECT id FROM t ORDER BY score DESC FETCH FIRST 2 ROWS WITH TIES")
            .unwrap(),
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(2));
}

#[test]
fn with_ties_and_offset_extends_after_offset_cutoff() {
    // OFFSET 2 drops ids {1, 2}; then FETCH FIRST 2 WITH TIES
    // picks rows starting at score=80 (id 3). The 2nd kept row
    // (id 4, score=80) ties with id 5 (also 80), so the result
    // includes ids {3, 4, 5}.
    let mut e = Engine::new();
    seed_scores(&mut e);
    let mut r = rows(
        e.execute(
            "SELECT id FROM t ORDER BY score DESC \
             OFFSET 2 FETCH FIRST 2 ROWS WITH TIES",
        )
        .unwrap(),
    );
    r.sort_by_key(|row| match row[0] {
        Value::Int(n) => n,
        _ => panic!(),
    });
    let ids: Vec<i32> = r
        .into_iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            _ => panic!(),
        })
        .collect();
    assert_eq!(ids, vec![3, 4, 5]);
}

#[test]
fn with_ties_multi_key_order_by_matches_full_tuple() {
    // Two-key order: (score DESC, id ASC). Ties only count when
    // BOTH keys match — id is unique here so no rows tie. Result
    // should be exactly 2 rows.
    let mut e = Engine::new();
    seed_scores(&mut e);
    let r = rows(
        e.execute(
            "SELECT id FROM t ORDER BY score DESC, id ASC \
             FETCH FIRST 2 ROWS WITH TIES",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(2));
}

#[test]
fn with_ties_asc_order_by_extends_at_low_end() {
    // ORDER BY score ASC: FETCH FIRST 2 picks {7, 6}. id 6 has
    // score=70, no tie, so result is exactly 2 rows.
    // FETCH FIRST 3 picks {7, 6, then one of {3,4,5}}; the 3rd
    // row is score=80, which ties with two others → 5 rows total.
    let mut e = Engine::new();
    seed_scores(&mut e);
    let mut r = rows(
        e.execute("SELECT id FROM t ORDER BY score ASC FETCH FIRST 3 ROWS WITH TIES")
            .unwrap(),
    );
    r.sort_by_key(|row| match row[0] {
        Value::Int(n) => n,
        _ => panic!(),
    });
    let ids: Vec<i32> = r
        .into_iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            _ => panic!(),
        })
        .collect();
    assert_eq!(ids, vec![3, 4, 5, 6, 7]);
}

#[test]
fn fetch_first_larger_than_row_count_returns_all() {
    let mut e = Engine::new();
    seed_scores(&mut e);
    let r = rows(
        e.execute("SELECT id FROM t ORDER BY score DESC FETCH FIRST 100 ROWS WITH TIES")
            .unwrap(),
    );
    assert_eq!(r.len(), 7);
}
