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

/// v7.39 (round 717) — GREATEST / LEAST compile to their own step
/// (`Step::Extremum`): the uniform-type fast path compares in place,
/// and these are the shapes that must still FALL BACK to the function
/// arm and answer exactly as before. Expectations measured on PG18
/// round 717: `greatest(3, 2.5)` widens to numeric 3, all-NULL is
/// NULL, unknown-type text literals compare as text.
#[test]
fn round717_extremum_fallback_shapes_unchanged() {
    let mut e = Engine::new();
    seed(&mut e);
    for (sql, want) in [
        // Mixed int/numeric widens the winner (PG: numeric 3).
        ("SELECT greatest(3, 2.5)", "3"),
        ("SELECT least(3, 2.5)", "2.5"),
        // All-NULL answers NULL ("NULL" is the harness's rendering).
        ("SELECT greatest(NULL::int, NULL::int)", "NULL"),
        // Unknown-type literals compare as text.
        ("SELECT least('14:30', '09:00')", "09:00"),
        // NULLs are ignored, not poisoning (PG dialect).
        ("SELECT count(greatest(nullif(id, 1), 0)) FROM f716", "100"),
        // Per-row uniform fast path over two columns.
        ("SELECT sum(greatest(id, g)) FROM f716", "5050"),
    ] {
        assert_eq!(row_text(&mut e, sql), want, "{sql}");
    }
}

/// v7.39 (round 724) — string_agg / array_agg ride the fused parallel
/// lanes (`FusedOp::Collect`): shard collection concatenates in shard
/// order (= row order), ORDER BY keys collect flat per row, and the
/// ordinary finalize does the sort and join. Expectations are the
/// round-724 PG18 differential (10/10 byte-same, including multi-key
/// DESC orders and a 100k md5-pinned concatenation). This engine has
/// no parallel runner, so these exercise the serial arm of the same
/// ops the shards run.
#[test]
fn round724_collect_aggregates_answer_as_pg() {
    let mut e = Engine::new();
    seed(&mut e);
    for (sql, want) in [
        (
            "SELECT string_agg(s, '|' ORDER BY id) FROM f716 WHERE id <= 4",
            "row1|row2|row3|row4",
        ),
        (
            "SELECT string_agg(s, ',' ORDER BY id DESC) FROM f716 WHERE id <= 3",
            "row3,row2,row1",
        ),
        (
            "SELECT g, string_agg(s, ',') FROM f716 WHERE id <= 6 GROUP BY g ORDER BY g",
            "0|row3,row6 / 1|row1,row4 / 2|row2,row5",
        ),
        (
            "SELECT array_agg(id ORDER BY id DESC) FROM f716 WHERE id <= 4",
            "{4,3,2,1}",
        ),
        // Mixed with plain fused specs on the same scan.
        (
            "SELECT g, string_agg(s, ',' ORDER BY id), count(*), max(id)              FROM f716 WHERE id <= 6 GROUP BY g ORDER BY g",
            "0|row3,row6|2|6 / 1|row1,row4|2|4 / 2|row2,row5|2|5",
        ),
    ] {
        assert_eq!(row_text(&mut e, sql), want, "{sql}");
    }
}

/// v7.39 (round 726) — the batch SRF path builds ONE plan per scan and
/// clones the base row once per input row (slots rewritten per output
/// row). These pin the expansion's answers — round-726 differential,
/// 7/7 byte-same (multi-SRF padding, text arrays, generate_series over
/// a column, ORDER BY the expanded item).
#[test]
fn round726_srf_expansion_answers_as_pg() {
    let mut e = Engine::new();
    seed(&mut e);
    for (sql, want) in [
        (
            "SELECT count(*) FROM (SELECT unnest(ARRAY[id, g]) v FROM f716 WHERE id <= 50) q",
            "100",
        ),
        (
            "SELECT sum(v) FROM (SELECT unnest(ARRAY[id, g]) v FROM f716 WHERE id <= 10) q",
            "65",
        ),
        // Two SRFs of different lengths: the shorter pads with NULL.
        (
            "SELECT unnest(ARRAY[id]), unnest(ARRAY[g, id, 7]) FROM f716 WHERE id = 5",
            "5|2 / NULL|5 / NULL|7",
        ),
        // A projected non-SRF column repeats per expanded row.
        (
            "SELECT s, unnest(ARRAY[id, g]) FROM f716 WHERE id = 3",
            "row3|3 / row3|0",
        ),
    ] {
        assert_eq!(row_text(&mut e, sql), want, "{sql}");
    }
}

/// v7.39 (round 727) — a SIMPLE derived table flattens (PG's subquery
/// pull-up): bare-column projection over one stored table rewrites to
/// the unwrapped form and gets the parallel lanes back. These pin the
/// SEMANTICS the rewrite must preserve — alias visibility, WHERE
/// conjunction, GROUP BY through the alias — and the two shapes that
/// must NOT be legalised by it (round-727 differential, 8/8 plus two
/// error shapes whose only difference is PG's LINE decoration).
#[test]
fn round727_simple_derived_flattens_and_answers_as_pg() {
    let mut e = Engine::new();
    seed(&mut e);
    for (sql, want) in [
        ("SELECT count(*) FROM (SELECT id v FROM f716 WHERE id <= 50) q", "50"),
        ("SELECT max(v) FROM (SELECT id v FROM f716 WHERE id <= 50) q", "50"),
        // Outer WHERE over the alias conjoins with the inner filter.
        (
            "SELECT count(*) FROM (SELECT id v FROM f716 WHERE id <= 50) q WHERE v > 20",
            "30",
        ),
        // GROUP BY through the alias.
        (
            "SELECT v, count(*) FROM (SELECT g v FROM f716 WHERE id <= 30) q              GROUP BY v ORDER BY v",
            "0|10 / 1|10 / 2|10",
        ),
        // Qualified references and an inner table alias.
        (
            "SELECT sum(v) FROM (SELECT id AS v FROM f716 ff WHERE ff.id <= 10) q",
            "55",
        ),
    ] {
        assert_eq!(row_text(&mut e, sql), want, "{sql}");
    }
    // A name q does not export stays an ERROR — flattening must not
    // legalise it against the base table.
    let err = format!(
        "{}",
        e.execute("SELECT s FROM (SELECT id v FROM f716) q")
            .expect_err("s is not exported by q")
    );
    assert!(err.contains("does not exist") || err.contains("not found"), "{err}");
}
