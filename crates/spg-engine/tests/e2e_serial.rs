//! v7.9.6 — SERIAL / BIGSERIAL keyword aliases. mailrs migration
//! blocker #5.

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

#[test]
fn bigserial_creates_bigint_not_null_auto_increment() {
    let mut eng = engine_with(&["CREATE TABLE u (id BIGSERIAL, name TEXT)"]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let id_col = &cat.get("u").unwrap().schema().columns[0];
    assert!(matches!(id_col.ty, DataType::BigInt));
    assert!(!id_col.nullable, "BIGSERIAL must imply NOT NULL");
    assert!(id_col.auto_increment, "BIGSERIAL must imply AUTO_INCREMENT");
    // Sanity insert + RETURNING id.
    let r = eng
        .execute("INSERT INTO u (name) VALUES ('alice') RETURNING id")
        .unwrap();
    let rows = match r {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    assert_eq!(rows.len(), 1);
    let Value::BigInt(_) = rows[0].values[0] else {
        panic!("RETURNING id must yield BigInt for BIGSERIAL")
    };
}

#[test]
fn serial_creates_int_not_null_auto_increment() {
    let mut eng = engine_with(&["CREATE TABLE t (id SERIAL, name TEXT)"]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let id_col = &cat.get("t").unwrap().schema().columns[0];
    assert!(matches!(id_col.ty, DataType::Int));
    assert!(!id_col.nullable);
    assert!(id_col.auto_increment);
}

#[test]
fn smallserial_creates_smallint_not_null_auto_increment() {
    let mut eng = engine_with(&["CREATE TABLE t (id SMALLSERIAL, name TEXT)"]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let id_col = &cat.get("t").unwrap().schema().columns[0];
    assert!(matches!(id_col.ty, DataType::SmallInt));
    assert!(!id_col.nullable);
    assert!(id_col.auto_increment);
}

#[test]
fn bigserial_aliases_serial8() {
    let mut eng = engine_with(&["CREATE TABLE t (id SERIAL8, name TEXT)"]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    assert!(matches!(
        cat.get("t").unwrap().schema().columns[0].ty,
        DataType::BigInt
    ));
}

#[test]
fn serial_with_explicit_not_null_is_rejected() {
    // BIGSERIAL already implies NOT NULL — repeating it should
    // surface as "NOT NULL specified twice" so the user doesn't
    // think they're "reinforcing" something.
    let mut eng = Engine::new();
    let r = eng.execute("CREATE TABLE t (id BIGSERIAL NOT NULL)");
    assert!(
        r.is_err(),
        "expected parse error for redundant NOT NULL on BIGSERIAL"
    );
}

#[test]
fn pg_dump_style_serial_with_returning() {
    // mailrs IMAP UID alloc shape, but using PG-source SERIAL.
    let mut eng = engine_with(&["CREATE TABLE messages (
            id BIGSERIAL,
            mailbox_id INT NOT NULL,
            subject TEXT
        )"]);
    let r = eng
        .execute("INSERT INTO messages (mailbox_id, subject) VALUES (1, 'hello') RETURNING id")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    let Value::BigInt(id) = rows[0].values[0] else {
        panic!()
    };
    assert!(id > 0);
}
