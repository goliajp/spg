//! v7.39 (round 574) — the join's side filter compiles its predicate,
//! as the single-table scan has since round 479.
//!
//! Round 573 refuted the obvious fix and left one instruction: find
//! which path the query actually takes before writing more code. The
//! profile answered it — `exec_joined_select` ->
//! `build_joined_filtered_rows` -> `filter_table_indices`, and that last
//! one is 32% of the connection thread with `eval_expr` about 17% of the
//! total inside it. It evaluated every conjunct INTERPRETIVELY for every
//! row: 500,000 evaluations to find 100 rows.
//!
//! `run_single_table_scan` compiles its WHERE and evaluates with
//! `eval_compiled_pred`. The join's side seed never learned to. It does
//! now, with the interpretive path kept for any conjunct that will not
//! compile.
//!
//! Over pgwire, `SELECT count(*) FROM a JOIN b ON a.id = b.id WHERE
//! b.id < 100` on 500k rows a side, three paired batches of five:
//!
//!     before  75.59 / 76.51 / 81.10 ms      after  70.34 / 73.31 / 65.93
//!
//! -10.1%, lower in 3 of 3 — and the size agrees with what the profile
//! predicted, which is the reason to believe it. The other join shapes
//! moved less or not at all: this takes out the interpretive evaluation,
//! not the rest of what round 573 measured, and the self-join is still
//! 4-6x PG.
//!
//! The risk in swapping evaluators is that the two disagree somewhere.
//! The pins below run the same predicate shapes through a join's side
//! filter and through the plain single-table path, and require the same
//! answers: three-valued logic against NULLs, an OR, a NOT, a BETWEEN,
//! an IN list, a LIKE, and expressions that do not compile at all.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE f574 (id INT, g INT, t TEXT)").unwrap();
    e.execute(
        "INSERT INTO f574 SELECT gg, CASE WHEN gg % 11 = 0 THEN NULL ELSE gg % 7 END, \
         CASE WHEN gg % 13 = 0 THEN NULL ELSE 'r' || gg END FROM generate_series(1, 600) gg",
    )
    .unwrap();
    e
}

/// For each predicate: the count through a join's side filter must equal
/// the count the plain single-table path gives.
#[test]
fn round574_join_side_filter_agrees_with_the_plain_scan() {
    let mut e = engine();
    for pred in [
        "b.id < 100",
        "b.id BETWEEN 50 AND 150",
        "b.g = 3",
        // Three-valued: NULL is not true, and `<> 3` must not keep the
        // NULL rows either.
        "b.g <> 3",
        "b.g IS NULL",
        "b.g IS NOT NULL",
        "b.id < 100 OR b.g = 3",
        "NOT (b.id < 100)",
        "b.id IN (1, 2, 3, 500, 9999)",
        "b.t LIKE 'r1%'",
        "b.t IS NULL",
        // Arithmetic and a function — nearer the edge of what compiles.
        "b.id % 3 = 0",
        "b.id + 1 > 500",
        "length(b.t) = 4",
        "upper(b.t) = 'R42'",
        "coalesce(b.g, -1) = -1",
    ] {
        let plain = one(&mut e, &format!("SELECT count(*) FROM f574 b WHERE {pred}"));
        let joined = one(
            &mut e,
            &format!("SELECT count(*) FROM f574 a JOIN f574 b ON a.id = b.id WHERE {pred}"),
        );
        assert_eq!(joined, plain, "predicate `{pred}`");
    }
}

/// The same, with the predicate on the DRIVING side, and with both.
#[test]
fn round574_either_side_and_both() {
    let mut e = engine();
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM f574 a JOIN f574 b ON a.id = b.id WHERE a.g IS NULL"
        ),
        one(&mut e, "SELECT count(*) FROM f574 a WHERE a.g IS NULL")
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM f574 a JOIN f574 b ON a.id = b.id \
             WHERE a.id < 100 AND b.g = 3"
        ),
        one(
            &mut e,
            "SELECT count(*) FROM f574 x WHERE x.id < 100 AND x.g = 3"
        )
    );
    // A conjunct that spans both sides cannot be pushed to either and
    // must still be enforced.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM f574 a JOIN f574 b ON a.id = b.id WHERE a.id < b.id + 1"
        ),
        "600"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM f574 a JOIN f574 b ON a.id = b.id WHERE a.id < b.id"
        ),
        "0"
    );
}

/// A LEFT join's unmatched rows and its NULL-extended peer columns are
/// unaffected — a peer predicate is not pushed under an outer join.
#[test]
fn round574_outer_join_semantics_hold() {
    let mut e = engine();
    e.execute("CREATE TABLE s574 (id INT)").unwrap();
    e.execute("INSERT INTO s574 SELECT gg FROM generate_series(1, 50) gg")
        .unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM f574 a LEFT JOIN s574 b ON a.id = b.id"
        ),
        "600"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM f574 a LEFT JOIN s574 b ON a.id = b.id WHERE b.id IS NULL"
        ),
        "550"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM f574 a LEFT JOIN s574 b ON a.id = b.id WHERE b.id < 10"
        ),
        "9"
    );
}

/// The values, not just the counts — a compiled predicate that kept the
/// wrong rows would still count right if it kept as many.
#[test]
fn round574_the_rows_themselves() {
    let mut e = engine();
    let rows = |e: &mut Engine, sql: &str| -> Vec<String> {
        match e.execute(sql).unwrap() {
            QueryResult::Rows { rows, .. } => rows
                .iter()
                .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
                .collect(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(
        rows(
            &mut e,
            "SELECT b.id FROM f574 a JOIN f574 b ON a.id = b.id \
             WHERE b.g = 3 AND b.id < 30 ORDER BY b.id"
        ),
        vec!["3", "10", "17", "24"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT b.id FROM f574 a JOIN f574 b ON a.id = b.id \
             WHERE b.g IS NULL AND b.id < 40 ORDER BY b.id"
        ),
        vec!["11", "22", "33"]
    );
}
