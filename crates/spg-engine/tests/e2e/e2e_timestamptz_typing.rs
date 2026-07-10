//! v7.38 (T-tstz Phase 1) — `timestamptz` is a distinct type on every surface
//! that reads the static `DataType`, even though the runtime value is the same
//! UTC-microsecond `Value::Timestamp` as `timestamp`. Oracle: live PG 18.4 at
//! `TimeZone='UTC'`.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Text(t) => t.to_string(),
                        spg_storage::Value::Null => "<NULL>".to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect()
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn information_schema_udt_name_is_the_pg_internal_typname() {
    // These four all fell into a `text` catch-all and mis-reported themselves.
    // PG18.4 udt_name for the same columns: timestamptz / timestamp / time /
    // interval / numeric.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ut(a timestamptz, b timestamp, c time, d interval, e numeric(5,2))")
        .unwrap();
    let got = rows(
        &mut e,
        "SELECT column_name, udt_name FROM information_schema.columns WHERE table_name = 'ut'",
    );
    let pairs: Vec<(String, String)> = got
        .into_iter()
        .map(|r| (r[0].clone(), r[1].clone()))
        .collect();
    for (col, want) in [
        ("a", "timestamptz"),
        ("b", "timestamp"),
        ("c", "time"),
        ("d", "interval"),
        ("e", "numeric"),
    ] {
        let got = pairs
            .iter()
            .find(|(c, _)| c == col)
            .unwrap_or_else(|| panic!("column {col} missing"));
        assert_eq!(got.1, want, "udt_name for column {col}");
    }
}

#[test]
fn copy_renders_timestamptz_with_its_offset() {
    // PG's COPY emits `2024-01-15 10:30:00+00` for timestamptz and
    // `2024-01-15 10:30:00` for timestamp. The offset used to be dropped for
    // both, because the COPY cell renderer never saw the column's type.
    let mut e = Engine::new();
    e.execute("CREATE TABLE cz(a timestamptz, b timestamp)")
        .unwrap();
    e.execute("INSERT INTO cz VALUES ('2024-01-15 10:30:00+00', '2024-01-15 10:30:00')")
        .unwrap();
    let got = rows(&mut e, "COPY cz TO STDOUT");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][0], "2024-01-15 10:30:00+00\t2024-01-15 10:30:00");
}

#[test]
fn timestamptz_input_is_normalized_to_utc() {
    // The value itself was already right — a non-UTC literal folds to the same
    // instant. Locking it so the rendering work above can't drift it.
    let mut e = Engine::new();
    e.execute("CREATE TABLE cz2(a timestamptz)").unwrap();
    e.execute("INSERT INTO cz2 VALUES ('2024-01-15 10:30:00+09')")
        .unwrap();
    let got = rows(&mut e, "COPY cz2 TO STDOUT");
    assert_eq!(got[0][0], "2024-01-15 01:30:00+00");
}

#[test]
fn pg_typeof_uses_the_static_type_not_the_runtime_value() {
    // The runtime value is a tz-less Value::Timestamp, so pg_typeof must read
    // the static expression type. Oracle: PG18.4.
    let mut e = Engine::new();
    e.execute("CREATE TABLE tzt(a timestamptz, b timestamp)")
        .unwrap();
    e.execute("INSERT INTO tzt VALUES ('2024-01-15 10:30:00+00', '2024-01-15 10:30:00')")
        .unwrap();
    let t = |e: &mut Engine, sql: &str| rows(e, sql)[0][0].clone();
    assert_eq!(
        t(&mut e, "SELECT pg_typeof(a) FROM tzt"),
        "timestamp with time zone"
    );
    assert_eq!(
        t(&mut e, "SELECT pg_typeof(b) FROM tzt"),
        "timestamp without time zone"
    );
}

#[test]
fn clock_functions_carry_the_right_tz_type() {
    // now()/current_timestamp/clock_timestamp are timestamptz; localtimestamp
    // is not. Oracle: PG18.4.
    let mut e = Engine::new().with_clock(|| 1_705_314_600_000_000);
    let t = |e: &mut Engine, sql: &str| rows(e, sql)[0][0].clone();
    assert_eq!(
        t(&mut e, "SELECT pg_typeof(now())"),
        "timestamp with time zone"
    );
    assert_eq!(
        t(&mut e, "SELECT pg_typeof(current_timestamp)"),
        "timestamp with time zone"
    );
    assert_eq!(
        t(&mut e, "SELECT pg_typeof(clock_timestamp())"),
        "timestamp with time zone"
    );
    assert_eq!(
        t(&mut e, "SELECT pg_typeof(localtimestamp)"),
        "timestamp without time zone"
    );
}

