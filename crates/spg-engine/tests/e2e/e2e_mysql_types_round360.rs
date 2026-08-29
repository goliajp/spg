//! read01 round 360 (MySQL differential) — the missing type spellings.
//!
//! Nine of MySQL's own column types did not exist: the whole BLOB family
//! (`BLOB`, `TINYBLOB`, `MEDIUMBLOB`, `LONGBLOB`), the sized TEXT family
//! (`TINYTEXT`, `MEDIUMTEXT`, `LONGTEXT`), `VARBINARY(n)` / `BINARY(n)`,
//! and MySQL's `FLOAT(m,d)` display form. `LONGTEXT` and `BLOB` are in
//! nearly every real MySQL schema, and `CREATE TABLE` failed outright —
//! `type "blob" does not exist` — so the table was never made.
//!
//! The sizes differ only in the maximum length MySQL enforces, which SPG
//! does not cap, so they collapse onto TEXT and BYTEA the way the unsized
//! spellings already did. The `(m,d)` digits are a display hint; the full
//! double is stored.
//!
//! Found while measuring M18 (information_schema type names) — a schema
//! that cannot be created is the more severe half of that, so it went
//! first.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
            .unwrap_or(Value::Null),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// Every spelling parses.
#[test]
fn the_mysql_type_spellings_exist() {
    let mut e = mysql();
    for (i, ty) in [
        "BLOB",
        "TINYBLOB",
        "MEDIUMBLOB",
        "LONGBLOB",
        "TINYTEXT",
        "MEDIUMTEXT",
        "LONGTEXT",
        "VARBINARY(10)",
        "BINARY(4)",
        "FLOAT(10,2)",
    ]
    .iter()
    .enumerate()
    {
        e.execute(&format!("CREATE TABLE t{i} (c {ty})"))
            .unwrap_or_else(|err| panic!("{ty}: {err}"));
    }
}

/// …and the columns really store and return data, rather than the
/// declaration merely being accepted.
#[test]
fn the_columns_round_trip() {
    let mut e = mysql();
    e.execute("CREATE TABLE d (t LONGTEXT, b BLOB, v VARBINARY(10), f FLOAT(10,2))")
        .unwrap();
    e.execute("INSERT INTO d VALUES ('hello', NULL, NULL, 1.5)")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT t FROM d"), Value::text("hello"));
    // v7.39.2 — a MySQL FLOAT is four bytes there (measured:
    // 3.14159265358979 reads back 3.14159), so `FLOAT(10,2)` lands in
    // SPG's own 4-byte type rather than being widened to a double.
    assert_eq!(one(&mut e, "SELECT f FROM d"), Value::Real(1.5));
    assert_eq!(one(&mut e, "SELECT b FROM d"), Value::Null);

    // The byte columns hold bytes — shown on a PG session, where the
    // bytea literal means what it says (a MySQL session reads the
    // backslash itself, and MySQL's own `X'…'` literal is a separate
    // divergence, recorded but not fixed here).
    let mut p = Engine::new();
    p.execute("CREATE TABLE d (b BLOB, v VARBINARY(10))")
        .unwrap();
    p.execute("INSERT INTO d VALUES ('\\x414243'::bytea, '\\x00ff'::bytea)")
        .unwrap();
    assert_eq!(one(&mut p, "SELECT length(b) FROM d"), Value::Int(3));
    assert_eq!(one(&mut p, "SELECT length(v) FROM d"), Value::Int(2));
}

/// The sized spellings are the same type as the unsized ones.
#[test]
fn the_sizes_collapse_onto_one_type() {
    let mut e = mysql();
    e.execute("CREATE TABLE s (a TEXT, b LONGTEXT, c BYTEA, d BLOB)")
        .unwrap();
    let types = |e: &mut Engine, col: &str| {
        one(
            e,
            &format!(
                "SELECT data_type FROM information_schema.columns \
                   WHERE table_name = 's' AND column_name = '{col}'"
            ),
        )
    };
    assert_eq!(types(&mut e, "a"), types(&mut e, "b"), "TEXT vs LONGTEXT");
    assert_eq!(types(&mut e, "c"), types(&mut e, "d"), "BYTEA vs BLOB");
}

/// PG's own `FLOAT(p)` still means precision, and is not read as a
/// display form.
#[test]
fn pg_float_precision_is_untouched() {
    let mut p = Engine::new();
    p.execute("CREATE TABLE f1 (a FLOAT(24), b FLOAT(53))")
        .unwrap();
    assert!(
        p.execute("CREATE TABLE f2 (a FLOAT(10,2))").is_err(),
        "PG has no (m,d) form"
    );
    assert!(
        p.execute("CREATE TABLE f3 (a FLOAT(54))").is_err(),
        "…and still rejects an out-of-range precision"
    );
}
