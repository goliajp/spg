//! v7.39 (read01 round 114) — `date_trunc` / `date_bin` over a `date` argument
//! resolve to PG's timestamptz overload.
//!
//! `date_trunc('quarter', date '2024-05-15')` returned a plain timestamp
//! (`2024-04-01 00:00:00`), but PG resolves a `date` argument to the
//! *timestamptz* overload (timestamptz is date's preferred implicit cast), so
//! the result is `timestamp with time zone` and keeps its `+00`. `date_bin`
//! is the same, and additionally rejected a date argument at runtime. Both now
//! infer timestamptz for a date input and accept it. A bare `timestamp`
//! argument still yields a plain timestamp. Locked byte-identical against PG
//! 18.4 (UTC session).

use spg_engine::{Engine, QueryResult};

fn render(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Null => "NULL".to_string(),
            v => spg_engine::eval::value_to_text(v),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn date_trunc_over_date_is_timestamptz() {
    let mut e = Engine::new();
    assert_eq!(
        render(&mut e, "SELECT date_trunc('quarter', date '2024-05-15')::text"),
        "2024-04-01 00:00:00+00"
    );
    assert_eq!(
        render(&mut e, "SELECT date_trunc('day', date '2024-05-15')::text"),
        "2024-05-15 00:00:00+00"
    );
    assert_eq!(
        render(&mut e, "SELECT pg_typeof(date_trunc('quarter', date '2024-05-15'))::text"),
        "timestamp with time zone"
    );
}

#[test]
fn date_trunc_over_timestamp_stays_timestamp() {
    let mut e = Engine::new();
    // Regression: a bare timestamp argument keeps the plain timestamp overload.
    assert_eq!(
        render(&mut e, "SELECT date_trunc('month', timestamp '2024-05-15 12:00')::text"),
        "2024-05-01 00:00:00"
    );
    assert_eq!(
        render(&mut e, "SELECT pg_typeof(date_trunc('month', timestamp '2024-05-15 12:00'))::text"),
        "timestamp without time zone"
    );
}

#[test]
fn date_bin_over_date_is_timestamptz() {
    let mut e = Engine::new();
    // date_bin previously errored on a date argument; now it resolves to the
    // timestamptz overload like date_trunc.
    assert_eq!(
        render(&mut e, "SELECT date_bin('1 day', date '2024-05-15', date '2024-01-01')::text"),
        "2024-05-15 00:00:00+00"
    );
    assert_eq!(
        render(&mut e, "SELECT pg_typeof(date_bin('1 day', date '2024-05-15', date '2024-01-01'))::text"),
        "timestamp with time zone"
    );
}
