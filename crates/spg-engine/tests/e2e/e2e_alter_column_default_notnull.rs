//! v7.37.18 (18.1 + 18.2) — proper engine handlers for
//! `ALTER TABLE … ALTER COLUMN col {SET|DROP} {DEFAULT expr | NOT NULL}`.
//! Previously these were parsed as no-ops; now they edit the
//! column's `default` / `runtime_default` / `nullable` fields.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter()
        .next()
        .expect("one row")
        .values
        .into_iter()
        .next()
        .expect("one col")
}

#[test]
fn alter_column_set_default_literal_takes_effect_on_insert() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, status TEXT)").unwrap();
    e.execute("ALTER TABLE t ALTER COLUMN status SET DEFAULT 'pending'")
        .unwrap();
    e.execute("INSERT INTO t (id) VALUES (1)").unwrap();
    assert_eq!(
        one(&mut e, "SELECT status FROM t WHERE id = 1"),
        Value::text("pending")
    );
}

#[test]
fn alter_column_drop_default_clears_runtime_and_literal() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, status TEXT DEFAULT 'pending')")
        .unwrap();
    e.execute("ALTER TABLE t ALTER COLUMN status DROP DEFAULT")
        .unwrap();
    // After DROP DEFAULT, omitted column inserts as NULL.
    e.execute("INSERT INTO t (id) VALUES (1)").unwrap();
    assert!(matches!(
        one(&mut e, "SELECT status FROM t WHERE id = 1"),
        Value::Null
    ));
}

#[test]
fn alter_column_set_not_null_rejects_existing_nulls() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, status TEXT)").unwrap();
    e.execute("INSERT INTO t VALUES (1, NULL)").unwrap();
    let err = e
        .execute("ALTER TABLE t ALTER COLUMN status SET NOT NULL")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("NULL") || msg.contains("status"),
        "expected NULL-rejection, got {msg}"
    );
}

#[test]
fn alter_column_set_not_null_accepts_clean_data_then_blocks_inserts() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, status TEXT)").unwrap();
    e.execute("INSERT INTO t VALUES (1, 'ok')").unwrap();
    e.execute("ALTER TABLE t ALTER COLUMN status SET NOT NULL")
        .unwrap();
    let err = e.execute("INSERT INTO t (id) VALUES (2)").unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("NULL") || msg.contains("status") || msg.contains("not"),
        "expected NOT NULL violation: {msg}"
    );
}

#[test]
fn alter_column_drop_not_null_reallows_nulls() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, status TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'ok')").unwrap();
    // Drop NOT NULL → omitted column now allowed.
    e.execute("ALTER TABLE t ALTER COLUMN status DROP NOT NULL")
        .unwrap();
    e.execute("INSERT INTO t (id) VALUES (2)").unwrap();
    assert!(matches!(
        one(&mut e, "SELECT status FROM t WHERE id = 2"),
        Value::Null
    ));
}

#[test]
fn alter_column_set_default_with_pg_dump_nextval_still_takes_auto_increment_path() {
    // Backwards-compat with the v7.22 BIGSERIAL pg_dump shape.
    // `SET DEFAULT nextval('seq')` should lower to
    // SetColumnAutoIncrement, not AlterColumnSetDefault.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("CREATE SEQUENCE t_id_seq").unwrap();
    e.execute("ALTER TABLE t ALTER COLUMN id SET DEFAULT nextval('t_id_seq')")
        .unwrap();
    // No regression — ALTER TABLE parsed + applied without error.
    // The lowering itself (SetColumnAutoIncrement vs
    // AlterColumnSetDefault) is exercised by tests in
    // e2e_alter_set_default_nextval.
}
