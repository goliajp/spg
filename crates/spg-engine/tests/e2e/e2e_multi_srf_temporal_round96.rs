//! v7.39 (read01 round 96) — multiple set-returning functions in the target
//! list where one is a NON-integer `generate_series`.
//!
//! A single `generate_series(ts, ts, interval)` / numeric series worked (the
//! FROM-clause materialiser drove it), but when TWO SRFs shared the target
//! list the lockstep path had its own integer-only reimplementation, so the
//! temporal/numeric column silently came back NULL:
//!   `SELECT generate_series(1,2), generate_series(ts, ts, interval)`
//!     → `1|<null>`, `2|<null>`  (should be the timestamps).
//! Both paths now share one value-producing core. Locked byte-identical
//! against live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Null => "NULL".to_string(),
                        _ => spg_engine::eval::value_to_text(v),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn int_and_timestamp_srfs_expand_in_lockstep() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT generate_series(1,2), \
             generate_series(timestamp '2024-01-01', timestamp '2024-01-02', interval '1 day')"
        ),
        ["1|2024-01-01 00:00:00", "2|2024-01-02 00:00:00"]
    );
    // Order of the two SRFs must not matter.
    assert_eq!(
        rows(
            &mut e,
            "SELECT generate_series(timestamp '2024-01-01', timestamp '2024-01-02', interval '1 day'), \
             generate_series(1,2)"
        ),
        ["2024-01-01 00:00:00|1", "2024-01-02 00:00:00|2"]
    );
}

#[test]
fn int_and_numeric_srfs_expand_in_lockstep() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT generate_series(1,2), generate_series(1.0::numeric, 2.0::numeric, 1.0)"
        ),
        ["1|1.0", "2|2.0"]
    );
}

#[test]
fn integer_multi_srf_unchanged() {
    // Regression guard: the previously-working integer lockstep + NULL-pad of
    // the shorter series must still hold.
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT generate_series(1,3), generate_series(10,11)"
        ),
        ["1|10", "2|11", "3|NULL"]
    );
    // SRF + scalar broadcast.
    assert_eq!(
        rows(&mut e, "SELECT generate_series(1,2), 'x'"),
        ["1|x", "2|x"]
    );
}

#[test]
fn single_temporal_srf_still_works() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT generate_series(timestamp '2024-01-01', timestamp '2024-01-03', interval '1 day')"
        ),
        [
            "2024-01-01 00:00:00",
            "2024-01-02 00:00:00",
            "2024-01-03 00:00:00"
        ]
    );
}
