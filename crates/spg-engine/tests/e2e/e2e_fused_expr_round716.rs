//! Round 716 — the fused aggregate lane accepts COMPILED argument
//! expressions (S07). `count(least(id, 0))` used to fall off the lane —
//! `fused_layout` only took bound columns — and landed in the SERIAL
//! generic per-row loop; PG runs the same cell as a parallel seq scan,
//! and that one structural difference was most of the P2/P3/P4 package:
//! measured on the 500k-row panel, `coalesce(nullif(s,'row1'),'z')`
//! went 4.00× → 0.75×, `greatest(id,0)` 5.44× → 0.98×,
//! `s::VARCHAR(20)` 9.32× → 0.90×, `to_char(t,…)` 3.21× → 0.71×.
//!
//! These pins hold the lane's ANSWERS in place: every shape below ran
//! through the round-716 differential against PG18 (12/12 byte-same,
//! both the anonymous-group lane and the single-int-GROUP-BY lane).
//! The engine here has no parallel runner, so the pins exercise the
//! serial fused path — the same op arms the shards run.

use spg_engine::{Engine, QueryResult};

fn row_text(e: &mut Engine, sql: &str) -> String {
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
            .join(" / "),
        other => panic!("{other:?}"),
    }
}

fn seed(e: &mut Engine) {
    e.execute("CREATE TABLE f716 (id INT, g INT, s TEXT)").unwrap();
    e.execute(
        "INSERT INTO f716 SELECT gg, gg % 3, 'row' || gg FROM generate_series(1, 100) gg",
    )
    .unwrap();
}

/// The anonymous-group lane: count/sum/avg/min/max over compiled
/// arguments, NULL-producing arguments counted as PG counts them.
#[test]
fn round716_fused_compiled_args_anonymous_group() {
    let mut e = Engine::new();
    seed(&mut e);
    // PG18 answers (round-716 differential, seeded 1..=100, g = id % 3).
    for (sql, want) in [
        ("SELECT count(least(id, 0)) FROM f716", "100"),
        // nullif(id, 1) is NULL exactly once.
        ("SELECT count(nullif(id, 1)) FROM f716", "99"),
        ("SELECT sum(least(id, 50)) FROM f716", "3775"),
        ("SELECT min(greatest(id, 7)) FROM f716", "7"),
        ("SELECT max(mod(id, 7)) FROM f716", "6"),
        ("SELECT count(coalesce(nullif(s, 'row1'), 'z')) FROM f716", "100"),
        // Mixed bound-column and compiled specs share one scan.
        (
            "SELECT count(*), count(id), sum(id + 0), min(least(id, 5)) FROM f716",
            "100|100|5050|1",
        ),
    ] {
        assert_eq!(row_text(&mut e, sql), want, "{sql}");
    }
}

/// The single-int-GROUP-BY lane runs the same ops per group.
#[test]
fn round716_fused_compiled_args_grouped() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        row_text(
            &mut e,
            "SELECT g, count(nullif(id, 1)), sum(least(id, 3)) \
             FROM f716 GROUP BY g ORDER BY g",
        ),
        // PG18's answer, measured round 716 (not derived by hand — the
        // hand derivation got two of these wrong before the probe ran).
        "0|33|99 / 1|33|100 / 2|33|98",
    );
}

/// An argument expression that errors mid-scan aborts the aggregate —
/// the lane must propagate, not swallow.
#[test]
fn round716_fused_compiled_arg_error_propagates() {
    let mut e = Engine::new();
    seed(&mut e);
    let err = format!(
        "{}",
        e.execute("SELECT sum(id / (id - id)) FROM f716")
            .expect_err("division by zero reaches the caller")
    );
    assert!(err.contains("division by zero"), "{err}");
}
