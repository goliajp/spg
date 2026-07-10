//! INSERT ... DEFAULT VALUES + OVERRIDING {SYSTEM|USER} VALUE —
//! pg_dump / ORM INSERT-clause completions.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter().map(|row| row.values.clone()).collect()
}

fn as_i64(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn default_values_fills_serial_and_defaults() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d (id SERIAL, tag TEXT DEFAULT 'fresh', note TEXT)")
        .unwrap();
    e.execute("INSERT INTO d DEFAULT VALUES").unwrap();
    e.execute("INSERT INTO d DEFAULT VALUES").unwrap();
    let got = rows(&mut e, "SELECT id, tag, note FROM d ORDER BY id");
    assert_eq!(got.len(), 2);
    // Serial advances per row; declared default applies; bare
    // column is NULL.
    assert_eq!(as_i64(&got[0][0]), 1);
    assert_eq!(as_i64(&got[1][0]), 2);
    for r in &got {
        assert!(matches!(&r[1], spg_storage::Value::Text(s) if s == "fresh"));
        assert!(matches!(&r[2], spg_storage::Value::Null));
    }
}

#[test]
fn default_values_with_returning() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE r (id SERIAL, v INT DEFAULT 7)")
        .unwrap();
    let got = rows(&mut e, "INSERT INTO r DEFAULT VALUES RETURNING id, v");
    assert_eq!(got.len(), 1);
    assert_eq!(as_i64(&got[0][0]), 1);
    assert_eq!(as_i64(&got[0][1]), 7);
}

#[test]
fn overriding_system_value_accepts_explicit_id() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE o (id SERIAL, v INT)").unwrap();
    // pg_dump emits this for identity columns; the supplied id
    // must win.
    e.execute("INSERT INTO o (id, v) OVERRIDING SYSTEM VALUE VALUES (42, 1)")
        .unwrap();
    e.execute("INSERT INTO o (id, v) OVERRIDING USER VALUE VALUES (43, 2)")
        .unwrap();
    let got = rows(&mut e, "SELECT id FROM o ORDER BY id");
    assert_eq!(as_i64(&got[0][0]), 42);
    assert_eq!(as_i64(&got[1][0]), 43);
}
