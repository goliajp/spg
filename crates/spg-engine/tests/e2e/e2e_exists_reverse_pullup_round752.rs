//! Round 752 — the EXISTS pull-up's REVERSE correlation form: an outer
//! bare column equal to an inner-only integer expression
//! (`WHERE a.id = b.id + 1`). The round-721 ledger's second entry: it
//! ran the per-row correlated executor, which at the panel's 500k scale
//! is not merely slow but effectively unbounded (the round-752 yardstick
//! form never finished a 600 s window), while the pulled-up join answers
//! in join time. The pair's inner half widened from a column name to
//! `InnerHalf::{Col, IntExpr}`; the ON it emits (`<fresh int expr> =
//! <outer int col>`) is exactly the round-719 i64 build-expr lane.
//!
//! Answer pins are PG18-measured (round-752 differential, 12/12
//! byte-identical over the wire). The seed deliberately carries NULL
//! keys on both sides and duplicate inner matches (ids 5-9 twice) so
//! the semi join's no-multiplication and NOT EXISTS three-valued logic
//! are pinned by content, not just by counts.

use spg_engine::subquery::EXISTS_PULLUP_FIRE_COUNT;
use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
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
            .join(";"),
        other => panic!("{sql} -> {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE r752a (id INT, g INT)").unwrap();
    e.execute("CREATE TABLE r752b (id INT, g INT)").unwrap();
    e.execute("INSERT INTO r752a SELECT gg, gg % 5 FROM generate_series(1, 30) gg")
        .unwrap();
    e.execute("INSERT INTO r752a VALUES (NULL, 1), (NULL, 2)")
        .unwrap();
    e.execute("INSERT INTO r752b SELECT gg, gg % 7 FROM generate_series(1, 20) gg")
        .unwrap();
    // Duplicate inner matches: a semi join must not multiply outer rows.
    e.execute("INSERT INTO r752b SELECT gg, gg % 7 FROM generate_series(5, 9) gg")
        .unwrap();
    e.execute("INSERT INTO r752b VALUES (NULL, 3)").unwrap();
    e
}

#[test]
fn round752_reverse_form_answers_as_pg() {
    let mut e = seeded();
    for (sql, want) in [
        // Reverse positive, both operand orders.
        (
            "SELECT count(*) FROM r752a a WHERE EXISTS \
             (SELECT 1 FROM r752b b WHERE a.id = b.id + 1)",
            "20",
        ),
        (
            "SELECT count(*) FROM r752a a WHERE EXISTS \
             (SELECT 1 FROM r752b b WHERE b.id + 1 = a.id)",
            "20",
        ),
        // Reverse negative — the two NULL-id outer rows count as
        // no-match (their Eq is UNKNOWN for every inner row).
        (
            "SELECT count(*) FROM r752a a WHERE NOT EXISTS \
             (SELECT 1 FROM r752b b WHERE a.id = b.id + 1)",
            "12",
        ),
        // Two inner columns in the expression.
        (
            "SELECT count(*) FROM r752a a WHERE NOT EXISTS \
             (SELECT 1 FROM r752b b WHERE a.id = b.id + b.g)",
            "12",
        ),
        // Reverse + all-inner residual.
        (
            "SELECT count(*) FROM r752a a WHERE EXISTS \
             (SELECT 1 FROM r752b b WHERE a.id = b.id + 1 AND b.g > 3)",
            "9",
        ),
        // Mixed pair: bare column pair + reverse computed pair.
        (
            "SELECT count(*) FROM r752a a WHERE EXISTS \
             (SELECT 1 FROM r752b b WHERE b.g = a.g AND a.id = b.id + 3)",
            "5",
        ),
        (
            "SELECT count(*) FROM r752a a WHERE NOT EXISTS \
             (SELECT 1 FROM r752b b WHERE b.g = a.g AND a.id = b.id + 3)",
            "27",
        ),
        // Literal arithmetic inner half.
        (
            "SELECT count(*) FROM r752a a WHERE NOT EXISTS \
             (SELECT 1 FROM r752b b WHERE a.id = b.id * 2)",
            "17",
        ),
        // Guard forms: no correlation / outer column inside the inner
        // expression — the pull-up must refuse, the answer must match.
        (
            "SELECT count(*) FROM r752a a WHERE EXISTS \
             (SELECT 1 FROM r752b b WHERE a.id = 5)",
            "1",
        ),
        (
            "SELECT count(*) FROM r752a a WHERE EXISTS \
             (SELECT 1 FROM r752b b WHERE a.id = b.id + a.g)",
            "20",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
    // Row-level content, not just counts.
    assert_eq!(
        one(
            &mut e,
            "SELECT a.id, a.g FROM r752a a WHERE EXISTS \
             (SELECT 1 FROM r752b b WHERE a.id = b.id + 1) ORDER BY a.id"
        ),
        "2|2;3|3;4|4;5|0;6|1;7|2;8|3;9|4;10|0;11|1;12|2;13|3;14|4;15|0;\
         16|1;17|2;18|3;19|4;20|0;21|1",
    );
}

#[test]
fn round752_reverse_form_engages_the_pullup() {
    use std::sync::atomic::Ordering::Relaxed;
    // Monotone counter assertion — parallel-runner safe: concurrent
    // tests can only push the counter further up (the I06 lesson).
    let mut e = seeded();
    let before = EXISTS_PULLUP_FIRE_COUNT.load(Relaxed);
    let _ = e
        .execute(
            "SELECT count(*) FROM r752a a WHERE EXISTS \
             (SELECT 1 FROM r752b b WHERE a.id = b.id + 1)",
        )
        .unwrap();
    assert!(
        EXISTS_PULLUP_FIRE_COUNT.load(Relaxed) > before,
        "the reverse form must pull up, not run the correlated executor"
    );
    let before = EXISTS_PULLUP_FIRE_COUNT.load(Relaxed);
    let _ = e
        .execute(
            "SELECT count(*) FROM r752a a WHERE NOT EXISTS \
             (SELECT 1 FROM r752b b WHERE a.id = b.id + 1)",
        )
        .unwrap();
    assert!(
        EXISTS_PULLUP_FIRE_COUNT.load(Relaxed) > before,
        "the reverse anti-join must pull up too"
    );
}
