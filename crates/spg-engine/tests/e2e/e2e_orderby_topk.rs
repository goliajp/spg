//! read01 B8 — streaming top-N for `ORDER BY … LIMIT k`. With 200 rows
//! and a small LIMIT the executor trims its accumulator down to the
//! running top-`keep` many times over the scan (keep ≪ rows), so these
//! assert that the trimmed result is still exactly the k rows a full
//! sort would keep. Values are deterministic: `v = g % 20` for
//! g ∈ 1..=200, i.e. each of 0..19 appears 10 times.

use spg_engine::testkit::EnvConfig;
use spg_engine::{Engine, QueryResult};

fn seed_engine(mut e: Engine) -> Engine {
    e.execute("CREATE TABLE topk (id INT, v INT)").unwrap();
    e.execute("INSERT INTO topk SELECT g, g % 20 FROM generate_series(1, 200) g")
        .unwrap();
    e
}

fn seed() -> Engine {
    seed_engine(Engine::new())
}

fn scalar(e: &mut Engine, sql: &str) -> i64 {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("expected Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::BigInt(n) => *n,
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::Null => -1,
        other => panic!("{sql}: expected integer, got {other:?}"),
    }
}

#[test]
fn topk_asc_selects_smallest() {
    let mut e = seed();
    // LIMIT 5: five smallest → all v = 0 (there are ten).
    assert_eq!(
        scalar(
            &mut e,
            "SELECT count(*) FROM (SELECT v FROM topk ORDER BY v LIMIT 5) s"
        ),
        5
    );
    assert_eq!(
        scalar(
            &mut e,
            "SELECT coalesce(sum(v),0) FROM (SELECT v FROM topk ORDER BY v LIMIT 5) s"
        ),
        0
    );
    assert_eq!(
        scalar(
            &mut e,
            "SELECT max(v) FROM (SELECT v FROM topk ORDER BY v LIMIT 5) s"
        ),
        0
    );

    // LIMIT 25 spans v=0 (10) + v=1 (10) + v=2 (5): sum = 20, max = 2.
    assert_eq!(
        scalar(
            &mut e,
            "SELECT count(*) FROM (SELECT v FROM topk ORDER BY v LIMIT 25) s"
        ),
        25
    );
    assert_eq!(
        scalar(
            &mut e,
            "SELECT sum(v) FROM (SELECT v FROM topk ORDER BY v LIMIT 25) s"
        ),
        20
    );
    assert_eq!(
        scalar(
            &mut e,
            "SELECT max(v) FROM (SELECT v FROM topk ORDER BY v LIMIT 25) s"
        ),
        2
    );
}

#[test]
fn topk_desc_selects_largest() {
    let mut e = seed();
    // DESC LIMIT 25: v=19 (10) + v=18 (10) + v=17 (5) → sum = 455, min = 17.
    assert_eq!(
        scalar(
            &mut e,
            "SELECT count(*) FROM (SELECT v FROM topk ORDER BY v DESC LIMIT 25) s"
        ),
        25
    );
    assert_eq!(
        scalar(
            &mut e,
            "SELECT sum(v) FROM (SELECT v FROM topk ORDER BY v DESC LIMIT 25) s"
        ),
        455
    );
    assert_eq!(
        scalar(
            &mut e,
            "SELECT min(v) FROM (SELECT v FROM topk ORDER BY v DESC LIMIT 25) s"
        ),
        17
    );
}

#[test]
fn topk_with_offset() {
    let mut e = seed();
    // keep = limit + offset = 25; after ordering, OFFSET 15 skips the
    // first 15 (v=0 ×10, v=1 ×5) and takes the next 10 (v=1 ×5, v=2 ×5)
    // → sum = 15, count = 10.
    let q = "SELECT {} FROM (SELECT v FROM topk ORDER BY v LIMIT 10 OFFSET 15) s";
    assert_eq!(scalar(&mut e, &q.replace("{}", "count(*)")), 10);
    assert_eq!(scalar(&mut e, &q.replace("{}", "sum(v)")), 15);
}

#[test]
fn topk_matches_full_sort_under_disable_gate() {
    // The result must be identical whether the streaming trim runs or the
    // full-sort fallback does — same query, one engine each way.
    let q = "SELECT sum(v) FROM (SELECT v FROM topk ORDER BY v LIMIT 37) s";
    let mut streamed_engine = seed();
    let mut full_engine =
        seed_engine(Engine::new().with_env_cfg(EnvConfig::builder().disable_topk(true).build()));
    let streamed = scalar(&mut streamed_engine, q);
    let full = scalar(&mut full_engine, q);
    assert_eq!(streamed, full, "streaming top-N diverged from full sort");
    // 37 = v0..v2 (30 rows, sum 30) + v3 ×7 (sum 21) → 51.
    assert_eq!(streamed, 51);
}
