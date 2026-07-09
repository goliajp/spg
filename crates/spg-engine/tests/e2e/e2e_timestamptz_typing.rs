//! v7.38 (T-tstz Phase 1) — `timestamptz` is a distinct type on every surface
//! that reads the static `DataType`, even though the runtime value is the same
//! UTC-microsecond `Value::Timestamp` as `timestamp`. Oracle: live PG 18.4 at
//! `TimeZone='UTC'`.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
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
    e.execute("CREATE TABLE cz(a timestamptz, b timestamp)").unwrap();
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
    e.execute("INSERT INTO cz2 VALUES ('2024-01-15 10:30:00+09')").unwrap();
    let got = rows(&mut e, "COPY cz2 TO STDOUT");
    assert_eq!(got[0][0], "2024-01-15 01:30:00+00");
}
