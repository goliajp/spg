//! read01 round 345 (MySQL differential, M5) — `DECIMAL` is a type name.
//!
//! Only `NUMERIC` parsed as a column type, so `CREATE TABLE t (a
//! DECIMAL(10,2))` — how nearly every money column is spelled, in either
//! dialect — was `syntax error at or near "("` and the table was never
//! created at all. That gates a whole schema, which is why it leads the
//! MySQL checklist.
//!
//! Measured: PG 18.4 accepts `DECIMAL(10,2)` and `DEC(5,1)`, reporting
//! both as `numeric`; MariaDB 11 accepts those plus `FIXED(4,2)`,
//! reporting `decimal`. PG rejects `FIXED` — `type "fixed" does not
//! exist` — so SPG takes that spelling only in the MySQL dialect rather
//! than being loosely permissive in both.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn first(e: &mut Engine, sql: &str) -> Vec<Value<'static>> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| r.values.iter().cloned().map(Value::into_owned).collect())
            .unwrap_or_default(),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// The standard's own spellings, in the default (PG) dialect.
#[test]
fn decimal_and_dec_are_numeric() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d1 (a DECIMAL(10,2), b DEC(5,1), c NUMERIC(3))")
        .expect("DECIMAL and DEC are PG type names too");
    e.execute("CREATE TABLE d2 (a DECIMAL)").unwrap();
    e.execute("CREATE TABLE d3 (a DECIMAL(10))").unwrap();

    // They ARE numeric — the scale is applied on the way in.
    e.execute("INSERT INTO d1 VALUES (1.005, 2.25, 7)").unwrap();
    let row = first(&mut e, "SELECT a, b, c FROM d1");
    assert_eq!(row[0], Value::numeric(101, 2), "1.005 at scale 2");
    assert_eq!(row[1], Value::numeric(23, 1), "2.25 at scale 1");
    assert_eq!(row[2], Value::numeric(7, 0));
}

/// …and they report as one type, with the declared precision and scale.
#[test]
fn the_catalog_reports_them_as_numeric() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d1 (a DECIMAL(10,2), b DEC(5,1), c NUMERIC(3))")
        .unwrap();
    let r = first(
        &mut e,
        "SELECT data_type, numeric_precision, numeric_scale \
           FROM information_schema.columns \
          WHERE table_name = 'd1' AND column_name = 'a'",
    );
    assert_eq!(r[0], Value::text("numeric"));
    assert_eq!(r[1], Value::Int(10));
    assert_eq!(r[2], Value::Int(2));
}

/// `FIXED` is MySQL's alias alone — PG says `type "fixed" does not exist`.
#[test]
fn fixed_is_mysql_only() {
    let mut e = Engine::new();
    assert!(
        e.execute("CREATE TABLE f1 (a FIXED(4,2))").is_err(),
        "the PG dialect has no FIXED"
    );
    let mut m = Engine::new();
    m.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    m.execute("CREATE TABLE f1 (a FIXED(4,2))")
        .expect("MariaDB spells NUMERIC this way too");
    m.execute("INSERT INTO f1 VALUES (1.005)").unwrap();
    assert_eq!(first(&mut m, "SELECT a FROM f1")[0], Value::numeric(101, 2));
}

/// The cast spellings already worked; they must keep working.
#[test]
fn the_cast_forms_are_unchanged() {
    let mut e = Engine::new();
    assert_eq!(
        first(&mut e, "SELECT 1.5::decimal(4,1)")[0],
        Value::numeric(15, 1)
    );
    assert_eq!(
        first(&mut e, "SELECT CAST(1.5 AS DECIMAL(4,1))")[0],
        Value::numeric(15, 1)
    );
}
