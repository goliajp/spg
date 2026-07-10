//! v7.38 (read01) — EXTRACT / date_part from a TIME value. Only the
//! time-of-day fields apply and, like PG, they all return NUMERIC; a date
//! field is rejected with PG's exact wording. Every value is from live PG18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => panic!("{sql}: expected Text, got {other:?}"),
        },
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

#[test]
fn extract_time_fields() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT (extract(hour from TIME '14:30:45'))::text"),
        "14"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT (extract(minute from TIME '14:30:45'))::text"
        ),
        "30"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT (extract(second from TIME '14:30:45.123456'))::text"
        ),
        "45.123456"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT (extract(millisecond from TIME '14:30:45.5'))::text"
        ),
        "45500.000"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT (extract(microseconds from TIME '14:30:45.123456'))::text"
        ),
        "45123456"
    );
    assert_eq!(
        one(&mut e, "SELECT (extract(epoch from TIME '14:30:45'))::text"),
        "52245.000000"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT (extract(epoch from TIME '14:30:45.5'))::text"
        ),
        "52245.500000"
    );
    assert_eq!(
        one(&mut e, "SELECT (date_part('hour', TIME '09:15:30'))::text"),
        "9"
    );
    // All results are numeric, like PG.
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_typeof(extract(hour from TIME '14:30'))::text"
        ),
        "numeric"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_typeof(extract(second from TIME '14:30:45.5'))::text"
        ),
        "numeric"
    );
}

#[test]
fn extract_time_rejects_date_fields() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT extract(year from TIME '14:30')").is_err());
    assert!(e.execute("SELECT extract(dow from TIME '14:30')").is_err());
    assert!(e.execute("SELECT extract(day from TIME '14:30')").is_err());
}
