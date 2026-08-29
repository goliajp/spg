//! read01 round 389 (MySQL type-fidelity epic — P4a) — SMALLINT / INT
//! UNSIGNED hold their full range, and the integer types render with their
//! MySQL name / width / ` unsigned` suffix.
//!
//! An UNSIGNED integer column holds the full unsigned range; SPG rejected
//! the upper half because storage was signed (`INSERT 65535 INTO SMALLINT
//! UNSIGNED` errored). P4a widens the "real" SMALLINT UNSIGNED to i32 and
//! INT UNSIGNED to i64 and enforces the real bounds. It also fixes the
//! rendering that dropped the narrow name + ` unsigned` (a TINYINT rendered
//! "smallint(6)"): column_type is `smallint(5) unsigned`, data_type the
//! bare `smallint`. A PostgreSQL session is unchanged.
//!
//! Every value / rendering is copied from a MariaDB 11 run.
//!
//! v7.39.2 — RE-CALIBRATED against MySQL 9.7.2, which is the engine SPG
//! advertises itself as (`MYSQL_SERVER_VERSION` feeds the handshake,
//! `@@version` and `VERSION()`). These expectations were a MariaDB run,
//! and MySQL dropped the integer display width in 8.0.19: `int`, not
//! `int(11)`; `int unsigned`, not `int(10) unsigned`. `tinyint(1)`
//! survives there because it is how BOOLEAN is spelled.

use spg_engine::{Engine, QueryResult};
use spg_storage::{MysqlIntWidth, Value};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn one_row(e: &mut Engine, sql: &str) -> Vec<Value<'static>> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows[0].values.clone(),
        other => panic!("`{sql}`: {other:?}"),
    }
}

fn ok(e: &mut Engine, sql: &str) {
    assert!(
        matches!(e.execute(sql), Ok(QueryResult::CommandOk { .. })),
        "expected `{sql}` to succeed"
    );
}

fn out_of_range(e: &mut Engine, sql: &str) {
    assert!(
        e.execute(sql)
            .err()
            .is_some_and(|er| er.to_string().contains("Out of range value for column")),
        "expected `{sql}` to be out-of-range"
    );
}

/// The full unsigned range now inserts and reads back.
#[test]
fn unsigned_upper_range_stored() {
    let mut e = mysql();
    e.execute("CREATE TABLE w(c SMALLINT UNSIGNED, e INT UNSIGNED)")
        .unwrap();
    ok(&mut e, "INSERT INTO w(c) VALUES(65535)");
    ok(&mut e, "INSERT INTO w(e) VALUES(4294967295)");
    assert_eq!(
        one_row(&mut e, "SELECT c FROM w WHERE c = 65535"),
        vec![Value::Int(65535)]
    );
    assert_eq!(
        one_row(&mut e, "SELECT e FROM w WHERE e = 4294967295"),
        vec![Value::BigInt(4_294_967_295)]
    );
}

/// Past the bound (or negative) is still rejected.
#[test]
fn unsigned_overflow_rejected() {
    let mut e = mysql();
    e.execute("CREATE TABLE w(c SMALLINT UNSIGNED, e INT UNSIGNED)")
        .unwrap();
    out_of_range(&mut e, "INSERT INTO w(c) VALUES(65536)");
    out_of_range(&mut e, "INSERT INTO w(c) VALUES(-1)");
    out_of_range(&mut e, "INSERT INTO w(e) VALUES(4294967296)");
    out_of_range(&mut e, "INSERT INTO w(e) VALUES(-1)");
}

/// information_schema column_type / data_type match MariaDB.
#[test]
fn info_schema_rendering() {
    let mut e = mysql();
    e.execute(
        "CREATE TABLE w(a TINYINT, b TINYINT UNSIGNED, c SMALLINT UNSIGNED, \
         d MEDIUMINT UNSIGNED, f INT UNSIGNED, g SMALLINT, h INT)",
    )
    .unwrap();
    let rows = match e
        .execute(
            "SELECT column_name, column_type, data_type FROM information_schema.columns \
             WHERE table_name = 'w' ORDER BY ordinal_position",
        )
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    };
    let got: Vec<(String, String, String)> = rows
        .iter()
        .map(|r| {
            let s = |v: &Value| match v {
                Value::Text(t) => t.to_string(),
                o => panic!("{o:?}"),
            };
            (s(&r.values[0]), s(&r.values[1]), s(&r.values[2]))
        })
        .collect();
    // Measured on MySQL 9.7.2.
    let want = [
        ("a", "tinyint", "tinyint"),
        ("b", "tinyint unsigned", "tinyint"),
        ("c", "smallint unsigned", "smallint"),
        ("d", "mediumint unsigned", "mediumint"),
        ("f", "int unsigned", "int"),
        ("g", "smallint", "smallint"),
        ("h", "int", "int"),
    ];
    for (i, (name, ct, dt)) in want.iter().enumerate() {
        assert_eq!(got[i].0, *name);
        assert_eq!(got[i].1, *ct, "column_type of {name}");
        assert_eq!(got[i].2, *dt, "data_type of {name}");
    }
}

/// SHOW CREATE TABLE prints the same MySQL spellings.
#[test]
fn show_create_rendering() {
    let mut e = mysql();
    e.execute("CREATE TABLE w(c SMALLINT UNSIGNED, i INT UNSIGNED, t TINYINT)")
        .unwrap();
    let text = match e.execute("SHOW CREATE TABLE w").unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[1] {
            Value::Text(s) => s.to_string(),
            o => panic!("{o:?}"),
        },
        other => panic!("{other:?}"),
    };
    assert!(text.contains("smallint unsigned"), "{text}");
    assert!(text.contains("int unsigned"), "{text}");
    assert!(text.contains("tinyint"), "{text}");
    // And no integer display width anywhere in the statement. (The
    // first draft banned every `(`, which the statement's own opening
    // paren trips.)
    for w in ["int(", "tinyint(4)", "smallint(", "bigint(", "mediumint("] {
        assert!(
            !text.contains(w),
            "a display width crept back ({w}): {text}"
        );
    }
}

/// The widened storage type + annotation survive a snapshot round-trip.
#[test]
fn survives_snapshot() {
    let mut e = mysql();
    e.execute("CREATE TABLE w(c SMALLINT UNSIGNED, i INT UNSIGNED)")
        .unwrap();
    ok(&mut e, "INSERT INTO w VALUES(65535, 4294967295)");
    let snap = e.catalog().serialize();
    let reloaded = spg_storage::Catalog::deserialize(&snap).expect("roundtrip");
    let e2 = Engine::restore(reloaded);
    let t = e2.catalog().get("w").unwrap();
    let cols = &t.schema().columns;
    assert_eq!(cols[0].mysql_int_width, Some(MysqlIntWidth::Small));
    assert_eq!(cols[0].ty, spg_storage::DataType::Int);
    assert_eq!(cols[1].mysql_int_width, Some(MysqlIntWidth::Int));
    assert_eq!(cols[1].ty, spg_storage::DataType::BigInt);
}

/// A PostgreSQL session is unchanged (no widening, no annotation).
#[test]
fn postgres_unaffected() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE w(c SMALLINT)").unwrap();
    let t = e.catalog().get("w").unwrap();
    assert_eq!(t.schema().columns[0].mysql_int_width, None);
    assert_eq!(t.schema().columns[0].ty, spg_storage::DataType::SmallInt);
}
