//! v7.38 (read01 P6.57) — CREATE UNLOGGED TABLE creates a real, usable table
//! (unlike TEMP, which is a no-op). SPG doesn't yet skip WAL for it, but the
//! table behaves normally so dumps / apps that declare UNLOGGED tables work.

use spg_engine::{Engine, QueryResult};

#[test]
fn unlogged_table_is_a_real_table() {
    let mut e = Engine::new();
    e.execute("CREATE UNLOGGED TABLE u (id INT, name TEXT)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 'a'), (2, 'b')")
        .unwrap();
    match e.execute("SELECT count(*) FROM u").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], spg_storage::Value::BigInt(2))
        }
        _ => panic!("expected rows"),
    }
    match e.execute("SELECT name FROM u WHERE id = 2").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], spg_storage::Value::text("b"))
        }
        _ => panic!("expected rows"),
    }
}
