//! GROUP BY 1 positional + DESCRIBE t + MySQL LIMIT offset,count.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter().map(|row| row.values.to_vec()).collect()
}

fn as_i64(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn group_by_position() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE gp (k TEXT, v INT)").unwrap();
    e.execute("INSERT INTO gp VALUES ('a', 1), ('a', 2), ('b', 5)")
        .unwrap();
    let got = rows(&mut e, "SELECT k, sum(v) FROM gp GROUP BY 1 ORDER BY 1");
    assert_eq!(got.len(), 2);
    assert_eq!(as_i64(&got[0][1]), 3);
    assert_eq!(as_i64(&got[1][1]), 5);
    // Out-of-range errors.
    assert!(e.execute("SELECT k FROM gp GROUP BY 3").is_err());
}

#[test]
fn describe_routes_to_show_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE dt (id INT PRIMARY KEY, name TEXT)")
        .unwrap();
    let a = rows(&mut e, "DESCRIBE dt");
    let b = rows(&mut e, "DESC dt");
    let c = rows(&mut e, "SHOW COLUMNS FROM dt");
    assert_eq!(a.len(), 2);
    assert_eq!(a, b);
    assert_eq!(a, c);
}

#[test]
fn mysql_limit_offset_count() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ml (v INT)").unwrap();
    e.execute("INSERT INTO ml VALUES (1), (2), (3), (4)")
        .unwrap();
    // LIMIT 1, 2 — skip 1, take 2.
    let got = rows(&mut e, "SELECT v FROM ml ORDER BY v LIMIT 1, 2");
    assert_eq!(got.len(), 2);
    assert_eq!(as_i64(&got[0][0]), 2);
    assert_eq!(as_i64(&got[1][0]), 3);
}
