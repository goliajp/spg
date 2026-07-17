//! v7.39 (read01 round 120, Track A — execSRF.c 补读) — set-returning-function
//! execution semantics, locked byte-identical against PG 18.4.
//!
//! Read-driven scan of `src/backend/executor/execSRF.c`: no SPG divergence was
//! found (the executor already matches PG). These pins lock the subtle observable
//! contracts against regression — a strict SRF with a NULL argument is an empty
//! set, an empty range yields no rows, multiple target-list SRFs run in lockstep
//! (max rows, shorter ones NULL-padded), a non-SRF column repeats per SRF row,
//! and WITH ORDINALITY over an empty SRF yields no rows.

use spg_engine::{Engine, QueryResult};

fn count(e: &mut Engine, sql: &str) -> i64 {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::BigInt(n) => *n,
            spg_storage::Value::Int(n) => i64::from(*n),
            other => panic!("{sql}: {other:?}"),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Null => "NULL".to_string(),
            v => spg_engine::eval::value_to_text(v),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn strict_srf_null_arg_is_empty_set() {
    let mut e = Engine::new();
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM (SELECT generate_series(1, NULL::int) g) s"
        ),
        0
    );
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM (SELECT unnest(NULL::int[]) g) s"
        ),
        0
    );
    // Empty range → no rows.
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM (SELECT generate_series(5, 1) g) s"
        ),
        0
    );
}

#[test]
fn target_list_srfs_run_in_lockstep() {
    let mut e = Engine::new();
    // Shorter SRF is NULL-padded to the longer one's length.
    assert_eq!(
        text(
            &mut e,
            "SELECT string_agg(a||','||coalesce(b::text,'X'), '/') \
                      FROM (SELECT generate_series(1,3) a, generate_series(1,2) b) s"
        ),
        "1,1/2,2/3,X"
    );
    // An empty sibling does not shorten the output.
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM (SELECT generate_series(1,3) a, generate_series(1,0) b) s"
        ),
        3
    );
}

#[test]
fn scalar_column_repeats_per_srf_row() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT string_agg(x||','||g, '/') FROM (SELECT 9 x, generate_series(1,2) g) s"
        ),
        "9,1/9,2"
    );
}

#[test]
fn with_ordinality_over_empty_is_empty() {
    let mut e = Engine::new();
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM (SELECT * FROM generate_series(1,0) WITH ORDINALITY) s"
        ),
        0
    );
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM (SELECT * FROM unnest(ARRAY[]::int[]) WITH ORDINALITY) s"
        ),
        0
    );
    // Ordinality counts from 1 in output order.
    assert_eq!(
        text(
            &mut e,
            "SELECT string_agg(v||':'||n, '/') \
                      FROM (SELECT * FROM unnest(ARRAY['a','b']) WITH ORDINALITY AS t(v,n)) s"
        ),
        "a:1/b:2"
    );
}
