//! 8.0.3 — two arbiter shapes that answered "no such key" and let the
//! row be refused by the very index that should have arbitrated it.
//!
//! 1. An explicit target covered only by a PARTIAL unique index —
//!    sentori's `notifier/service.rs:112`,
//!    `ON CONFLICT (dedup_key) WHERE dedup_key IS NOT NULL DO NOTHING`.
//!    The single-column existence probe consults only an index with no
//!    predicate, and the explicit path carried no predicate to send it
//!    down the predicate-aware scan instead. The composite form took a
//!    row scan anyway, which is why only the single-column one failed.
//! 2. An UNTARGETED `ON CONFLICT` over a full COMPOSITE unique index —
//!    found by crossing every arbiter axis against PG 18.6 (target
//!    spelling × arity × constraint / index × full / partial × column
//!    type × action × the update arm's other key; 132 cases), the only
//!    shape still diverging. The untargeted path kept the index's
//!    leading column as if it were the whole key.
//!
//! Every expectation is PostgreSQL 18.6's for the same statements.

use spg_engine::{Engine, QueryResult};

fn setup(sqls: &[&str]) -> Engine {
    let mut e = Engine::new();
    for s in sqls {
        e.execute(s).unwrap_or_else(|x| panic!("{s}: {x:?}"));
    }
    e
}

fn affected(e: &mut Engine, sql: &str) -> u64 {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::CommandOk { affected, .. } => affected as u64,
        QueryResult::Rows { rows, .. } => rows.len() as u64,
        other => panic!("{sql}: {other:?}"),
    }
}

fn rows(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join(":")
            })
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn a_single_column_partial_arbiter_skips_the_duplicate() {
    for (ty, v) in [("int", "1"), ("text", "'a'")] {
        let mut e = setup(&[
            &format!("CREATE TABLE sp (k {ty}, n int)"),
            "CREATE UNIQUE INDEX sp_k ON sp (k) WHERE k IS NOT NULL",
            &format!("INSERT INTO sp VALUES ({v}, 1)"),
        ]);
        assert_eq!(
            affected(
                &mut e,
                &format!(
                    "INSERT INTO sp VALUES ({v}, 2) ON CONFLICT (k) WHERE k IS NOT NULL DO NOTHING"
                )
            ),
            0,
            "{ty}: PG answers INSERT 0 0"
        );
    }
}

#[test]
fn a_predicate_on_another_column_is_carried_too() {
    let mut e = setup(&[
        "CREATE TABLE sp_o (k int, n int)",
        "CREATE UNIQUE INDEX sp_o_k ON sp_o (k) WHERE n > 0",
        "INSERT INTO sp_o VALUES (1, 1)",
    ]);
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO sp_o VALUES (1, 2) ON CONFLICT (k) WHERE n > 0 DO NOTHING"
        ),
        0
    );
}

#[test]
fn do_update_on_a_single_column_partial_arbiter_updates() {
    let mut e = setup(&[
        "CREATE TABLE sp_u (k int, n int)",
        "CREATE UNIQUE INDEX sp_u_k ON sp_u (k) WHERE k IS NOT NULL",
        "INSERT INTO sp_u VALUES (1, 1)",
    ]);
    e.execute("INSERT INTO sp_u VALUES (1, 2) ON CONFLICT (k) WHERE k IS NOT NULL DO UPDATE SET n = EXCLUDED.n")
        .expect("PG answers INSERT 0 1");
    assert_eq!(rows(&mut e, "SELECT k, n FROM sp_u"), "1:2");
}

#[test]
fn an_untargeted_conflict_over_a_composite_unique_index_skips_the_duplicate() {
    for (ty, v) in [("int", "1"), ("text", "'a'"), ("text COLLATE \"C\"", "'a'")] {
        let mut e = setup(&[
            &format!("CREATE TABLE ci (k1 {ty}, k2 {ty}, v int)"),
            "CREATE UNIQUE INDEX ci_k ON ci (k1, k2)",
            &format!("INSERT INTO ci VALUES ({v}, {v}, 1)"),
        ]);
        assert_eq!(
            affected(
                &mut e,
                &format!("INSERT INTO ci VALUES ({v}, {v}, 2) ON CONFLICT DO NOTHING")
            ),
            0,
            "{ty}: PG answers INSERT 0 0"
        );
        assert_eq!(rows(&mut e, "SELECT v FROM ci"), "1");
    }
}

/// The controls that already answered, beside the two that did not.
#[test]
fn the_adjacent_arbiters_still_answer() {
    let mut e = setup(&[
        "CREATE TABLE cp (k int, t int)",
        "CREATE UNIQUE INDEX cp_kt ON cp (k, t) WHERE t IS NOT NULL",
        "INSERT INTO cp VALUES (1, 1)",
        "CREATE TABLE fu (k int, n int)",
        "CREATE UNIQUE INDEX fu_k ON fu (k)",
        "INSERT INTO fu VALUES (1, 1)",
    ]);
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO cp VALUES (1, 1) ON CONFLICT (k, t) WHERE t IS NOT NULL DO NOTHING"
        ),
        0
    );
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO fu VALUES (1, 2) ON CONFLICT (k) DO NOTHING"
        ),
        0
    );
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO fu VALUES (2, 2) ON CONFLICT (k) DO NOTHING"
        ),
        1
    );
}
