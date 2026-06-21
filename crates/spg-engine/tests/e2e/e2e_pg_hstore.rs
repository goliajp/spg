//! v7.17.0 Phase 3.P0-39 — PG `hstore` type.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/hstore.html
//!
//! Surface:
//!   * `CREATE TABLE … (col HSTORE)` — DDL accept.
//!   * Text input: `'a=>1, b=>2'`, `'a=>"x y", b=>NULL'`.
//!   * Display: PG canonical `"a"=>"1", "b"=>"2"` (every key
//!     and non-NULL value is double-quoted in PG output).
//!   * Catalog snapshot survival.
//!   * NULL preserved.
//!
//! Invariants pinned:
//!   * Storage: `Vec<(String, Option<String>)>` — keys are
//!     unique; insertion preserves first occurrence.
//!   * Duplicate keys → last-write-wins (matches PG).
//!   * NULL value → stored as `None`; renders as `=>NULL`
//!     (no quotes on NULL token).
//!   * Empty input `''` → empty map.
//!
//! v7.17.0 ships parse + display + storage + DDL accept; hstore
//! operators (`->`, `?`, `?|`, `?&`, `@>`, `<@`, `||`) land in a
//! follow-up.

use spg_engine::{Engine, QueryResult};
use spg_storage::{DataType, Value};

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        let r = eng
            .execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
        assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql:?}");
    }
    eng
}

fn select(eng: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn ddl_accepts_hstore_keyword() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, props HSTORE)")
        .unwrap();
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let schema = cat.get("t").unwrap().schema();
    assert!(matches!(schema.columns[1].ty, DataType::Hstore));
}

#[test]
fn insert_simple_pair_round_trips() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, props HSTORE)",
        "INSERT INTO t VALUES (1, 'a=>1, b=>2')",
    ]);
    let rows = select(&mut eng, "SELECT props FROM t");
    let Value::Hstore(pairs) = &rows[0][0] else {
        panic!("expected Hstore, got {:?}", rows[0][0]);
    };
    assert_eq!(pairs.len(), 2);
    assert_eq!(pairs[0], ("a".to_string(), Some("1".to_string())));
    assert_eq!(pairs[1], ("b".to_string(), Some("2".to_string())));
}

#[test]
fn insert_quoted_values() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, props HSTORE)",
        r#"INSERT INTO t VALUES (1, 'name=>"alice", city=>"new york"')"#,
    ]);
    let rows = select(&mut eng, "SELECT props FROM t");
    let Value::Hstore(pairs) = &rows[0][0] else {
        panic!()
    };
    assert_eq!(pairs.len(), 2);
    assert_eq!(pairs[0].1.as_deref(), Some("alice"));
    assert_eq!(pairs[1].1.as_deref(), Some("new york"));
}

#[test]
fn insert_null_value() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, props HSTORE)",
        "INSERT INTO t VALUES (1, 'a=>NULL, b=>2')",
    ]);
    let rows = select(&mut eng, "SELECT props FROM t");
    let Value::Hstore(pairs) = &rows[0][0] else {
        panic!()
    };
    assert_eq!(pairs[0], ("a".to_string(), None));
    assert_eq!(pairs[1], ("b".to_string(), Some("2".to_string())));
}

#[test]
fn insert_empty_hstore() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, props HSTORE)",
        "INSERT INTO t VALUES (1, '')",
    ]);
    let rows = select(&mut eng, "SELECT props FROM t");
    let Value::Hstore(pairs) = &rows[0][0] else {
        panic!()
    };
    assert!(pairs.is_empty());
}

#[test]
fn duplicate_keys_last_wins() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, props HSTORE)",
        "INSERT INTO t VALUES (1, 'a=>1, a=>2, b=>3')",
    ]);
    let rows = select(&mut eng, "SELECT props FROM t");
    let Value::Hstore(pairs) = &rows[0][0] else {
        panic!()
    };
    // After dedup: a=>2 (last wins) and b=>3.
    assert_eq!(pairs.len(), 2);
    let a = pairs.iter().find(|(k, _)| k == "a").unwrap();
    assert_eq!(a.1.as_deref(), Some("2"));
}

#[test]
fn hstore_null_column() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, props HSTORE)",
        "INSERT INTO t VALUES (1, NULL)",
    ]);
    let rows = select(&mut eng, "SELECT props FROM t");
    assert!(matches!(rows[0][0], Value::Null));
}

#[test]
fn hstore_column_survives_catalog_round_trip() {
    let mut eng = engine_with(&[
        "CREATE TABLE config (id INT NOT NULL, props HSTORE)",
        "INSERT INTO config VALUES (1, 'theme=>dark, lang=>en'), (2, 'k=>v')",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let mut eng2 = Engine::restore(cat);
    let rows = select(&mut eng2, "SELECT id, props FROM config ORDER BY id");
    assert_eq!(rows.len(), 2);
    let Value::Hstore(p1) = &rows[0][1] else {
        panic!()
    };
    let Value::Hstore(p2) = &rows[1][1] else {
        panic!()
    };
    assert_eq!(p1.len(), 2);
    assert_eq!(p2.len(), 1);
}

#[test]
fn hstore_display_canonical_quotes_keys_and_values() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, props HSTORE)",
        "INSERT INTO t VALUES (1, 'a=>1, b=>2')",
    ]);
    // Cast to text → PG canonical `"k"=>"v"` form.
    let r = eng.execute("SELECT props::text FROM t").unwrap();
    match r {
        QueryResult::Rows { rows, .. } => {
            let Value::Text(s) = &rows[0].values[0] else {
                panic!()
            };
            assert_eq!(s, r#""a"=>"1", "b"=>"2""#);
        }
        _ => panic!(),
    }
}

#[test]
fn hstore_malformed_input_is_error() {
    let mut eng = engine_with(&["CREATE TABLE t (id INT NOT NULL, props HSTORE)"]);
    let r = eng.execute("INSERT INTO t VALUES (1, 'a=>1, junk-no-arrow')");
    assert!(r.is_err(), "garbage hstore literal must error");
}
