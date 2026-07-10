//! MySQL upserts — INSERT ... ON DUPLICATE KEY UPDATE + REPLACE
//! INTO, lowered onto the ON CONFLICT machinery.

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
fn on_duplicate_key_update_basic() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE du (id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO du VALUES (1, 10)").unwrap();
    // New key inserts; duplicate key updates.
    e.execute("INSERT INTO du VALUES (2, 20) ON DUPLICATE KEY UPDATE v = 99")
        .unwrap();
    e.execute("INSERT INTO du VALUES (1, 5) ON DUPLICATE KEY UPDATE v = v + 100")
        .unwrap();
    let got = rows(&mut e, "SELECT v FROM du ORDER BY id");
    assert_eq!(as_i64(&got[0][0]), 110);
    assert_eq!(as_i64(&got[1][0]), 20);
}

#[test]
fn on_duplicate_key_values_reads_incoming() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE dv (id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO dv VALUES (1, 10)").unwrap();
    // MySQL VALUES(col) is the incoming row's value.
    e.execute("INSERT INTO dv VALUES (1, 42) ON DUPLICATE KEY UPDATE v = VALUES(v) * 2")
        .unwrap();
    let got = rows(&mut e, "SELECT v FROM dv");
    assert_eq!(as_i64(&got[0][0]), 84);
}

#[test]
fn replace_into_swaps_whole_row() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE rp (id INT PRIMARY KEY, a INT, b TEXT)")
        .unwrap();
    e.execute("INSERT INTO rp VALUES (1, 10, 'old')").unwrap();
    // Existing key: the whole row is replaced (delete+insert
    // semantics), not merged.
    e.execute("REPLACE INTO rp VALUES (1, 20, 'new')").unwrap();
    let got = rows(&mut e, "SELECT a, b FROM rp");
    assert_eq!(got.len(), 1);
    assert_eq!(as_i64(&got[0][0]), 20);
    assert!(matches!(&got[0][1], spg_storage::Value::Text(s) if s == "new"));
    // Fresh key: plain insert.
    e.execute("REPLACE INTO rp VALUES (2, 1, 'x')").unwrap();
    assert_eq!(rows(&mut e, "SELECT id FROM rp").len(), 2);
}
