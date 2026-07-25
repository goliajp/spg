//! read01 round 424 (MySQL differential) — a temporal COLUMN's declared
//! fractional-seconds precision.
//!
//! `DATETIME(3)` / `TIME(2)` / `TIMESTAMP(6)` declare how many fractional
//! digits the column holds; MySQL TRUNCATES toward zero to that many on
//! write, and a BARE `DATETIME` / `TIME` / `TIMESTAMP` has precision ZERO —
//! it drops the fraction entirely. SPG parsed the `(N)` and threw it away,
//! so every temporal column kept full microseconds: a MySQL client that
//! stored `00:00:00.256789` into a `DATETIME` read back a DIFFERENT INSTANT
//! than MariaDB would give it.
//!
//! `ColumnSchema.mysql_fsp` now carries the precision (FILE_VERSION 82
//! sparse appendix, the shape round 386 used for mysql_int_width; catalogs
//! at 81 and below deserialise as None and keep PG behaviour). The DDL
//! copies it from the parsed ColumnDef, and the write path truncates next
//! to the existing range check.
//!
//! SCOPE — the RENDER half is not done: MariaDB pads a `DATETIME(3)` to
//! exactly three digits (`.250`, and `.000` for a whole second) where SPG
//! trims trailing zeros. The stored INSTANT now matches; only the text form
//! of a padded value differs. Padding needs the output ColumnSchema's fsp to
//! reach each wire renderer, which is a round of its own; today's behaviour
//! is pinned in `render_padding_is_not_yet_modelled`.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Null => "NULL".to_string(),
            v => spg_engine::eval::value_to_text(v),
        },
        other => panic!("{other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = mysql();
    e.execute(
        "CREATE TABLE c(d0 DATETIME, d1 DATETIME(1), d3 DATETIME(3), \
         d6 DATETIME(6), t0 TIME, t2 TIME(2))",
    )
    .unwrap();
    e.execute(
        "INSERT INTO c VALUES('2020-01-01 00:00:00.256789','2020-01-01 00:00:00.256789',\
         '2020-01-01 00:00:00.256789','2020-01-01 00:00:00.256789','10:00:00.756','10:00:00.756')",
    )
    .unwrap();
    e
}

/// A bare DATETIME / TIME column has precision 0 and drops the fraction.
#[test]
fn bare_temporal_column_drops_fraction() {
    let mut e = seeded();
    assert_eq!(one(&mut e, "SELECT d0 FROM c"), "2020-01-01 00:00:00");
    assert_eq!(one(&mut e, "SELECT t0 FROM c"), "10:00:00");
}

/// A declared precision truncates toward zero — it does not round.
#[test]
fn declared_precision_truncates() {
    let mut e = seeded();
    // .256789 -> .2 (rounding would give .3).
    assert_eq!(one(&mut e, "SELECT d1 FROM c"), "2020-01-01 00:00:00.2");
    assert_eq!(one(&mut e, "SELECT d3 FROM c"), "2020-01-01 00:00:00.256");
    // .756 -> .75 at TIME(2).
    assert_eq!(one(&mut e, "SELECT t2 FROM c"), "10:00:00.75");
}

/// Full precision keeps every digit.
#[test]
fn full_precision_keeps_microseconds() {
    let mut e = seeded();
    assert_eq!(one(&mut e, "SELECT d6 FROM c"), "2020-01-01 00:00:00.256789");
}

/// UPDATE goes through the same truncation as INSERT.
#[test]
fn update_truncates_too() {
    let mut e = mysql();
    e.execute("CREATE TABLE u(d1 DATETIME(1))").unwrap();
    e.execute("INSERT INTO u VALUES('2020-01-01 00:00:00')").unwrap();
    e.execute("UPDATE u SET d1 = '2020-01-01 00:00:00.256789'").unwrap();
    assert_eq!(one(&mut e, "SELECT d1 FROM u"), "2020-01-01 00:00:00.2");
}

/// The truncated value survives a catalog round-trip (FILE_VERSION 82
/// appendix), and a later write into the same column truncates again.
#[test]
fn precision_survives_and_keeps_applying() {
    let mut e = seeded();
    e.execute("INSERT INTO c(d1) VALUES('2020-06-01 12:00:00.999999')")
        .unwrap();
    assert_eq!(
        one(&mut e, "SELECT d1 FROM c WHERE d1 > '2020-02-01'"),
        "2020-06-01 12:00:00.9"
    );
}

/// Round 425 closed the render half: the RESULT schema now carries the
/// precision and the wire encoder pads to it. `value_to_text` (the
/// dialect-blind renderer this file uses) still trims, which is correct for
/// it — see `e2e_mysql_fsp_render_round425` for the padded contract.
#[test]
fn stored_instant_is_exact_regardless_of_renderer() {
    let mut e = mysql();
    e.execute("CREATE TABLE p(d3 DATETIME(3))").unwrap();
    e.execute("INSERT INTO p VALUES('2020-01-01 00:00:00.25')").unwrap();
    assert_eq!(one(&mut e, "SELECT d3 FROM p"), "2020-01-01 00:00:00.25");
    // The same value through the fsp-aware renderer is MariaDB's text.
    assert_eq!(
        match e.execute("SELECT d3 FROM p").unwrap() {
            QueryResult::Rows { columns, rows } =>
                spg_engine::eval::value_to_text_with_fsp(&rows[0].values[0], columns[0].mysql_fsp),
            other => panic!("{other:?}"),
        },
        "2020-01-01 00:00:00.250"
    );
}

/// A PostgreSQL session's temporal columns keep full microseconds — the
/// precision is captured only under the MySQL dialect.
#[test]
fn postgres_columns_keep_microseconds() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c(a TIMESTAMP, b TIMESTAMP(1), t TIME)")
        .unwrap();
    e.execute(
        "INSERT INTO c VALUES('2020-01-01 00:00:00.256789','2020-01-01 00:00:00.256789','10:00:00.756')",
    )
    .unwrap();
    assert_eq!(one(&mut e, "SELECT a FROM c"), "2020-01-01 00:00:00.256789");
    assert_eq!(one(&mut e, "SELECT b FROM c"), "2020-01-01 00:00:00.256789");
    assert_eq!(one(&mut e, "SELECT t FROM c"), "10:00:00.756");
}
