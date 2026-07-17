//! v7.39 (read01 utils/adt, round 32) — timestamp.c part 1: the
//! overflow-crash fixes (parse + arithmetic + date-sentinel cast),
//! infinity semantics (isfinite / absorption / diff rejection),
//! date_trunc(interval), make_timestamptz. Byte-locked vs PG18.

use spg_engine::{Engine, QueryResult};

fn row_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

#[test]
fn overflow_is_an_error_not_a_crash() {
    let mut e = Engine::new();
    // Both once aborted the process (debug multiply-overflow): a
    // beyond-window literal and a shifted-past-window instant.
    assert!(
        !err_of(
            &mut e,
            "SELECT timestamp '294276-12-31 23:59:59' + interval '100 years'"
        )
        .is_empty()
    );
    assert!(
        err_of(
            &mut e,
            "SELECT timestamp '4714-01-01 00:00:00 BC' - interval '100 years'"
        )
        .contains("timestamp out of range")
    );
    // The date-infinity sentinel casts to the timestamp sentinel
    // (used to overflow in the multiply).
    assert_eq!(
        row_of(
            &mut e,
            "SELECT '-infinity'::date::timestamp, 'infinity'::date"
        ),
        vec!["-infinity", "infinity"]
    );
}

#[test]
fn infinity_semantics() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT isfinite('infinity'::timestamp), isfinite(timestamp '2024-01-01'), \
             isfinite('infinity'::date)"
        ),
        vec!["false", "true", "false"]
    );
    // An infinite timestamp absorbs any finite interval.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT 'infinity'::timestamp + interval '1 day', \
             '-infinity'::timestamp + interval '1 day'"
        ),
        vec!["infinity", "-infinity"]
    );
    assert!(
        err_of(
            &mut e,
            "SELECT 'infinity'::timestamp - 'infinity'::timestamp"
        )
        .contains("interval out of range")
    );
}

#[test]
fn interval_trunc_and_make_timestamptz() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT date_trunc('hour', interval '2 days 13:47:12'), \
             date_trunc('month', interval '1 year 2 mons 3 days 04:05:06')"
        ),
        vec!["2 days 13:00:00", "1 year 2 mons"]
    );
    // Engine-level rendering has no wire type witness; the +00 offset
    // suffix rides the describe(make_timestamptz)=timestamptz witness at
    // the wire layer (differential-verified there).
    assert_eq!(
        row_of(
            &mut e,
            "SELECT make_timestamp(2024, 3, 15, 12, 30, 45.5), \
             make_timestamptz(2024, 3, 15, 12, 30, 45.5)"
        ),
        vec!["2024-03-15 12:30:45.5", "2024-03-15 12:30:45.5"]
    );
}
