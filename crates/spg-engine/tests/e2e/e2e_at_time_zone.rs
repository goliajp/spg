//! AT TIME ZONE + timezone(zone, ts) — offset zones shift for
//! real; named zones error honestly (no tzdata).

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn utc_is_identity_offsets_shift() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT date_format(TIMESTAMP '2024-01-01 12:00:00' AT TIME ZONE 'UTC', \
             '%Y-%m-%d %H:%i:%s')"
        ),
        "2024-01-01 12:00:00"
    );
    // UTC noon displayed in +09:00 is 21:00.
    assert_eq!(
        text(
            &mut e,
            "SELECT date_format(TIMESTAMP '2024-01-01 12:00:00' AT TIME ZONE '+09:00', \
             '%Y-%m-%d %H:%i:%s')"
        ),
        "2024-01-01 21:00:00"
    );
    // Negative offset crosses the date boundary.
    assert_eq!(
        text(
            &mut e,
            "SELECT date_format(TIMESTAMP '2024-01-01 03:00:00' AT TIME ZONE '-05:00', \
             '%Y-%m-%d %H:%i:%s')"
        ),
        "2023-12-31 22:00:00"
    );
}

#[test]
fn function_form_and_named_zone_error() {
    let mut e = Engine::new();
    // PG's timezone(zone, ts) is the same operation.
    assert_eq!(
        text(
            &mut e,
            "SELECT date_format(timezone('+01:00', TIMESTAMP '2024-06-01 00:30:00'), \
             '%Y-%m-%d %H:%i:%s')"
        ),
        "2024-06-01 01:30:00"
    );
    let err = e
        .execute("SELECT TIMESTAMP '2024-01-01 00:00:00' AT TIME ZONE 'Asia/Tokyo'")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("tzdata"), "unexpected error: {msg}");
}
