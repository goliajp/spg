//! v7.38 元机制 D unit pin — `SPG_TEST_DISABLE_TOPK=1` skips the
//! `partial_sort_tagged` fast path so an ORDER BY … LIMIT query falls
//! back to the full-sort branch. The user-visible result must stay
//! byte-equal between the two paths (that is the whole correctness
//! contract of the fast path); only the executor branch differs.
//!
//! Acceptor: `crates/spg-engine/src/lib.rs` two `partial_sort_tagged`
//! call sites (`self.env_cfg.disable_topk`).
//! Index   : `xtests/sigil/test-mode-gucs.md`.

use spg_engine::testkit::EnvConfig;
use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(r: &QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.iter().map(|row| row.values.clone()).collect(),
        _ => panic!("expected Rows"),
    }
}

fn seed_and_query(engine: &mut Engine) -> Vec<Vec<Value<'static>>> {
    engine
        .execute("CREATE TABLE m (id INT NOT NULL, k INT NOT NULL)")
        .unwrap();
    // Adversarial ordering: a TopK pivot vs a full sort must agree on
    // the top-K rows even when keys collide. Mix duplicates so the
    // `select_nth_unstable_by` partition has real choice to make.
    let pairs: [(i32, i32); 12] = [
        (0, 9),
        (1, 8),
        (2, 7),
        (3, 6),
        (4, 5),
        (5, 4),
        (6, 3),
        (7, 2),
        (8, 1),
        (9, 0),
        (10, 5),
        (11, 5),
    ];
    for (id, k) in pairs {
        engine
            .execute(&format!("INSERT INTO m VALUES ({id}, {k})"))
            .unwrap();
    }
    let r = engine
        .execute("SELECT id, k FROM m ORDER BY k DESC, id ASC LIMIT 3")
        .unwrap();
    rows_of(&r)
}

#[test]
fn disable_topk_produces_byte_equal_results() {
    let mut on = Engine::new(); // production: TopK on
    let mut off = Engine::new().with_env_cfg(EnvConfig::builder().disable_topk(true).build());

    let on_rows = seed_and_query(&mut on);
    let off_rows = seed_and_query(&mut off);

    // Correctness contract: same ORDER BY + LIMIT must yield same
    // rows whether or not the TopK fast path is taken.
    assert_eq!(on_rows, off_rows);
    assert_eq!(on_rows.len(), 3, "LIMIT 3 must trim to 3 rows");
}

#[test]
fn disable_topk_full_sort_still_honours_limit() {
    // Sanity: even on the slow path the LIMIT trims to the declared
    // row count — disable_topk must not turn LIMIT into a no-op.
    let mut off = Engine::new().with_env_cfg(EnvConfig::builder().disable_topk(true).build());
    let rows = seed_and_query(&mut off);
    assert_eq!(rows.len(), 3);
    // The top row by k DESC is id=0 (k=9).
    assert_eq!(rows[0][0], Value::Int(0));
    assert_eq!(rows[0][1], Value::Int(9));
}
