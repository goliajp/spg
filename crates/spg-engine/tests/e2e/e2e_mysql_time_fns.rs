//! v7.37.17 (17.6 siblings) — MySQL time-of-day arithmetic:
//! time_to_sec / sec_to_time / maketime / addtime / subtime /
//! timediff / microsecond.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn time_to_sec_doc_vectors() {
    let mut e = Engine::new();
    // MySQL doc vectors: TIME_TO_SEC('22:23:00') = 80580,
    // TIME_TO_SEC('00:39:38') = 2378.
    assert!(matches!(
        first(&mut e, "SELECT time_to_sec('22:23:00')"),
        spg_storage::Value::BigInt(80_580)
    ));
    assert!(matches!(
        first(&mut e, "SELECT time_to_sec('00:39:38')"),
        spg_storage::Value::BigInt(2_378)
    ));
}

#[test]
fn sec_to_time_roundtrip() {
    let mut e = Engine::new();
    // MySQL doc vector: SEC_TO_TIME(2378) = '00:39:38'.
    assert_eq!(text(&first(&mut e, "SELECT sec_to_time(2378)")), "00:39:38");
    // Hours beyond 24 render like MySQL TIME.
    assert_eq!(
        text(&first(&mut e, "SELECT sec_to_time(90000)")),
        "25:00:00"
    );
    // Negative time keeps its sign.
    assert_eq!(
        text(&first(&mut e, "SELECT sec_to_time(-90)")),
        "-00:01:30"
    );
}

#[test]
fn maketime_builds() {
    let mut e = Engine::new();
    // MySQL doc vector: MAKETIME(12, 15, 30) = '12:15:30'.
    assert_eq!(
        text(&first(&mut e, "SELECT maketime(12, 15, 30)")),
        "12:15:30"
    );
    // Minute out of range → NULL (MySQL semantics).
    assert!(matches!(
        first(&mut e, "SELECT maketime(12, 60, 30)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn addtime_subtime_timediff() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT addtime('01:00:00', '00:30:00')")),
        "01:30:00"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT subtime('01:00:00', '00:30:00')")),
        "00:30:00"
    );
    // MySQL doc vector: TIMEDIFF('08:08:00', '05:05:00') gives
    // '03:03:00'.
    assert_eq!(
        text(&first(&mut e, "SELECT timediff('08:08:00', '05:05:00')")),
        "03:03:00"
    );
    // Negative difference keeps the sign.
    assert_eq!(
        text(&first(&mut e, "SELECT timediff('05:05:00', '08:08:00')")),
        "-03:03:00"
    );
    // Fractional seconds carry through.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT addtime('01:00:00.500000', '00:00:00.700000')"
        )),
        "01:00:01.200000"
    );
}

#[test]
fn microsecond_extracts() {
    let mut e = Engine::new();
    // MySQL doc vector: MICROSECOND('12:00:00.123456') = 123456.
    assert!(matches!(
        first(&mut e, "SELECT microsecond('12:00:00.123456')"),
        spg_storage::Value::Int(123_456)
    ));
    assert!(matches!(
        first(&mut e, "SELECT microsecond('12:00:00')"),
        spg_storage::Value::Int(0)
    ));
}

#[test]
fn mysql_time_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "time_to_sec(NULL::text)",
        "sec_to_time(NULL::int)",
        "maketime(NULL::int, 1, 1)",
        "addtime(NULL::text, '00:00:01')",
        "timediff('01:00:00', NULL::text)",
        "microsecond(NULL::text)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