#[test]
fn cast_to_text_renders_offset_for_timestamptz() {
    // `<timestamptz>::text` carries the +00 offset; `<timestamp>::text` does not.
    let mut e = Engine::new();
    e.execute("CREATE TABLE tzc(a timestamptz, b timestamp)")
        .unwrap();
    e.execute("INSERT INTO tzc VALUES ('2024-01-15 10:30:00+00', '2024-01-15 10:30:00')")
        .unwrap();
    let t = |e: &mut Engine, sql: &str| rows(e, sql)[0][0].clone();
    assert_eq!(
        t(&mut e, "SELECT a::text FROM tzc"),
        "2024-01-15 10:30:00+00"
    );
    assert_eq!(t(&mut e, "SELECT b::text FROM tzc"), "2024-01-15 10:30:00");
    // A non-UTC literal folds to UTC then renders +00.
    assert_eq!(
        t(&mut e, "SELECT '2024-01-15 10:30:00+09'::timestamptz::text"),
        "2024-01-15 01:30:00+00"
    );
}

#[test]
fn union_of_timestamptz_and_timestamp_is_timestamptz() {
    // PG18.4: any timestamptz branch makes the common type timestamptz, and the
    // column then renders with offsets.
    let mut e = Engine::new();
    let r = e
        .execute(
            "SELECT '2024-01-01 00:00:00+00'::timestamptz AS x \
             UNION ALL SELECT '2024-01-02 00:00:00'::timestamp",
        )
        .unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("rows")
    };
    assert_eq!(columns[0].ty, spg_storage::DataType::Timestamptz);
}

#[test]
fn timestamptz_text_honours_session_timezone() {
    // v7.38 (T-tstz Phase 2) — `<timestamptz>::text` renders in the session
    // TimeZone. A fixed offset / abbreviation shifts the wall clock and shows
    // the matching suffix; a named IANA zone (no tzdata) falls back to +00
    // rather than erroring. AT TIME ZONE keeps its own (PG-quirky) sign
    // convention and is checked here too so the shared parser can't drift it.
    // Oracle: live PG 18.4.
    let mut e = Engine::new();
    let t = |e: &mut Engine, sql: &str| rows(e, sql)[0][0].clone();

    e.execute("SET TimeZone='JST'").unwrap();
    assert_eq!(
        t(&mut e, "SELECT '2024-01-15 10:30:00+00'::timestamptz::text"),
        "2024-01-15 19:30:00+09"
    );
    e.execute("SET TimeZone='EST'").unwrap();
    assert_eq!(
        t(&mut e, "SELECT '2024-01-15 10:30:00+00'::timestamptz::text"),
        "2024-01-15 05:30:00-05"
    );
    e.execute("SET TimeZone='+05:30'").unwrap();
    assert_eq!(
        t(&mut e, "SELECT '2024-01-15 10:30:00+00'::timestamptz::text"),
        "2024-01-15 16:00:00+05:30"
    );
    // Named IANA zone → fallback to +00 (no tzdata), never an error.
    e.execute("SET TimeZone='America/New_York'").unwrap();
    assert_eq!(
        t(&mut e, "SELECT '2024-01-15 10:30:00+00'::timestamptz::text"),
        "2024-01-15 10:30:00+00"
    );
    // Back to UTC restores the plain +00 form.
    e.execute("SET TimeZone='UTC'").unwrap();
    assert_eq!(
        t(&mut e, "SELECT '2024-01-15 10:30:00+00'::timestamptz::text"),
        "2024-01-15 10:30:00+00"
    );

    // AT TIME ZONE unchanged: numeric adds, named subtracts (PG's quirk).
    assert_eq!(
        t(
            &mut e,
            "SELECT ('2024-01-15 10:30:00'::timestamp AT TIME ZONE '+09')::text"
        ),
        "2024-01-15 19:30:00"
    );
    assert_eq!(
        t(
            &mut e,
            "SELECT ('2024-01-15 10:30:00'::timestamp AT TIME ZONE 'JST')::text"
        ),
        "2024-01-15 01:30:00"
    );
}
