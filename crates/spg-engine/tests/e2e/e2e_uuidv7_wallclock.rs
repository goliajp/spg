//! v7.38 (read01 P6.08) — uuidv7() uses the host wall clock for its 48-bit
//! millisecond prefix (time-ordered) and a process-wide monotonic counter so
//! successive values strictly sort. With no host clock it falls back to a
//! deterministic anchor but stays monotonic.

use spg_engine::{Engine, QueryResult};

fn fixed_clock() -> i64 {
    // 2024-06-01 00:00:00 UTC, in microseconds.
    1_717_200_000_000_000
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => panic!("expected text, got {v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn uuidv7_prefix_is_host_wall_clock() {
    let mut e = Engine::new().with_clock(fixed_clock);
    // The 48-bit prefix (12 hex chars) equals the injected ms since epoch:
    // 1_717_200_000_000 ms = 0x018fd1189400.
    let hex = text(&mut e, "SELECT substring(replace(uuidv7()::text,'-',''),1,12)");
    assert_eq!(hex, "018fd1189400", "prefix should be the injected wall clock");
    // Version nibble is 7.
    assert_eq!(text(&mut e, "SELECT substring(uuidv7()::text,15,1)"), "7");
}

#[test]
fn uuidv7_is_monotonic_within_a_millisecond() {
    // Same fixed ms for every call → ordering must come from the counter.
    let mut e = Engine::new().with_clock(fixed_clock);
    let mut prev = String::new();
    for _ in 0..500 {
        let u = text(&mut e, "SELECT uuidv7()::text");
        assert!(u > prev, "uuidv7 must strictly increase: {prev} !< {u}");
        prev = u;
    }
}

#[test]
fn uuidv7_monotonic_without_a_clock() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT (uuidv7() < uuidv7())::text"), "true");
}
