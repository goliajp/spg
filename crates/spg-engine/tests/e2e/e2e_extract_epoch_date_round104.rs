//! v7.39 (read01 round 104) — `extract(epoch from DATE)` scale.
//!
//! A DATE has no sub-day precision, so PG returns its epoch as a whole-second
//! integer numeric (`1704067200`, scale 0). SPG returned scale 6
//! (`1704067200.000000`) — it ran every DATE/TIMESTAMP epoch through the same
//! microsecond path. Now a DATE input yields scale 0 while TIMESTAMP / TIME /
//! INTERVAL keep scale 6. Locked byte-identical against live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn epoch_from_date_is_scale_zero() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT extract(epoch from date '2024-01-01')::text"), "1704067200");
    assert_eq!(text(&mut e, "SELECT extract(epoch from date '1970-01-01')::text"), "0");
    assert_eq!(text(&mut e, "SELECT extract(epoch from date '1969-12-31')::text"), "-86400");
    assert_eq!(text(&mut e, "SELECT date_part('epoch', date '2024-01-01')::text"), "1704067200");
}

#[test]
fn epoch_from_timestamp_time_interval_keep_scale_six() {
    // Regression guard: sub-second-capable types keep the microsecond scale.
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT extract(epoch from timestamp '2024-01-01 00:00:00')::text"),
        "1704067200.000000"
    );
    assert_eq!(
        text(&mut e, "SELECT extract(epoch from timestamp '2024-01-01 00:00:00.5')::text"),
        "1704067200.500000"
    );
    assert_eq!(text(&mut e, "SELECT extract(epoch from time '01:00:00')::text"), "3600.000000");
    assert_eq!(text(&mut e, "SELECT extract(epoch from interval '1 day')::text"), "86400.000000");
}
