//! Round 755 (F31-B6) — the parenless time-of-day keywords carry PG's
//! types, PG18-measured: `pg_typeof(current_time)` is `time with time
//! zone` (it answered `unknown` — the keyword produced untyped text,
//! and the value namer had no TimeTz arm), `pg_typeof(localtime)` is
//! `time without time zone` (it answered `timestamp without time
//! zone` — the keyword folded into current_timestamp).

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect::<Vec<_>>()
            .join("|"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn round755_time_keywords_carry_pg_types() {
    let mut e = Engine::new().with_clock(|| 1_700_000_000_000_000);
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_typeof(current_time), pg_typeof(localtime), \
             pg_typeof('12:30+09'::timetz)"
        ),
        "time with time zone|time without time zone|time with time zone"
    );
    // Value shape: HH:MM:SS(+offset) at precision 0 — the fraction is
    // truncated, the timetz keeps the session offset, the time does not.
    let v = one(&mut e, "SELECT current_time(0), localtime(0)");
    let (ct, lt) = v.split_once('|').unwrap();
    assert!(
        ct.len() == 11 && ct.ends_with("+00") && ct.as_bytes()[2] == b':',
        "current_time(0) must read HH:MM:SS+00, got {ct}"
    );
    assert!(
        lt.len() == 8 && lt.as_bytes()[2] == b':',
        "localtime(0) must read HH:MM:SS, got {lt}"
    );
    // Default precision keeps microseconds (PG18: 11:14:20.658952+00).
    // A whole-second clock trims its zero fraction (PG does the same),
    // so the fraction probe uses a clock with microseconds on it.
    let mut ef = Engine::new().with_clock(|| 1_700_000_000_123_456);
    let full = one(&mut ef, "SELECT current_time::text");
    assert_eq!(
        full, "22:13:20.123456+00",
        "default current_time keeps the fraction and offset"
    );
}
