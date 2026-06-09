//! v7.17.0 Phase 2.5 — `COLLATE "case_insensitive"` (and MySQL
//! `_ci` collations) on TEXT columns. Pre-2.5 SPG parsed the
//! clause and dropped the name, so `WHERE name = 'foo'` never
//! matched stored `'Foo'` — a Tier-S silent failure. This pins
//! the new case-aware equality + the catalog round-trip.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn case_insensitive_equality_matches_mixed_case() {
    let mut e = Engine::new();
    e.execute(r#"CREATE TABLE t (id INT NOT NULL, name TEXT COLLATE "case_insensitive" NOT NULL)"#)
        .unwrap();
    e.execute("INSERT INTO t (id, name) VALUES (1, 'Alice'), (2, 'bob'), (3, 'CAROL')")
        .unwrap();
    let r = rows(e.execute("SELECT id FROM t WHERE name = 'alice'").unwrap());
    assert_eq!(r.len(), 1, "case_insensitive eq should match 'Alice'");
    assert_eq!(r[0][0], Value::Int(1));

    // Reverse: literal in mixed case, stored lowercase.
    let r = rows(e.execute("SELECT id FROM t WHERE name = 'BOB'").unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(2));

    // != is also collation-aware: 'alice' should NOT exclude the
    // 'Alice' row (it does collate-equal to 'alice').
    let r = rows(e.execute("SELECT id FROM t WHERE name != 'alice'").unwrap());
    let ids: Vec<i32> = r
        .iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            _ => unreachable!(),
        })
        .collect();
    assert!(!ids.contains(&1), "row 1 (Alice) should be excluded by != 'alice'");
    assert!(ids.contains(&2) && ids.contains(&3));
}

#[test]
fn binary_collation_still_byte_strict_by_default() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t (id, name) VALUES (1, 'Alice'), (2, 'alice')")
        .unwrap();
    let r = rows(e.execute("SELECT id FROM t WHERE name = 'alice'").unwrap());
    assert_eq!(r.len(), 1, "default binary collation only matches exact case");
    assert_eq!(r[0][0], Value::Int(2));
}

#[test]
fn mysql_ci_collation_classification() {
    // mysqldump emits `COLLATE utf8mb4_unicode_ci` (and friends).
    // The parser normalises to CaseInsensitive on the `_ci`
    // suffix.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT COLLATE utf8mb4_unicode_ci NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t (id, name) VALUES (1, 'Foo')")
        .unwrap();
    let r = rows(e.execute("SELECT id FROM t WHERE name = 'foo'").unwrap());
    assert_eq!(r.len(), 1, "MySQL `_ci` should fold case");
}

#[test]
fn unknown_collation_falls_back_to_binary() {
    // `pg_catalog.default` and `C` are the standard PG byte-wise
    // collations. mysqldump may also emit `*_bin` or `*_cs`. All
    // resolve to Binary, preserving the pre-2.5 byte compare.
    let mut e = Engine::new();
    e.execute(r#"CREATE TABLE t (id INT NOT NULL, name TEXT COLLATE "C" NOT NULL)"#)
        .unwrap();
    e.execute("INSERT INTO t (id, name) VALUES (1, 'Alice')")
        .unwrap();
    let r = rows(e.execute("SELECT id FROM t WHERE name = 'alice'").unwrap());
    assert!(r.is_empty(), "COLLATE 'C' is binary; mixed case shouldn't match");
}

#[test]
fn case_insensitive_persists_through_catalog_roundtrip() {
    // Verify the catalog appendix carries the collation across a
    // serialize → deserialize cycle.
    let mut e = Engine::new();
    e.execute(r#"CREATE TABLE t (id INT NOT NULL, name TEXT COLLATE "case_insensitive" NOT NULL)"#)
        .unwrap();
    e.execute("INSERT INTO t (id, name) VALUES (1, 'Bar')")
        .unwrap();
    let snapshot = e.catalog().serialize();
    let reloaded = spg_storage::Catalog::deserialize(&snapshot).expect("roundtrip");
    let mut e2 = Engine::restore(reloaded);
    let r = rows(e2.execute("SELECT id FROM t WHERE name = 'BAR'").unwrap());
    assert_eq!(r.len(), 1, "collation should survive snapshot");
}

#[test]
fn collation_field_on_column_schema() {
    let mut e = Engine::new();
    e.execute(r#"CREATE TABLE t (id INT NOT NULL, name TEXT COLLATE "case_insensitive" NOT NULL)"#)
        .unwrap();
    let table = e.catalog().get("t").expect("table");
    let col = table
        .schema()
        .columns
        .iter()
        .find(|c| c.name == "name")
        .expect("column");
    assert_eq!(col.collation, spg_storage::Collation::CaseInsensitive);
}
