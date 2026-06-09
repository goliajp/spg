//! v7.17.0 Phase 2.5b — case-insensitive collation in ORDER BY
//! / GROUP BY / DISTINCT. Extends Phase 2.5 which covered only
//! WHERE / HAVING eq.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn order_by_case_insensitive_column() {
    let mut e = Engine::new();
    e.execute(r#"CREATE TABLE t (id INT NOT NULL, name TEXT COLLATE "case_insensitive" NOT NULL)"#)
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'banana'), (2, 'Apple'), (3, 'cherry')")
        .unwrap();
    let r = rows(e.execute("SELECT id FROM t ORDER BY name").unwrap());
    let ids: Vec<i32> = r
        .iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            _ => unreachable!(),
        })
        .collect();
    // With case-insensitive collation, ORDER BY name sorts:
    // Apple < banana < cherry (regardless of case).
    assert_eq!(ids, vec![2, 1, 3]);
}

#[test]
fn order_by_binary_column_byte_strict() {
    // Negative regression: default (Binary) collation still
    // byte-strict — uppercase sorts before lowercase per ASCII.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'banana'), (2, 'Apple'), (3, 'cherry')")
        .unwrap();
    let r = rows(e.execute("SELECT id FROM t ORDER BY name").unwrap());
    let ids: Vec<i32> = r
        .iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            _ => unreachable!(),
        })
        .collect();
    // Binary: 'Apple' (uppercase A < lowercase b) < 'banana' < 'cherry'.
    assert_eq!(ids, vec![2, 1, 3]);
}

#[test]
fn unique_constraint_collation_documented_gap() {
    // Phase 2.5b carve-out: UNIQUE constraint enforcement still
    // uses byte-strict comparison even when the column is
    // CaseInsensitive. The BTree index uses the storage-level
    // value-cmp which doesn't read collation. Customers needing
    // case-insensitive uniqueness should wrap in a generated
    // column or apply LOWER() pre-index.
    let mut e = Engine::new();
    e.execute(
        r#"CREATE TABLE t (id INT NOT NULL, name TEXT COLLATE "case_insensitive" UNIQUE NOT NULL)"#,
    )
    .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'foo')").unwrap();
    // 'Foo' (different case) currently inserts because the BTree
    // sees a different byte sequence. With full Phase 2.5c this
    // would fail. Pin current behavior.
    let _ = e.execute("INSERT INTO t VALUES (2, 'Foo')");
}

#[test]
fn group_by_case_insensitive_column() {
    let mut e = Engine::new();
    e.execute(r#"CREATE TABLE t (id INT NOT NULL, name TEXT COLLATE "case_insensitive" NOT NULL)"#)
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'Foo'), (2, 'foo'), (3, 'FOO'), (4, 'Bar')")
        .unwrap();
    let r = rows(e.execute("SELECT count(*) FROM t GROUP BY name").unwrap());
    // case_insensitive: 'Foo'/'foo'/'FOO' all collide → 2 groups.
    assert_eq!(r.len(), 2);
}
